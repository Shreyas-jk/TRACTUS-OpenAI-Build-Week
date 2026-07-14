use chaos_core::contract::{ContractSpec, Effects, GitOpSet, OpClass, OpSet};
use chaos_core::parse::SimpleCommand;
use chaosd::handoff;
use chaosd::server::{serve, ServerConfig};
use chaosd::state::shared_state;
use chaosd::twin::{TwinExecutor, TwinOutcome};
use serde_json::json;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

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
        allowed_paths: vec!["**".to_owned()],
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
    client.get_mut().write_all(request.as_bytes()).await.unwrap();
    client.get_mut().write_all(b"\n").await.unwrap();
    let mut response = String::new();
    client.read_line(&mut response).await.unwrap();
    assert!(response.contains("\"action\":\"set\""));
}

async fn invoke(socket: &Path, cwd: &Path, command: String) -> Output {
    let socket = socket.to_path_buf();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ct-shim"))
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .env("CHAOSTWIN_SOCK", socket)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn shim_executes_only_allowed_commands_and_fails_closed() {
    let index = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "chaostwin-shim-test-{}-{index}",
        std::process::id()
    ));
    let socket = root.join("chaostwin.sock");
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
        format!("{}\n", handoff::scope_violation("R-NET-01: network = false"))
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
        "Chaos Twin daemon unreachable; command not executed. Start chaosd or unset SHELL.\n"
    );

    daemon.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn shim_rejects_wrong_arguments_with_usage() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ct-shim"))
        .arg("-x")
        .arg("touch should-not-run")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "usage: ct-shim -c <command>\n"
    );
}
