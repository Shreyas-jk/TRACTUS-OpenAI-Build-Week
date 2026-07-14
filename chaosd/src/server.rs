use crate::handoff;
use crate::state::{HoldDecision, PendingReport, SharedState};
use crate::twin::{TwinExecutor, TwinOutcome};
use chaos_core::classify::{classify, Classification};
use chaos_core::contract::{ContractSpec, ProofTrace, Reason, Verdict};
use chaos_core::history::{ExitClass, History};
use chaos_core::parse::{parse_with_env, ParseOutcome};
use chaos_core::verdict::{assemble, check, combine_verdicts};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, oneshot};
use tokio::time::{timeout, Duration};

const HOLD_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ServerConfig {
    pub state: SharedState,
    pub workspace_root: PathBuf,
    pub twin: Arc<dyn TwinExecutor>,
    pub events: broadcast::Sender<Value>,
}

impl ServerConfig {
    pub fn new(state: SharedState, workspace_root: PathBuf, twin: Arc<dyn TwinExecutor>) -> Self {
        let (events, _) = broadcast::channel(128);
        Self {
            state,
            workspace_root,
            twin,
            events,
        }
    }
}

pub fn default_socket_path() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("chaostwin.sock");
    }

    let uid = std::env::var("UID").unwrap_or_else(|_| "0".to_owned());
    PathBuf::from(format!("/tmp/chaostwin-{uid}.sock"))
}

pub fn bind_default_listener() -> io::Result<UnixListener> {
    let socket_path = default_socket_path();
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    UnixListener::bind(socket_path)
}

pub async fn serve(listener: UnixListener, config: Arc<ServerConfig>) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, config).await {
                tracing::debug!(%error, "chaosd client connection closed with an error");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, config: Arc<ServerConfig>) -> io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_json(&mut write_half, &json!({"type": "error", "message": error.to_string()})).await?;
                continue;
            }
        };

        match request {
            Request::Subscribe => {
                let mut events = config.events.subscribe();
                while let Ok(event) = events.recv().await {
                    write_json(&mut write_half, &event).await?;
                }
                return Ok(());
            }
            Request::SetContract { contract } => {
                let compiled = contract.compile(&config.workspace_root).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
                })?;
                config.state.lock().await.active_contract = Some(Arc::new(compiled));
                emit(&config, json!({"type": "contract", "action": "set"}));
                write_json(&mut write_half, &json!({"type": "contract", "action": "set"})).await?;
            }
            Request::Report { id, exit_code } => {
                let recorded = record_report(&config.state, &id, exit_code).await;
                write_json(
                    &mut write_half,
                    &json!({"type": "report", "id": id, "recorded": recorded}),
                )
                .await?;
            }
            Request::Resolve { id, decision } => {
                let sender = config.state.lock().await.pending_holds.remove(&id);
                let resolved = sender
                    .map(|sender| sender.send(decision.into()).is_ok())
                    .unwrap_or(false);
                write_json(
                    &mut write_half,
                    &json!({"type": "resolve", "id": id, "resolved": resolved}),
                )
                .await?;
            }
            Request::Propose {
                id,
                cmd,
                cwd,
                env,
                agent_session,
            } => {
                emit(&config, json!({"type": "proposed", "id": id, "cmd": cmd}));
                handle_propose(
                    &mut write_half,
                    &config,
                    id,
                    cmd,
                    cwd,
                    env,
                    agent_session.unwrap_or_else(|| "default".to_owned()),
                )
                .await?;
            }
        }
    }

    Ok(())
}

async fn handle_propose(
    writer: &mut OwnedWriteHalf,
    config: &Arc<ServerConfig>,
    id: String,
    raw_command: String,
    cwd: PathBuf,
    env: HashMap<String, String>,
    agent_session: String,
) -> io::Result<()> {
    let pipeline = evaluate(&config, &raw_command, &cwd, &env, &agent_session).await;

    match pipeline.verdict {
        Verdict::InScope => {
            save_pending_report(&config.state, &id, &agent_session, pipeline.commands).await;
            let response = json!({"type": "verdict", "id": id, "action": "allow"});
            emit(config, response.clone());
            write_json(writer, &response).await
        }
        Verdict::ScopeViolation(proofs) => {
            let clause = violated_clause(&proofs);
            let response = blocked_response(&id, handoff::scope_violation(&clause), &proofs);
            emit(config, json!({"type": "blocked", "id": id, "proofs": proof_values(&proofs)}));
            write_json(writer, &response).await
        }
        Verdict::Loop { n, signature } => {
            let response = json!({
                "type": "verdict",
                "id": id,
                "action": "block",
                "exit_code": 1,
                "synthetic_stdout": handoff::loop_halt(n),
                "signature": signature,
            });
            emit(config, json!({"type": "loop-halt", "id": id, "n": n}));
            write_json(writer, &response).await
        }
        Verdict::NeedsHuman(_) => {
            let (sender, receiver) = oneshot::channel();
            config.state.lock().await.pending_holds.insert(id.clone(), sender);
            let hold = json!({"type": "verdict", "id": id, "action": "hold"});
            emit(config, hold.clone());
            write_json(writer, &hold).await?;

            let decision = timeout(HOLD_TIMEOUT, receiver).await;
            config.state.lock().await.pending_holds.remove(&id);
            match decision {
                Ok(Ok(HoldDecision::ApproveOnce)) => {
                    save_pending_report(&config.state, &id, &agent_session, pipeline.commands).await;
                    let response = json!({"type": "verdict", "id": id, "action": "allow"});
                    emit(config, response.clone());
                    write_json(writer, &response).await
                }
                Ok(Ok(HoldDecision::Reject)) | Ok(Err(_)) | Err(_) => {
                    let response = json!({
                        "type": "verdict",
                        "id": id,
                        "action": "block",
                        "exit_code": 1,
                        "synthetic_stdout": handoff::needs_human(),
                    });
                    emit(config, json!({"type": "blocked", "id": id, "reason": "needs-human"}));
                    write_json(writer, &response).await
                }
            }
        }
    }
}

