use serde_json::json;
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::pin::Pin;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tractus_core::contract::{ContractSpec, Effects, GitOpSet, OpClass, OpSet};
use tractus_core::parse::SimpleCommand;
use tractusd::handoff;
use tractusd::server::{serve, ServerConfig};
use tractusd::state::shared_state;
use tractusd::twin::{TwinExecutor, TwinOutcome};

static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

struct AllowTouchTwin;

impl TwinExecutor for AllowTouchTwin {
    fn speculate<'a>(
        &'a self,
        command: &'a SimpleCommand,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = TwinOutcome> + Send + 'a>> {
        let path = command
            .argv
            .get(1)
            .map(|path| cwd.join(path))
            .unwrap_or_else(|| cwd.join("unknown"));
        Box::pin(async move {
            TwinOutcome::Effects(Effects {
                writes: vec![path],
                op: OpClass::Create,
                ..Effects::default()
            })
        })
    }
}

fn contract() -> ContractSpec {
    let mut allowed_ops = OpSet::empty();
    for operation in [
        OpClass::Read,
        OpClass::Edit,
        OpClass::Create,
        OpClass::Delete,
        OpClass::Test,
        OpClass::Build,
        OpClass::Run,
    ] {
        allowed_ops.insert(operation);
    }
    ContractSpec {
        task: "shim integration test".to_owned(),
        allowed_paths: vec!["**".to_owned(), "target/**".to_owned()],
        allowed_ops,
        deps_may_change: false,
        git_ops: GitOpSet::empty(),
        network: false,
    }
}

async fn set_contract(socket: &Path) {
    let stream = UnixStream::connect(socket).await.unwrap();
    let mut client = BufReader::new(stream);
    let request = serde_json::to_string(&json!({
        "type": "set_contract",
        "contract": contract(),
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

async fn invoke(socket: &Path, cwd: &Path, command: String) -> Output {
    let socket = socket.to_path_buf();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_tractus-shim"))
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .env("TRACTUS_SOCK", socket)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

async fn invoke_repl(socket: &Path, cwd: &Path, input: Vec<u8>, path: &Path) -> Output {
    let socket = socket.to_path_buf();
    let cwd = cwd.to_path_buf();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tractus-shim"))
            .current_dir(cwd)
            .env("TRACTUS_SOCK", socket)
            .env("PATH", path)
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

#[tokio::test]
async fn shim_executes_only_allowed_commands_and_fails_closed() {
    let index = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("tractus-shim-test-{}-{index}", std::process::id()));
    let socket = root.join("tractus.sock");
    std::fs::create_dir_all(&root).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let config = Arc::new(ServerConfig::new(
        shared_state(),
        root.clone(),
        Arc::new(AllowTouchTwin),
    ));
    let daemon = tokio::spawn(serve(listener, config));
    set_contract(&socket).await;

    let allowed = root.join("allowed.txt");
    let allow = invoke(&socket, &root, format!("touch {}", allowed.display())).await;
    assert!(allow.status.success());
    assert!(allowed.exists());

    let blocked = root.join("blocked.txt");
    let block = invoke(
        &socket,
        &root,
        format!("cargo add axios && touch {}", blocked.display()),
    )
    .await;
    assert_eq!(block.status.code(), Some(1));
    assert!(!blocked.exists());
    assert_eq!(
        String::from_utf8(block.stdout).unwrap(),
        format!(
            "{}\n",
            handoff::scope_violation("R-NET-01: network = false")
        )
    );

    let daemon_down = root.join("daemon-down.txt");
    let missing_socket = root.join("missing.sock");
    let down = invoke(
        &missing_socket,
        &root,
        format!("touch {}", daemon_down.display()),
    )
    .await;
    assert_eq!(down.status.code(), Some(1));
    assert!(!daemon_down.exists());
    assert_eq!(
        String::from_utf8(down.stdout).unwrap(),
        "Tractus daemon unreachable; command not executed. Start tractusd or unset SHELL.\n"
    );

    daemon.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn repl_executes_allowed_commands_and_returns_block_handoffs() {
    let index = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("tractus-repl-{}-{index}", std::process::id()));
    let socket = root.join("tractus.sock");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake_cargo = bin.join("cargo");
    std::fs::write(
        &fake_cargo,
        "#!/bin/sh\nprintf 'REPL_EXECUTED: %s\\n' \"$*\"\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_cargo, permissions).unwrap();

    let compiled = contract().compile(&root).unwrap();
    assert!(compiled.allowed_paths.is_match(root.join("target")));

    let listener = UnixListener::bind(&socket).unwrap();
    let config = Arc::new(ServerConfig::new(
        shared_state(),
        root.clone(),
        Arc::new(AllowTouchTwin),
    ));
    let daemon = tokio::spawn(serve(listener, config));
    set_contract(&socket).await;

    let output = invoke_repl(
        &socket,
        &root,
        b"cargo test\ncargo add axios\n".to_vec(),
        &bin,
    )
    .await;

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("REPL_EXECUTED: test\n"),
        "unexpected REPL stdout: {stdout:?}"
    );
    assert!(!stdout.contains("REPL_EXECUTED: add axios"));
    assert!(stdout.contains(&handoff::scope_violation("R-NET-01: network = false")));

    daemon.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn shim_rejects_wrong_arguments_with_usage() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tractus-shim"))
        .arg("-x")
        .arg("touch should-not-run")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "usage: tractus-shim -c <command>\n"
    );
}
