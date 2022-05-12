#![feature(let_else)]
mod lsp;

use lsp::RequestId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::{io, sync::Arc};
use tokio::fs;
use tokio::io::BufReader;

use tokio::net::{tcp::OwnedWriteHalf, TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};

pub struct MainContext {
    servers: Mutex<Vec<Arc<Mutex<Server>>>>,
}

pub struct Server {
    stdin: tokio::process::ChildStdin,
    child: tokio::process::Child,
    root: PathBuf,
    clients: Vec<Client>,
    request_id_to_original: HashMap<RequestId, ClientRequestId>,
    initialize_response: Option<lsp::Message>,
    cancel_shutdown: Option<oneshot::Sender<()>>,
}

pub struct ClientRequestId {
    client_id: usize,
    request_id: RequestId,
}

impl Server {
    pub fn new(root: PathBuf) -> io::Result<Arc<Mutex<Self>>> {
        let mut process = tokio::process::Command::new("rust-analyzer");
        process.stdin(std::process::Stdio::piped());
        process.stdout(std::process::Stdio::piped());
        let mut child = process.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        // TODO: consider using a buffered writer
        let this = Arc::new(Mutex::new(Self {
            stdin,
            root,
            child,
            clients: Vec::new(),
            request_id_to_original: HashMap::new(),
            initialize_response: None,
            cancel_shutdown: None,
        }));

        tokio::spawn(Self::handle_server_output(this.clone(), stdout));

        Ok(this)
    }

    pub fn attach_client(&mut self, client: Client) {
        self.clients.push(client);
        if let Some(cancel_shutdown) = self.cancel_shutdown.take() {
            cancel_shutdown.send(()).unwrap();
        }
    }

    pub async fn handle_server_output(
        this: Arc<Mutex<Self>>,
        stdout: tokio::process::ChildStdout,
    ) -> io::Result<()> {
        let mut reader = BufReader::new(stdout);
        while let Some(line) = lsp::Message::read(&mut reader).await? {
            this.lock().await.handle_server_message(line).await.ok();
        }
        println!("server exited");
        Ok(())
    }

    pub async fn handle_server_message(&mut self, mut message: lsp::Message) -> io::Result<()> {
        let client_id = match &mut message {
            lsp::Message::Request(_) | lsp::Message::Notification(_) => None,
            lsp::Message::Response(res) => {
                let orig_req = self.request_id_to_original.remove(&res.id);
                if let Some(orig_req) = orig_req {
                    res.id = dbg!(orig_req.request_id);
                    Some(orig_req.client_id)
                } else {
                    None
                }
            }
        };

        if let Some(client_id) = client_id {
            for client in &mut self.clients {
                if client.id == client_id {
                    client.send_message(&message).await.ok();
                    break;
                }
            }
        } else {
            for client in &mut self.clients {
                client.send_message(&message).await.ok();
            }
        }
        if self.initialize_response.is_none() {
            self.initialize_response = Some(message);
        }

        Ok(())
    }

    pub async fn handle_disconnect(
        this: Arc<Mutex<Self>>,
        main_ctx: Arc<MainContext>,
        client_id: usize,
    ) {
        let mut this_lock = this.lock().await;
        this_lock.clients.retain(|client| client.id != client_id);
        this_lock
            .request_id_to_original
            .retain(|_, v| v.client_id != client_id);

        if this_lock.clients.is_empty() {
            println!("no more clients, shutting down");
            // schedule shutdown
            let (tx, rx) = tokio::sync::oneshot::channel();
            this_lock.cancel_shutdown = Some(tx);
            drop(this_lock);
            tokio::spawn(async move {
                let sleep = tokio::time::sleep(std::time::Duration::from_secs(20 * 60));
                tokio::select! {
                    _ = sleep => {
                        println!("shutting down");
                        this.lock().await.child.kill().await.ok();
                        main_ctx.servers.lock().await.retain(|s| !Arc::ptr_eq(s, &this));
                    },
                    _ = rx => {
                        println!("shutdown cancelled");
                    }
                };
            });
        }
    }

