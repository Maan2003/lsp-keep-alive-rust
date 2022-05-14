#![feature(let_else)]
mod lsp;

use lsp::RequestId;
use std::collections::HashMap;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::{io, sync::Arc};
use tokio::fs;
use tokio::io::BufReader;
use tracing::{error, info, instrument, Value};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

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

impl Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("root", &self.root).finish()
    }
}

pub struct ClientRequestId {
    client_id: usize,
    request_id: RequestId,
}

impl Server {
    #[instrument(name = "new server", level = "info")]
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

    #[instrument(
        level = "info",
        skip_all,
        fields(
            server_root = ?self.root,
            client_id = client.id,
        )
    )]
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
        let this = this.lock().await;
        info!(server_root = ?this.root, "lsp server exited");
        Ok(())
    }

    #[instrument(level = "trace", skip(self))]
    pub async fn handle_server_message(&mut self, mut message: lsp::Message) -> io::Result<()> {
        let client_id = match &mut message {
            lsp::Message::Request(_) | lsp::Message::Notification(_) => None,
            lsp::Message::Response(res) => {
                let orig_req = self.request_id_to_original.remove(&res.id);
                if let Some(orig_req) = orig_req {
                    res.id = orig_req.request_id;
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

    #[instrument(level = "info", skip(this, main_ctx))]
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
            info!(server_root = ?this_lock.root, "no clients left, scheduling shutting down");
            let (tx, rx) = tokio::sync::oneshot::channel();
            this_lock.cancel_shutdown = Some(tx);
            drop(this_lock);
            tokio::spawn(async move {
                let sleep = tokio::time::sleep(std::time::Duration::from_secs(20 * 60));
                tokio::select! {
                    _ = sleep => {
                        {
                            let mut this = this.lock().await;
                            info!(server_root = ?this.root, "shutting down server due to inactivity");
                            this.child.kill().await.ok();
                        }
                        main_ctx.servers.lock().await.retain(|s| !Arc::ptr_eq(s, &this));
                    },
                    _ = rx => {
                        let this = this.lock().await;
                        info!(server_root = ?this.root,"shutting down cancelled");
                    }
                };
            });
        }
    }

    pub fn get_next_id(&mut self, client_id: usize) -> RequestId {
        static NEXT_ID: AtomicI32 = AtomicI32::new(0);
        NEXT_ID.fetch_add(1, Ordering::SeqCst).into()
    }

    #[instrument(level = "trace", skip(self, client_id))]
    pub async fn handle_client_message(
        &mut self,
        client_id: usize,
        mut message: lsp::Message,
    ) -> io::Result<()> {
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

pub struct Client {
    id: usize,
    write: OwnedWriteHalf,
}

impl Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Client").field(&self.id).finish()
    }
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
        let mut client_id = 0;
        loop {
            let (socket, _) = listener.accept().await?;
            tokio::spawn(self.clone().handle_client(client_id, socket));
            client_id += 1;
        }
    }

    async fn find_or_make_server(&self, root: &Path) -> io::Result<Arc<Mutex<Server>>> {
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

    #[instrument(level = "info", skip(self, socket))]
    async fn handle_client(self: Arc<Self>, client_id: usize, socket: TcpStream) -> io::Result<()> {
        let (read, write) = socket.into_split();

        let mut read = BufReader::new(read);

        let Some(init) = lsp::Message::read(&mut read).await? else {
            error!("client disconnected before initialization");
            return Ok(());
        };

        let Some(root) = lsp::get_root_path(&init) else {
            error!("client sent invalid initialization");
            return Ok(());
        };

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
                    info!(client_id, "shutdown request");
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
                    info!(client_id, "exit notification");
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
    use tracing_subscriber::fmt::format::FmtSpan;
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_span_events(FmtSpan::NEW)
        .pretty();
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .init();
    let main_context = MainContext::new().await;
    main_context.tcp_server().await
}
