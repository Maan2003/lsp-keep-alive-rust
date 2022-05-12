mod lsp;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{io, sync::Arc};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};

use tokio::net::{tcp::OwnedWriteHalf, TcpListener, TcpStream};
use tokio::sync::Mutex;

pub struct MainContext {
    servers: Mutex<Vec<Arc<Server>>>,
}

pub struct Server {
    id: usize,
    stdin: tokio::process::ChildStdin,
    root: PathBuf,
    clients: Mutex<Vec<Client>>,
}

impl Server {
    pub fn new(root: PathBuf) -> io::Result<Arc<Self>> {
        static SERVER_ID: AtomicUsize = AtomicUsize::new(0);
        let id = SERVER_ID.fetch_add(1, Ordering::SeqCst);

        let mut process = tokio::process::Command::new("rust-analyzer");
        process.stdin(std::process::Stdio::piped());
        process.stdout(std::process::Stdio::piped());
        let mut child = process.spawn()?;
        let stdin = child.stdin.take().unwrap();
        // TODO: consider using a buffered writer
        let this = Arc::new(Self {
            id,
            stdin,
            root,
            clients: Mutex::new(Vec::new()),
        });

        Ok(this)
    }

    pub async fn attach_client(&self, client: Client) {
        self.clients.lock().await.push(client);
    }

    pub async fn handle_server_output(
        self: Arc<Self>,
        stdout: tokio::process::ChildStdout,
    ) -> io::Result<()> {
        let mut reader = BufReader::new(stdout);
        while let Some(line) = lsp::Message::read(&mut reader).await? {
            self.handle_server_message(line).await?;
        }
        Ok(())
    }

    pub async fn handle_server_message(&self, message: lsp::Message) -> io::Result<()> {
        todo!()
    }

    pub async fn handle_client_message(&self, message: lsp::Message) -> io::Result<()> {
        todo!()
    }
}

pub struct Client {
    id: usize,
    write: Mutex<OwnedWriteHalf>,
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

    async fn find_or_make_server(self: Arc<Self>, root: &Path) -> io::Result<Arc<Server>> {
        let root = fs::canonicalize(root).await?;
        let mut servers = self.servers.lock().await;
        for server in &*servers {
            if server.root == root {
                return Ok(server.clone());
            }
        }
        let server = Server::new(root)?;
        servers.push(server.clone());
        Ok(server)
    }

    async fn handle_client(self: Arc<Self>, socket: TcpStream) -> io::Result<()> {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);

        let (read, write) = socket.into_split();

        let mut read = BufReader::new(read);

        let root = match lsp::Message::read(&mut read).await? {
            Some(lsp::Message::Request(req)) if req.method == "intialize" => {
                // TODO: better error handling
                req.params
                    .as_object()
                    .unwrap()
                    .get("rootPath")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            }
            _ => {
                println!("didnot get initialize request");
                return Ok(());
            }
        };
        let root = Path::new(&root).canonicalize()?;
        let server = self.find_or_make_server(&root).await?;
        let client = Client {
            id,
            write: Mutex::new(write),
        };
        server.attach_client(client).await;

        Ok(())
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let main_context = MainContext::new().await;
    let join_handle = tokio::spawn(main_context.tcp_server());
    join_handle.await??;
    Ok(())
}
