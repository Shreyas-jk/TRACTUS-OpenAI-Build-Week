//! Async JSON-lines client for the `tractusd` Unix-domain-socket protocol.
//!
//! One request per connection, except for the long-lived subscribe stream.
//! Mirrors the former Python `daemon.py`, including socket-path discovery so the
//! dashboard selects the same workspace-local daemon `tractus codex` started.

use serde_json::{json, Value};
use std::env;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;

/// Find the workspace daemon first, then fall back to the shared default.
pub fn default_socket_path() -> PathBuf {
    if let Some(configured) = env::var_os("TRACTUS_SOCK").filter(|value| !value.is_empty()) {
        return PathBuf::from(configured);
    }
    if let Some(workspace_root) = env::var_os("TRACTUS_WORKSPACE_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(workspace_root)
            .join(".tractus")
            .join("tractusd.sock");
    }
    // `tractus codex` owns a daemon per workspace. Started from that repository,
    // the local contract store is enough to select the same socket.
    if let Ok(cwd) = env::current_dir() {
        let local_store = cwd.join(".tractus");
        if local_store.is_dir() {
            return local_store.join("tractusd.sock");
        }
    }
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(runtime_dir).join("tractus.sock");
    }
    PathBuf::from(format!("/tmp/tractus-{}.sock", current_uid()))
}

fn current_uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|uid| uid.trim().to_owned())
        .filter(|uid| !uid.is_empty())
        .unwrap_or_else(|| "0".to_owned())
}

/// A JSON-lines client bound to one daemon socket.
#[derive(Clone, Debug)]
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    pub async fn set_contract(&self, contract: Value) -> io::Result<Value> {
        self.request(json!({ "type": "set_contract", "contract": contract }))
            .await
    }

    pub async fn resolve(&self, id: &str, decision: &str) -> io::Result<Value> {
        self.request(json!({ "type": "resolve", "id": id, "decision": decision }))
            .await
    }

    /// Open the long-lived event stream and return a line iterator over it.
    pub async fn subscribe(&self) -> io::Result<Lines<BufReader<UnixStream>>> {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        write_json(&mut stream, &json!({ "type": "subscribe" })).await?;
        Ok(BufReader::new(stream).lines())
    }

    async fn request(&self, message: Value) -> io::Result<Value> {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        write_json(&mut stream, &message).await?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "tractusd closed the connection without responding",
            ));
        }
        serde_json::from_str(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

async fn write_json(stream: &mut UnixStream, value: &Value) -> io::Result<()> {
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    stream.write_all(&encoded).await?;
    stream.flush().await
}
