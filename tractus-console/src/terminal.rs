//! PTY-backed terminal bridge for the Tractus console.
//!
//! Bridges an axum WebSocket to a pseudo-terminal running the demo shell
//! (`tractus-shim` by default, overridable with `DEMO_SHELL`). Blocking PTY I/O
//! runs on dedicated threads; the async task shuttles bytes between those
//! threads and the socket. Mirrors the former Python `terminal.py`.

use axum::extract::ws::{Message, WebSocket};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::Value;
use std::env;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;

pub async fn bridge_terminal(mut socket: WebSocket, daemon_socket: PathBuf) {
    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize {
        rows: INITIAL_ROWS,
        cols: INITIAL_COLS,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(pair) => pair,
        Err(_) => return,
    };

    let mut child = match pair.slave.spawn_command(demo_command(&daemon_socket)) {
        Ok(child) => child,
        Err(_) => return,
    };
    // Close the parent's handle on the slave side so the reader sees EOF when the
    // child exits.
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(_) => return,
    };
    let mut writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(_) => return,
    };
    let master = pair.master;

    // PTY output → async task.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if out_tx.blocking_send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Async task → PTY input.
    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        while let Some(bytes) = in_rx.blocking_recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            output = out_rx.recv() => match output {
                Some(bytes) => {
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Some(size) = parse_resize(text.as_str()) {
                        let _ = master.resize(size);
                    } else if in_tx.send(text.as_str().as_bytes().to_vec()).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if in_tx.send(bytes.to_vec()).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

fn demo_command(daemon_socket: &Path) -> CommandBuilder {
    // portable-pty seeds the child from the console's environment, so PATH and
    // friends are inherited. Point the shim at the same daemon the console uses;
    // otherwise it falls back to its own default socket and reports "unreachable".
    let mut builder =
        configured_command().unwrap_or_else(|| CommandBuilder::new(default_shim_path()));
    builder.env("TRACTUS_SOCK", daemon_socket);
    builder
}

fn configured_command() -> Option<CommandBuilder> {
    let configured = env::var("DEMO_SHELL")
        .ok()
        .filter(|value| !value.is_empty())?;
    let mut parts = shell_words::split(&configured).ok()?;
    if parts.is_empty() {
        return None;
    }
    let mut builder = CommandBuilder::new(parts.remove(0));
    builder.args(parts);
    Some(builder)
}

fn default_shim_path() -> PathBuf {
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("tractus-shim");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("tractus-shim")
}

fn parse_resize(text: &str) -> Option<PtySize> {
    let control: Value = serde_json::from_str(text).ok()?;
    if control.get("type").and_then(Value::as_str) != Some("resize") {
        return None;
    }
    let cols = control.get("cols").and_then(Value::as_u64)?;
    let rows = control.get("rows").and_then(Value::as_u64)?;
    if cols == 0 || rows == 0 {
        return None;
    }
    Some(PtySize {
        rows: rows.min(u16::MAX as u64) as u16,
        cols: cols.min(u16::MAX as u64) as u16,
        pixel_width: 0,
        pixel_height: 0,
    })
}