struct PipelineResult {
    verdict: Verdict,
    commands: Vec<Vec<String>>,
}

async fn evaluate(
    config: &Arc<ServerConfig>,
    raw_command: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    agent_session: &str,
) -> PipelineResult {
    let (contract, history) = {
        let mut state = config.state.lock().await;
        let history = state
            .histories
            .entry(agent_session.to_owned())
            .or_insert_with(History::default)
            .clone();
        (state.active_contract.clone(), history)
    };
    let Some(contract) = contract else {
        return PipelineResult {
            verdict: Verdict::NeedsHuman(Reason::ContractAmbiguous),
            commands: Vec::new(),
        };
    };

    match parse_with_env(raw_command, env) {
        ParseOutcome::Opaque(_) => PipelineResult {
            verdict: Verdict::NeedsHuman(Reason::Opaque),
            commands: Vec::new(),
        },
        ParseOutcome::NeedsHuman(reason) => PipelineResult {
            verdict: Verdict::NeedsHuman(reason),
            commands: Vec::new(),
        },
        ParseOutcome::Commands(commands) => {
            let mut verdicts = Vec::with_capacity(commands.len());
            let report_commands = commands.iter().map(|command| command.argv.clone()).collect();

            for command in &commands {
                let classification = classify(command, cwd, &config.workspace_root);
                let verdict = match classification {
                    Classification::Effects(_) => {
                        let effects = assemble(command, classification, cwd, &config.workspace_root);
                        check(&effects, &contract, &history)
                    }
                    Classification::Unclassified => match config.twin.speculate(command, cwd).await {
                        TwinOutcome::Effects(effects) => check(&effects, &contract, &history),
                        TwinOutcome::NeedsHuman(reason) => Verdict::NeedsHuman(reason),
                    },
                };
                verdicts.push(verdict);
            }

            PipelineResult {
                verdict: combine_verdicts(verdicts),
                commands: report_commands,
            }
        }
    }
}

async fn save_pending_report(
    state: &SharedState,
    id: &str,
    agent_session: &str,
    commands: Vec<Vec<String>>,
) {
    state.lock().await.pending_reports.insert(
        id.to_owned(),
        PendingReport {
            agent_session: agent_session.to_owned(),
            commands,
        },
    );
}

async fn record_report(state: &SharedState, id: &str, exit_code: Option<i32>) -> bool {
    let Some(report) = state.lock().await.pending_reports.remove(id) else {
        return false;
    };
    let exit_class = match exit_code {
        Some(0) => ExitClass::Success,
        Some(code) if code < 0 => ExitClass::Signal,
        Some(_) => ExitClass::NonZero,
        None => ExitClass::Signal,
    };
    let mut state = state.lock().await;
    let history = state
        .histories
        .entry(report.agent_session)
        .or_insert_with(History::default);
    for command in report.commands {
        history.record_execution(&command, exit_class);
    }
    true
}

fn violated_clause(proofs: &[ProofTrace]) -> String {
    proofs
        .first()
        .map(|proof| format!("{}: {}", proof.rule, proof.clause))
        .unwrap_or_else(|| "scope contract violated".to_owned())
}

fn proof_values(proofs: &[ProofTrace]) -> Vec<Value> {
    proofs
        .iter()
        .map(|proof| {
            json!({
                "rule": proof.rule,
                "clause": proof.clause,
                "effect": proof.effect,
                "rendered": proof.rendered,
            })
        })
        .collect()
}

fn blocked_response(id: &str, synthetic_stdout: String, proofs: &[ProofTrace]) -> Value {
    json!({
        "type": "verdict",
        "id": id,
        "action": "block",
        "exit_code": 1,
        "synthetic_stdout": synthetic_stdout,
        "proofs": proof_values(proofs),
    })
}

