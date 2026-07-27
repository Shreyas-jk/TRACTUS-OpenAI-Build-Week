use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[path = "../../tractus_socket.rs"]
mod socket_path;

pub const HOLD_WAIT: Duration = Duration::from_secs(65);
pub const REPORT_ACK_WAIT: Duration = Duration::from_secs(2);
pub const CONTRACT_SETUP_WAIT: Duration = Duration::from_secs(2);
const DEFAULT_HOLD_REASON: &str = "Tractus requires manual review.";

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ResolveMode {
    Daemon,
    Client,
}

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

/// Submit the shared JSON-lines `propose` request used by every Tractus adapter.
pub fn request_verdict(
    command: &str,
    cwd: &Path,
    agent_session: &str,
    environment: HashMap<String, String>,
) -> Result<ShimVerdict, ()> {
    request_verdict_with_resolve_mode(
        command,
        cwd,
        agent_session,
        environment,
        ResolveMode::Daemon,
    )
}

/// Submit a proposal whose human-resolution lifecycle is owned by the caller.
pub fn request_verdict_with_resolve_mode(
    command: &str,
    cwd: &Path,
    agent_session: &str,
    environment: HashMap<String, String>,
    resolve_mode: ResolveMode,
) -> Result<ShimVerdict, ()> {
    let id = command_id();
    let mut proposal = json!({
        "type": "propose",
        "id": id,
        "cmd": command,
        "cwd": cwd,
        "env": environment,
        "agent_session": agent_session,
    });
    if resolve_mode == ResolveMode::Client {
        proposal["resolve_mode"] = json!("client");
    }
    if let Some(contract_id) = active_contract_id() {
        proposal["contract_id"] = json!(contract_id);
    }
    submit_proposal(id, proposal)
}

/// Submit a native-editor change as deterministic write effects. The daemon
/// normalizes these paths against the proposed working directory before it
/// evaluates the active contract.
pub fn request_edit_verdict(
    writes: &[String],
    deletes: &[String],
    cwd: &Path,
    agent_session: &str,
) -> Result<ShimVerdict, ()> {
    request_edit_verdict_with_resolve_mode(writes, deletes, cwd, agent_session, ResolveMode::Daemon)
}

/// Submit native-editor effects whose human-resolution lifecycle is owned by
/// the caller rather than the daemon.
pub fn request_edit_verdict_with_resolve_mode(
    writes: &[String],
    deletes: &[String],
    cwd: &Path,
    agent_session: &str,
    resolve_mode: ResolveMode,
) -> Result<ShimVerdict, ()> {
    if writes.is_empty() && deletes.is_empty() {
        return Err(());
    }

    let id = command_id();
    let mut proposal = json!({
        "type": "propose_edit",
        "id": id,
        "cwd": cwd,
        "writes": writes,
        "deletes": deletes,
        "agent_session": agent_session,
    });
    if resolve_mode == ResolveMode::Client {
        proposal["resolve_mode"] = json!("client");
    }
    if let Some(contract_id) = active_contract_id() {
        proposal["contract_id"] = json!(contract_id);
    }
    submit_proposal(id, proposal)
}

/// Install a named contract over an explicit socket and require a matching
/// acknowledgment. The Tractus launcher uses this before it starts Codex; a
/// malformed or legacy response is a failure rather than an implicit allow.
pub fn set_contract_at(
    socket_path: &Path,
    workspace_root: &Path,
    contract_id: &str,
    contract: &Value,
) -> Result<(), ()> {
    if contract_id.trim().is_empty() {
        return Err(());
    }
    let mut connection = UnixStream::connect(socket_path).map_err(|_| ())?;
    connection
        .set_read_timeout(Some(CONTRACT_SETUP_WAIT))
        .map_err(|_| ())?;
    connection
        .set_write_timeout(Some(CONTRACT_SETUP_WAIT))
        .map_err(|_| ())?;
    write_json(
        &mut connection,
        &json!({
            "type": "set_contract",
            "contract_id": contract_id,
            "workspace_root": workspace_root,
            "contract": contract,
        }),
    )?;
    let response = read_json_value(&mut connection, CONTRACT_SETUP_WAIT)?;
    (response.get("type").and_then(Value::as_str) == Some("contract")
        && response.get("action").and_then(Value::as_str) == Some("set")
        && response.get("contract_id").and_then(Value::as_str) == Some(contract_id)
        && response
            .get("workspace_root")
            .and_then(Value::as_str)
            .is_some_and(|registered_root| {
                same_workspace_root(workspace_root, Path::new(registered_root))
            }))
    .then_some(())
    .ok_or(())
}

fn same_workspace_root(expected: &Path, actual: &Path) -> bool {
    canonical_or_original(expected) == canonical_or_original(actual)
}

fn canonical_or_original(path: &Path) -> std::path::PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
    let value = read_json_value(connection, timeout)?;
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

fn read_json_value(connection: &mut UnixStream, timeout: Duration) -> Result<Value, ()> {
    connection.set_read_timeout(Some(timeout)).map_err(|_| ())?;
    let mut line = String::new();
    BufReader::new(connection.try_clone().map_err(|_| ())?)
        .read_line(&mut line)
        .map_err(|_| ())?;
    if line.is_empty() {
        return Err(());
    }
    serde_json::from_str(&line).map_err(|_| ())
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

fn active_contract_id() -> Option<String> {
    env::var("TRACTUS_CONTRACT_ID")
        .ok()
        .filter(|contract_id| !contract_id.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_acknowledgment_accepts_only_the_same_workspace() {
        let root = std::env::temp_dir().join(format!("tractus-shim-root-{}", process::id()));
        fs::create_dir_all(&root).unwrap();

        assert!(same_workspace_root(
            &root,
            &fs::canonicalize(&root).unwrap()
        ));
        assert!(!same_workspace_root(&root, Path::new("/another/workspace")));

        let _ = fs::remove_dir_all(root);
    }
}
