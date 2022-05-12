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
use tokio::sync::Mutex;

pub struct MainContext {
    servers: Mutex<Vec<Arc<Mutex<Server>>>>,
}

pub struct Server {
    stdin: tokio::process::ChildStdin,
    root: PathBuf,
    clients: Vec<Client>,
    request_id_to_original: HashMap<RequestId, ClientRequestId>,
    initialize_response: Option<lsp::Message>,
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
        // TODO: consider using a buffered writer
        let this = Arc::new(Mutex::new(Self {
            stdin,
            root,
            clients: Vec::new(),
            request_id_to_original: HashMap::new(),
            initialize_response: None,
        }));

        tokio::spawn(Self::handle_server_output(this.clone(), child.stdout.take().unwrap()));

        Ok(this)
    }

    pub fn attach_client(&mut self, client: Client) {
        self.clients.push(client);
    }

    pub async fn handle_server_output(
        this: Arc<Mutex<Self>>,
        stdout: tokio::process::ChildStdout,
    ) -> io::Result<()> {
        let mut reader = BufReader::new(stdout);
        while let Some(line) = lsp::Message::read(&mut reader).await? {
            println!("{line:?}");
            this.lock().await.handle_server_message(line).await.ok();
        }
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

    pub fn get_next_id(&mut self, client_id: usize) -> RequestId {
        static NEXT_ID: AtomicI32 = AtomicI32::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let request_id = RequestId::from(id);
        self.request_id_to_original.insert(
            request_id.clone(),
            ClientRequestId {
                client_id,
                request_id: request_id.clone(),
            },
        );
        request_id
    }

    pub async fn handle_client_message(
        &mut self,
        client_id: usize,
        mut message: lsp::Message,
    ) -> io::Result<()> {
        println!("client msg: {message:?}");
        if let lsp::Message::Request(req) = &mut message {
            let request_id = self.get_next_id(client_id);
            req.id = request_id;
        }
        message.write(&mut self.stdin).await
    }
}

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
