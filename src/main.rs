#![feature(let_else)]
mod lsp;

use lsp::RequestId;
use std::collections::HashMap;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::{io, sync::Arc};
use tokio::fs;
use tokio::io::BufReader;
use tokio::net::tcp::OwnedReadHalf;
use tokio::time::sleep;
use tracing::{error, info, instrument, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use tokio::net::{tcp::OwnedWriteHalf, TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};

pub struct MainContext {
    servers: Mutex<Vec<Server>>,
}

pub struct Server {
    id: usize,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    child: tokio::process::Child,
    root: PathBuf,
    next_request_id: i32,
    initialize_response: Option<lsp::Message>,
    cancel_shutdown: Option<oneshot::Sender<()>>,
}

impl Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("root", &self.root).finish()
    }
}

impl Server {
    #[instrument(name = "new server", level = "info")]
    pub fn new(root: PathBuf) -> io::Result<Self> {
        static SERVER_ID: AtomicUsize = AtomicUsize::new(0);
        let id = SERVER_ID.fetch_add(1, Ordering::SeqCst);

        let mut process = tokio::process::Command::new("rust-analyzer");
        process.stdin(std::process::Stdio::piped());
        process.stdout(std::process::Stdio::piped());
        let mut child = process.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Ok(Self {
            id,
            stdin,
            stdout: BufReader::new(stdout),
            root,
            child,
            next_request_id: 0,
            initialize_response: None,
            cancel_shutdown: None,
        })
    }

    #[instrument(name = "Server::read_message", level = "trace", skip_all, ret)]
    pub async fn read_message(&mut self) -> io::Result<Option<lsp::Message>> {
        lsp::Message::read(&mut self.stdout).await
    }

    pub fn next_request_id(&mut self) -> RequestId {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id.into()
    }

    pub async fn send_message(&mut self, message: lsp::Message) -> io::Result<()> {
        tracing::trace!(?message, "sending message to server");
        message.write(&mut self.stdin).await
    }
}

#[derive(Debug)]
pub struct Client {
    write: OwnedWriteHalf,
    read: BufReader<OwnedReadHalf>,
}

impl Client {
    pub fn new(socket: TcpStream) -> Self {
        let (read, write) = socket.into_split();
        Self {
            write,
            read: BufReader::new(read),
        }
    }

    pub async fn send_message(&mut self, message: &lsp::Message) -> io::Result<()> {
        tracing::trace!(?message, "sending message to client");
        message.write(&mut self.write).await
    }

    #[instrument(name = "Client::read_message", level = "trace", skip_all, ret)]
    pub async fn read_message(&mut self) -> io::Result<Option<lsp::Message>> {
        lsp::Message::read(&mut self.read).await
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

    async fn find_or_spawn_server(&self, root: &Path) -> io::Result<Server> {
        let root = fs::canonicalize(root).await?;
        let mut servers = self.servers.lock().await;
        match servers.iter().position(|s| s.root == root) {
            Some(idx) => Ok(servers.swap_remove(idx)),
            None => Server::new(root),
        }
    }

    #[instrument(level = "info", skip(self, socket))]
    async fn handle_client(self: Arc<Self>, client_id: usize, socket: TcpStream) -> io::Result<()> {
        let mut client = Client::new(socket);

        let Some(init) = client.read_message().await? else {
            error!("client disconnected before initialization");
            return Ok(());
        };

        let Some(root) = lsp::get_root_path(&init) else {
            error!("client sent invalid initialization");
            return Ok(());
        };

        let mut server = self.find_or_spawn_server(&root).await?;
        let root = server.root.clone();

        // server is already initialized, so we just send the initialization response
        // this relies on client sending similar initialization message every time
        if let Some(init_response) = &server.initialize_response {
            client.send_message(init_response).await?;
        } else {
            server.send_message(init).await?;
            let Some(init_response) = server.read_message().await? else {
                error!("server disconnected before initialization response");
                return Ok(());
            };
            client.send_message(&init_response).await?;
            server.initialize_response = Some(init_response);
        }

        let mut request_ids_map = HashMap::new();
        loop {
            tokio::select! {
                client_msg = client.read_message() => {
                    let Some(mut client_msg) = client_msg? else { break; };
                    match &mut client_msg {
                        lsp::Message::Request(req) if req.method == "shutdown" => {
                            let response = lsp::Message::Response(lsp::Response::new_ok(req.id.clone(), serde_json::json!(null)));
                            client.send_message(&response).await?;
                            break;
                        },
                        lsp::Message::Request(req) => {
                            let request_id = server.next_request_id();
                            request_ids_map.insert(request_id.clone(), req.id.clone());
                            req.id = request_id;
                        },
                        _ => {}
                    }

                    server.send_message(client_msg).await?;
                },
                server_msg = server.read_message() => {
                    let Some(mut server_msg) = server_msg? else { return Ok(()); };
                    if let lsp::Message::Response(res) = &mut server_msg {
                        if let Some(request_id) = request_ids_map.remove(&res.id) {
                            res.id = request_id;
                        } else {
                            error!("server sent response with unknown request id");
                        }
                    }
                    // TODO: update message
                    client.send_message(&server_msg).await?;
                }
            }
        }
        // client disconnected

        let (tx, rx) = oneshot::channel();
        server.cancel_shutdown = Some(tx);
        let server_id = server.id;

        {
            let mut servers = self.servers.lock().await;
            servers.push(server);
        }

        tokio::select! {
            _ = sleep(Duration::from_secs(20 * 60)) => {
                let mut servers = self.servers.lock().await;
                let i = servers.iter().position(|s| s.id == server_id).expect("server not found");
                let mut server = servers.swap_remove(i);
                info!(server_root = ?server.root, "shutting down server due to inactivity");
                server.child.kill().await.ok();
            },
            _ = rx => {
                info!(server_root = ?root, "shutting down cancelled");
            }
        };

        Ok(())
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    use tracing_subscriber::fmt::format::FmtSpan;
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
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
