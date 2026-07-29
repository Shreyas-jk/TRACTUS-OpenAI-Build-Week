use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tractus_core::contract::{ContractSpec, GitOpSet, OpClass, OpSet};
use tractusd::handoff;
use tractusd::server::{serve, ServerConfig};
use tractusd::state::shared_state;
use tractusd::twin::NoTwin;

static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);
const UNAVAILABLE_REASON: &str =
    "Tractus unavailable; command denied. Start tractusd and retry, or amend the contract explicitly.";

fn deps_locked_contract() -> ContractSpec {
    let mut allowed_ops = OpSet::empty();
    allowed_ops.insert(OpClass::Edit);
    allowed_ops.insert(OpClass::Test);
    ContractSpec {
        task: "tractus-hook integration test".to_owned(),
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

async fn set_named_contract(
    socket: &Path,
    workspace_root: &Path,
    contract_id: &str,
    contract: ContractSpec,
) {
    let stream = UnixStream::connect(socket).await.unwrap();
    let mut client = BufReader::new(stream);
    let request = serde_json::to_string(&json!({
        "type": "set_contract",
        "contract_id": contract_id,
        "workspace_root": workspace_root,
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
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["action"], "set");
    assert_eq!(response["contract_id"], contract_id);
}

async fn invoke(socket: &Path, cwd: &Path, payload: Value) -> Output {
    invoke_with_contract(socket, cwd, payload, None).await
}

/// Models a `tractus`-launched (managed) session: the launcher exports
/// TRACTUS_WORKSPACE_ROOT, which the session gate uses to enable enforcement.
async fn invoke_with_contract(
    socket: &Path,
    cwd: &Path,
    payload: Value,
    contract_id: Option<&str>,
) -> Output {
    let socket = socket.to_path_buf();
    let cwd = cwd.to_path_buf();
    let contract_id = contract_id.map(str::to_owned);
    let input = serde_json::to_vec(&payload).unwrap();
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_tractus-hook"));
        command
            .current_dir(&cwd)
            .env("TRACTUS_SOCK", socket)
            .env("TRACTUS_WORKSPACE_ROOT", &cwd)
            .env_remove("TRACTUS_CONTRACT_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(contract_id) = contract_id {
            command.env("TRACTUS_CONTRACT_ID", contract_id);
        }
        let mut child = command.spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(&input).unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap()
}

/// Models an ordinary Codex session Tractus never launched: no managed-session
/// markers, so the global hook must pass the tool through.
async fn invoke_unmanaged(socket: &Path, cwd: &Path, payload: Value) -> Output {
    let socket = socket.to_path_buf();
    let cwd = cwd.to_path_buf();
    let input = serde_json::to_vec(&payload).unwrap();
    tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tractus-hook"))
            .current_dir(cwd)
            .env("TRACTUS_SOCK", socket)
            .env_remove("TRACTUS_WORKSPACE_ROOT")
            .env_remove("TRACTUS_CONTRACT_ID")
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
    let socket = root.join("tractus.sock");
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
        "tractus-hook-{label}-{}-{index}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn unmanaged_session_without_contract_id_passes_through() {
    // No TRACTUS_CONTRACT_ID and no daemon: an ordinary Codex session the
    // globally-installed hook must not touch, even for a would-be violation.
    let root = test_root("unmanaged");
    let response = parse_output(
        invoke_unmanaged(
            &root.join("missing.sock"),
            &root,
            payload(&root, "Bash", "cargo add axios"),
        )
        .await,
    );

    assert_eq!(response, json!({"continue": true}));
    let _ = std::fs::remove_dir_all(root);
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
async fn launcher_contract_id_binds_bash_and_native_edits_to_the_named_contract() {
    let root = test_root("named-contract");
    let (socket, daemon) = start_daemon(&root, deps_locked_contract()).await;

    let mut named = deps_locked_contract();
    named.allowed_paths = vec!["tests/**".to_owned(), "target/**".to_owned()];
    named.allowed_ops = OpSet::empty();
    named.allowed_ops.insert(OpClass::Edit);
    set_named_contract(&socket, &root, "active-project-contract", named).await;

    let bash = parse_output(
        invoke_with_contract(
            &socket,
            &root,
            payload(&root, "Bash", "cargo test"),
            Some("active-project-contract"),
        )
        .await,
    );
    assert_eq!(bash["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(bash["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .is_some_and(|reason| reason.contains("R-OP-01")));

    let patch = "*** Begin Patch\n*** Add File: src/forbidden.rs\n+blocked\n*** End Patch";
    let native_edit = parse_output(
        invoke_with_contract(
            &socket,
            &root,
            payload(&root, "apply_patch", patch),
            Some("active-project-contract"),
        )
        .await,
    );
    assert_eq!(
        native_edit["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    assert!(
        native_edit["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .is_some_and(|reason| reason.contains("R-PATH-01"))
    );

    let unknown = parse_output(
        invoke_with_contract(
            &socket,
            &root,
            payload(&root, "Bash", "cargo test"),
            Some("missing-project-contract"),
        )
        .await,
    );
    assert_eq!(unknown["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_ne!(unknown, json!({"continue": true}));

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
async fn daemon_down_denies_fail_closed() {
    let root = test_root("down");
    let response = parse_output(
        invoke(
            &root.join("missing.sock"),
            &root,
            payload(&root, "Bash", "cargo test"),
        )
        .await,
    );

    assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(
        response["hookSpecificOutput"]["permissionDecisionReason"],
        UNAVAILABLE_REASON
    );
    assert!(response.get("continue").is_none());

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn real_codex_apply_patch_command_payload_is_denied_outside_allowed_paths() {
    let root = test_root("patch-command");
    let (socket, daemon) = start_daemon(&root, edit_paths_contract()).await;
    let payload = json!({
        "session_id": "captured-codex-session",
        "cwd": root,
        "hook_event_name": "PreToolUse",
        "model": "gpt-5.6-terra",
        "permission_mode": "default",
        "tool_name": "apply_patch",
        "tool_use_id": "exec-captured",
        "turn_id": "turn-captured",
        "tool_input": {
            "command": "*** Begin Patch\n*** Add File: forbidden-smoke.txt\n+blocked\n*** End Patch"
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

#[tokio::test]
async fn needs_human_is_denied_instead_of_returning_unsupported_ask() {
    let root = test_root("hold");
    let (socket, daemon) = start_daemon(&root, deps_locked_contract()).await;

    let response =
        parse_output(invoke(&socket, &root, payload(&root, "Bash", "eval echo opaque")).await);

    assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(response["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .is_some_and(
            |reason| reason.starts_with("Tractus requires manual review; command denied:")
        ));

    daemon.abort();
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