    pub fn get_next_id(&mut self, client_id: usize) -> RequestId {
        static NEXT_ID: AtomicI32 = AtomicI32::new(0);
        NEXT_ID.fetch_add(1, Ordering::SeqCst).into()
    }

    pub async fn handle_client_message(
        &mut self,
        client_id: usize,
        mut message: lsp::Message,
    ) -> io::Result<()> {
        println!("client #{client_id}: {message:?}");
        if let lsp::Message::Request(req) = &mut message {
            let request_id = self.get_next_id(client_id);
            self.request_id_to_original.insert(
                request_id.clone(),
                ClientRequestId {
                    client_id,
                    request_id: req.id.clone(),
                },
            );
            req.id = request_id;
        }
        message.write(&mut self.stdin).await
    }
}

#[derive(Debug)]
pub struct Client {
    id: usize,
    write: OwnedWriteHalf,
}

impl Client {
    pub async fn send_message(&mut self, message: &lsp::Message) -> io::Result<()> {
        message.write(&mut self.write).await
    }
}

impl MainContext {
    async fn new() -> Arc<Self> {
        Arc::new(Self {
            servers: Mutex::new(Vec::new()),
        })
    }

    async fn tcp_server(self: Arc<Self>) -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:6969").await?;
        loop {
            let (socket, _) = listener.accept().await?;
            tokio::spawn(self.clone().handle_client(socket));
        }
    }

    async fn find_or_make_server(self: Arc<Self>, root: &Path) -> io::Result<Arc<Mutex<Server>>> {
        let root = fs::canonicalize(root).await?;
        let mut servers = self.servers.lock().await;
        for server in &*servers {
            if server.lock().await.root == root {
                return Ok(server.clone());
            }
        }
        let server = Server::new(root)?;
        servers.push(server.clone());
        Ok(server)
    }

    async fn handle_client(self: Arc<Self>, socket: TcpStream) -> io::Result<()> {
        static CLIENT_ID: AtomicUsize = AtomicUsize::new(0);
        let client_id = CLIENT_ID.fetch_add(1, Ordering::SeqCst);
        println!("connect client #{client_id}");

        let (read, write) = socket.into_split();

        let mut read = BufReader::new(read);

        let Some(init) = lsp::Message::read(&mut read).await? else {
            println!("no init message");
            return Ok(());
        };

        let Some(root) = lsp::get_root_path(&init) else {
            println!("no root path");
            return Ok(());
        };

        println!("intialized root: {}", root.display());
        let server = self.find_or_make_server(&root).await?;
        let mut client = Client {
            id: client_id,
            write,
        };
        {
            let mut server = server.lock().await;
            if let Some(init) = &server.initialize_response {
                client.send_message(init).await.ok();
                server.attach_client(client);
            } else {
                server.attach_client(client);
                server.handle_client_message(client_id, init).await.ok();
            }
        }
        while let Some(message) = lsp::Message::read(&mut read).await? {
            match message {
                lsp::Message::Request(req) if req.method == "shutdown" => {
                    println!("client #{client_id}: shutdown request");
                    let mut server = server.lock().await;
                    let msg = lsp::Message::Response(lsp::Response {
                        id: req.id,
                        result: Some(serde_json::json!(null)),
                        error: None,
                    });
                    let client = server
                        .clients
                        .iter_mut()
                        .find(|client| client.id == client_id)
                        .unwrap();
                    client.send_message(&msg).await.ok();
                    continue;
                }
                lsp::Message::Notification(notif) if notif.method == "exit" => {
                    println!("client #{client_id}: exited");
                    Server::handle_disconnect(server, self.clone(), client_id).await;
                    drop(read);
                    break;
                }
                _ => {}
            }
            let mut server = server.lock().await;
            server.handle_client_message(client_id, message).await?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let main_context = MainContext::new().await;
    main_context.tcp_server().await
}
