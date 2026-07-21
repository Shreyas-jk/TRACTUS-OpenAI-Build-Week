use chaos_core::contract::{ContractSpec, GitOpSet, OpClass, OpSet};
use chaosd::handoff;
use chaosd::server::{serve, ServerConfig};
use chaosd::state::shared_state;
use chaosd::twin::NoTwin;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);
const UNAVAILABLE_REASON: &str = "Chaos Twin unavailable so approve manually or start chaosd.";

fn deps_locked_contract() -> ContractSpec {
    let mut allowed_ops = OpSet::empty();
    allowed_ops.insert(OpClass::Edit);
    allowed_ops.insert(OpClass::Test);
    ContractSpec {
        task: "ct-hook integration test".to_owned(),
        allowed_paths: vec!["**".to_owned()],
        allowed_ops,
        deps_may_change: false,
        git_ops: GitOpSet::empty(),
        network: true,
    }
}

fn edit_paths_contract() -> ContractSpec {
    let mut allowed_ops = OpSet::empty();
    allowed_ops.insert(OpClass::Edit);
    ContractSpec {
        task: "edit only source files".to_owned(),
        allowed_paths: vec!["src/**".to_owned()],
        allowed_ops,
        deps_may_change: false,
        git_ops: GitOpSet::empty(),
        network: false,
    }
}

async fn set_contract(socket: &Path, contract: ContractSpec) {
    let stream = UnixStream::connect(socket).await.unwrap();
    let mut client = BufReader::new(stream);
    let request = serde_json::to_string(&json!({
        "type": "set_contract",
        "contract": contract,
    }))
    .unwrap();
    client
        .get_mut()
        .write_all(request.as_bytes())
        .await
        .unwrap();
    client.get_mut().write_all(b"\n").await.unwrap();
    let mut response = String::new();
    client.read_line(&mut response).await.unwrap();
    assert!(response.contains("\"action\":\"set\""));
}

async fn invoke(socket: &Path, cwd: &Path, payload: Value) -> Output {
    let socket = socket.to_path_buf();
    let cwd = cwd.to_path_buf();
    let input = serde_json::to_vec(&payload).unwrap();
    tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_ct-hook"))
            .current_dir(cwd)
            .env("CHAOSTWIN_SOCK", socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(&input).unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap()
}

fn payload(root: &Path, tool_name: &str, command: &str) -> Value {
    json!({
        "session_id": "codex-hook-test",
        "cwd": root,
        "hook_event_name": "PreToolUse",
        "tool_name": tool_name,
        "tool_use_id": "tool-use-1",
        "tool_input": {"command": command},
    })
}

fn parse_output(output: Output) -> Value {
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

async fn start_daemon(
    root: &Path,
    contract: ContractSpec,
) -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
    let socket = root.join("chaostwin.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let config = Arc::new(ServerConfig::new(
        shared_state(),
        root.to_path_buf(),
        Arc::new(NoTwin),
    ));
    let daemon = tokio::spawn(async move {
        let _ = serve(listener, config).await;
    });
    set_contract(&socket, contract).await;
    (socket, daemon)
}

fn test_root(label: &str) -> std::path::PathBuf {
    let index = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "chaostwin-hook-{label}-{}-{index}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn bash_dependency_change_is_denied_with_the_verbatim_handoff() {
    let root = test_root("deny");
    let (socket, daemon) = start_daemon(&root, deps_locked_contract()).await;

    let response =
        parse_output(invoke(&socket, &root, payload(&root, "Bash", "cargo add axios")).await);

    assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(
        response["hookSpecificOutput"]["permissionDecisionReason"],
        handoff::scope_violation("R-DEP-01: deps_may_change = false")
    );

    daemon.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn in_scope_bash_command_continues() {
    let root = test_root("allow");
    let (socket, daemon) = start_daemon(&root, deps_locked_contract()).await;

    let response = parse_output(invoke(&socket, &root, payload(&root, "Bash", "cargo test")).await);

    assert_eq!(response, json!({"continue": true}));

    daemon.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn non_bash_tool_continues_without_a_daemon() {
    let root = test_root("non-bash");
    let response = parse_output(
        invoke(
            &root.join("missing.sock"),
            &root,
            payload(&root, "Read", "ignored"),
        )
        .await,
    );

    assert_eq!(response, json!({"continue": true}));

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn daemon_down_asks_for_manual_approval() {
    let root = test_root("down");
    let response = parse_output(
        invoke(
            &root.join("missing.sock"),
            &root,
            payload(&root, "Bash", "cargo test"),
        )
        .await,
    );

    assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "ask");
    assert_eq!(
        response["hookSpecificOutput"]["permissionDecisionReason"],
        UNAVAILABLE_REASON
    );
    assert!(response.get("continue").is_none());

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn structured_apply_patch_delete_outside_allowed_paths_is_denied() {
    let root = test_root("patch-del");
    let (socket, daemon) = start_daemon(&root, edit_paths_contract()).await;
    let payload = json!({
        "session_id": "codex-hook-test",
        "cwd": root,
        "hook_event_name": "PreToolUse",
        "tool_name": "apply_patch",
        "tool_use_id": "tool-use-patch-1",
        "tool_input": {
            "changes": [
                {"path": "README.md", "kind": "delete"}
            ]
        },
    });

    let response = parse_output(invoke(&socket, &root, payload).await);

    assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(
        response["hookSpecificOutput"]["permissionDecisionReason"],
        handoff::scope_violation("R-PATH-01: path matches allowed_paths")
    );

    daemon.abort();
    let _ = std::fs::remove_dir_all(root);
}