fn emit(config: &ServerConfig, event: Value) {
    let _ = config.events.send(event);
}

async fn write_json(writer: &mut OwnedWriteHalf, value: &Value) -> io::Result<()> {
    let line = serde_json::to_string(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Propose {
        id: String,
        cmd: String,
        cwd: PathBuf,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        agent_session: Option<String>,
    },
    Report {
        id: String,
        #[serde(default)]
        exit_code: Option<i32>,
    },
    Subscribe,
    SetContract {
        contract: ContractSpec,
    },
    Resolve {
        id: String,
        decision: WireDecision,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireDecision {
    ApproveOnce,
    Reject,
}

impl From<WireDecision> for HoldDecision {
    fn from(value: WireDecision) -> Self {
        match value {
            WireDecision::ApproveOnce => Self::ApproveOnce,
            WireDecision::Reject => Self::Reject,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff;
    use crate::state::shared_state;
    use crate::twin::NoTwin;
    use chaos_core::contract::{GitOp, GitOpSet, OpClass, OpSet};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);

    fn test_contract() -> ContractSpec {
        let mut allowed_ops = OpSet::empty();
        for operation in [OpClass::Read, OpClass::Edit, OpClass::Test, OpClass::Build] {
            allowed_ops.insert(operation);
        }
        let mut git_ops = GitOpSet::empty();
        git_ops.insert(GitOp::Status);
        git_ops.insert(GitOp::Diff);
        ContractSpec {
            task: "fix the failing test in tests/api_test.rs".to_owned(),
            allowed_paths: vec![
                "tests/**".to_owned(),
                "src/api/**".to_owned(),
                "target/**".to_owned(),
            ],
            allowed_ops,
            deps_may_change: false,
            git_ops,
            network: false,
        }
    }

    async fn send_and_read(
        client: &mut BufReader<UnixStream>,
        message: Value,
    ) -> Value {
        let encoded = serde_json::to_string(&message).unwrap();
        client.get_mut().write_all(encoded.as_bytes()).await.unwrap();
        client.get_mut().write_all(b"\n").await.unwrap();
        client.get_mut().flush().await.unwrap();
        let mut response = String::new();
        client.read_line(&mut response).await.unwrap();
        serde_json::from_str(&response).unwrap()
    }

    async fn client(socket: &Path) -> BufReader<UnixStream> {
        BufReader::new(UnixStream::connect(socket).await.unwrap())
    }

    #[tokio::test]
    async fn uds_protocol_allows_blocks_and_resolves() {
        let suffix = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        let socket = std::env::temp_dir().join(format!(
            "chaostwin-server-test-{}-{suffix}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let config = Arc::new(ServerConfig::new(
            shared_state(),
            PathBuf::from("/workspace/repo"),
            Arc::new(NoTwin),
        ));
        let daemon = tokio::spawn(serve(listener, Arc::clone(&config)));

        let mut control = client(&socket).await;
        let contract_response = send_and_read(
            &mut control,
            json!({"type": "set_contract", "contract": test_contract()}),
        )
        .await;
        assert_eq!(contract_response["action"], "set");

        let mut shim = client(&socket).await;
        let allow = send_and_read(
            &mut shim,
            json!({
                "type": "propose",
                "id": "allow-1",
                "cmd": "cargo test",
                "cwd": "/workspace/repo",
                "agent_session": "agent-a",
            }),
        )
        .await;
        assert_eq!(allow["action"], "allow");

        let report = send_and_read(
            &mut shim,
            json!({"type": "report", "id": "allow-1", "exit_code": 0}),
        )
        .await;
        assert_eq!(report["recorded"], true);

        let block = send_and_read(
            &mut shim,
            json!({
                "type": "propose",
                "id": "block-1",
                "cmd": "cargo add axios",
                "cwd": "/workspace/repo",
                "agent_session": "agent-a",
            }),
        )
        .await;
        assert_eq!(block["action"], "block");
        assert_eq!(
            block["synthetic_stdout"],
            handoff::scope_violation("R-NET-01: network = false")
        );

        let mut held = client(&socket).await;
        let hold = send_and_read(
            &mut held,
            json!({
                "type": "propose",
                "id": "hold-1",
                "cmd": "unclassified-tool --preview",
                "cwd": "/workspace/repo",
                "agent_session": "agent-a",
            }),
        )
        .await;
        assert_eq!(hold["action"], "hold");

        let resolve = send_and_read(
            &mut control,
            json!({"type": "resolve", "id": "hold-1", "decision": "approve_once"}),
        )
        .await;
        assert_eq!(resolve["resolved"], true);
        let mut final_response = String::new();
        held.read_line(&mut final_response).await.unwrap();
        let final_response: Value = serde_json::from_str(&final_response).unwrap();
        assert_eq!(final_response["action"], "allow");

        daemon.abort();
        let _ = std::fs::remove_file(socket);
    }
}
