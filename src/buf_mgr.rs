use std::{collections::HashSet, io};

use serde_json::json;
use tracing::{error, info, warn};

use crate::{lsp, Server};

pub struct BufMgr {
    open_buffers: HashSet<String>,
}

const TEXT_DOCUMENT_DID_OPEN: &str = "textDocument/didOpen";
const TEXT_DOCUMENT_DID_CLOSE: &str = "textDocument/didClose";

impl BufMgr {
    pub fn new() -> Self {
        BufMgr {
            open_buffers: HashSet::new(),
        }
    }

    pub fn client_message(&mut self, msg: &lsp::Message) {
        match msg {
            lsp::Message::Notification(not) if not.method == TEXT_DOCUMENT_DID_OPEN => {
                let buf = not.params["textDocument"]["uri"]
                    .as_str()
                    .unwrap();
                info!(%buf, "textDocument/didOpen");
                if self.open_buffers.insert(buf.to_string()) {
                    warn!(%buf, "Duplicate textDocument/didOpen");
                }
            }
            lsp::Message::Notification(not) if not.method == TEXT_DOCUMENT_DID_CLOSE => {
                let buf = not.params["textDocument"]["uri"]
                    .as_str()
                    .unwrap()
                    .to_string();
                info!(%buf, "textDocument/didClose");
                if !self.open_buffers.remove(&buf) {
                    error!("client closed an non existent document");
                }
            }
            _ => {}
        }
    }

    pub async fn client_exit(&mut self, server: &mut Server) -> io::Result<()> {
        info!(docs = ?self.open_buffers, "Closing documents");
        for buf in self.open_buffers.drain() {
            let msg = lsp::Message::Notification(lsp::Notification {
                method: TEXT_DOCUMENT_DID_CLOSE.to_string(),
                params: json!({
                    "textDocument": {
                        "uri": buf,
                    }
                }),
            });
            server.send_message(msg).await?;
        }
        Ok(())
    }
}
