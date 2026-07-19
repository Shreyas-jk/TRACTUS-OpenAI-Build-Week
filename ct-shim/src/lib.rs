use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[path = "../../chaostwin_socket.rs"]
mod socket_path;

pub const HOLD_WAIT: Duration = Duration::from_secs(65);
pub const REPORT_ACK_WAIT: Duration = Duration::from_secs(2);
const DEFAULT_HOLD_REASON: &str = "Chaos Twin requires manual review.";

pub enum ShimVerdict {
    Allow {
        connection: UnixStream,
        id: String,
    },
    Block(String),
    Hold {
        connection: UnixStream,
        id: String,
        reason: String,
    },
}

pub enum Response {
    Allow,
    Block(String),
    Hold(String),
}

/// Submit the shared JSON-lines `propose` request used by every Chaos Twin adapter.
pub fn request_verdict(
    command: &str,
    cwd: &Path,
    agent_session: &str,
    environment: HashMap<String, String>,
) -> Result<ShimVerdict, ()> {
    let id = command_id();
    let proposal = json!({
        "type": "propose",
        "id": id,
        "cmd": command,
        "cwd": cwd,
        "env": environment,
        "agent_session": agent_session,
    });
    submit_proposal(id, proposal)
}

/// Submit a native-editor change as deterministic write effects. The daemon
/// normalizes these paths against the proposed working directory before it
/// evaluates the active contract.
pub fn request_edit_verdict(
    writes: &[String],
    cwd: &Path,
    agent_session: &str,
) -> Result<ShimVerdict, ()> {
    if writes.is_empty() {
        return Err(());
    }

    let id = command_id();
    let proposal = json!({
        "type": "propose_edit",
        "id": id,
        "cwd": cwd,
        "writes": writes,
        "agent_session": agent_session,
    });
    submit_proposal(id, proposal)
}

fn submit_proposal(id: String, proposal: Value) -> Result<ShimVerdict, ()> {
    let socket_path = socket_path::default_socket_path();
    let mut connection = UnixStream::connect(socket_path).map_err(|_| ())?;
    connection
        .set_read_timeout(Some(HOLD_WAIT))
        .map_err(|_| ())?;
    connection
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| ())?;
    write_json(&mut connection, &proposal)?;

    match read_response(&mut connection, HOLD_WAIT)? {
        Response::Allow => Ok(ShimVerdict::Allow { connection, id }),
        Response::Block(message) => Ok(ShimVerdict::Block(message)),
        Response::Hold(reason) => Ok(ShimVerdict::Hold {
            connection,
            id,
            reason,
        }),
    }
}

pub fn read_response(connection: &mut UnixStream, timeout: Duration) -> Result<Response, ()> {
    connection.set_read_timeout(Some(timeout)).map_err(|_| ())?;
    let mut line = String::new();
    BufReader::new(connection.try_clone().map_err(|_| ())?)
        .read_line(&mut line)
        .map_err(|_| ())?;
    if line.is_empty() {
        return Err(());
    }
    let value: Value = serde_json::from_str(&line).map_err(|_| ())?;
    match value.get("action").and_then(Value::as_str) {
        Some("allow") => Ok(Response::Allow),
        Some("hold") => Ok(Response::Hold(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_HOLD_REASON)
                .to_owned(),
        )),
        Some("block") => value
            .get("synthetic_stdout")
            .and_then(Value::as_str)
            .map(|message| Response::Block(message.to_owned()))
            .ok_or(()),
        _ => Err(()),
    }
}

pub fn write_json(connection: &mut UnixStream, value: &Value) -> Result<(), ()> {
    let encoded = serde_json::to_string(value).map_err(|_| ())?;
    connection.write_all(encoded.as_bytes()).map_err(|_| ())?;
    connection.write_all(b"\n").map_err(|_| ())?;
    connection.flush().map_err(|_| ())
}

fn command_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("shim-{}-{timestamp}", process::id())
}
