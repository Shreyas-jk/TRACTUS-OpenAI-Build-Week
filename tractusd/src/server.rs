use crate::handoff;
use crate::state::{HoldDecision, PendingReport, SessionKey, SharedState};
use crate::twin::{TwinExecutor, TwinOutcome};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, oneshot};
use tokio::time::{timeout, Duration};
use tractus_core::classify::{classify, Classification};
use tractus_core::contract::{ContractSpec, Effects, OpClass, ProofTrace, Reason, Verdict};
use tractus_core::history::{ExitClass, History};
use tractus_core::parse::{normalize_path, parse_with_env, ParseOutcome};
use tractus_core::verdict::{assemble, check, combine_verdicts};

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
    crate::socket_path::default_socket_path()
}

pub fn bind_default_listener() -> io::Result<UnixListener> {
    let socket_path = default_socket_path();
    if socket_path.exists() {
        // Never unlink a live peer's address. A second launcher can race us,
        // and unlinking its socket would make an otherwise healthy daemon
        // unreachable. Only stale socket files are safe to remove.
        match StdUnixStream::connect(&socket_path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "Tractus daemon is already listening at {}",
                        socket_path.display()
                    ),
                ));
            }
            Err(_) => std::fs::remove_file(&socket_path)?,
        }
    }
    UnixListener::bind(socket_path)
}

pub async fn serve(listener: UnixListener, config: Arc<ServerConfig>) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, config).await {
                tracing::debug!(%error, "tractusd client connection closed with an error");
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
                write_json(
                    &mut write_half,
                    &json!({"type": "error", "message": error.to_string()}),
                )
                .await?;
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
            Request::SetContract {
                contract,
                contract_id,
                workspace_root,
            } => {
                let contract_id = normalize_contract_id(contract_id)?;
                if let Some(contract_id) = &contract_id {
                    if !workspace_root
                        .as_deref()
                        .is_some_and(|root| workspace_roots_match(root, &config.workspace_root))
                    {
                        let response = json!({
                            "type": "error",
                            "code": "workspace_mismatch",
                            "message": "named Tractus contracts must name this daemon's workspace",
                            "contract_id": contract_id,
                            "workspace_root": config.workspace_root,
                        });
                        write_json(&mut write_half, &response).await?;
                        continue;
                    }
                }
                let compiled = contract.compile(&config.workspace_root).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
                })?;
                {
                    let mut state = config.state.lock().await;
                    if let Some(contract_id) = &contract_id {
                        state
                            .contracts
                            .insert(contract_id.clone(), Arc::new(compiled));
                    } else {
                        state.active_contract = Some(Arc::new(compiled));
                    }
                }
                let response = json!({
                    "type": "contract",
                    "action": "set",
                    "contract_id": contract_id,
                    "workspace_root": config.workspace_root,
                });
                emit(&config, response.clone());
                write_json(&mut write_half, &response).await?;
            }
            Request::Report { id, exit_code } => {
                let recorded = record_report(&config.state, &id, exit_code).await;
                if recorded {
                    config.twin.invalidate();
                }
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
                contract_id,
                resolve_mode,
            } => {
                emit(
                    &config,
                    json!({"type": "proposed", "id": id, "cmd": cmd, "contract_id": contract_id}),
                );
                handle_propose(
                    &mut write_half,
                    &config,
                    id,
                    cmd,
                    cwd,
                    env,
                    agent_session.unwrap_or_else(|| "default".to_owned()),
                    contract_id,
                    resolve_mode,
                )
                .await?;
            }
            Request::ProposeEdit {
                id,
                cwd,
                writes,
                deletes,
                agent_session,
                contract_id,
                resolve_mode,
            } => {
                let command = format!(
                    "apply_patch writes=[{}] deletes=[{}]",
                    writes
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(" "),
                    deletes
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                emit(
                    &config,
                    json!({"type": "proposed", "id": id, "cmd": command, "contract_id": contract_id}),
                );
                handle_propose_edit(
                    &mut write_half,
                    &config,
                    id,
                    cwd,
                    writes,
                    deletes,
                    agent_session.unwrap_or_else(|| "default".to_owned()),
                    contract_id,
                    resolve_mode,
                )
                .await?;
            }
        }
    }

    Ok(())
}

async fn handle_propose_edit(
    writer: &mut OwnedWriteHalf,
    config: &Arc<ServerConfig>,
    id: String,
    cwd: PathBuf,
    writes: Vec<PathBuf>,
    deletes: Vec<PathBuf>,
    agent_session: String,
    contract_id: Option<String>,
    resolve_mode: ResolveMode,
) -> io::Result<()> {
    let verdict = if writes.is_empty() && deletes.is_empty() {
        Verdict::NeedsHuman(Reason::Opaque)
    } else {
        evaluate_edit_effects(
            config,
            &cwd,
            &writes,
            &deletes,
            &agent_session,
            contract_id.as_deref(),
        )
        .await
    };

    respond(
        writer,
        config,
        id,
        verdict,
        resolve_mode,
        ApproveAction::None,
    )
    .await
}

async fn handle_propose(
    writer: &mut OwnedWriteHalf,
    config: &Arc<ServerConfig>,
    id: String,
    raw_command: String,
    cwd: PathBuf,
    env: HashMap<String, String>,
    agent_session: String,
    contract_id: Option<String>,
    resolve_mode: ResolveMode,
) -> io::Result<()> {
    let pipeline = evaluate(
        &config,
        &raw_command,
        &cwd,
        &env,
        &agent_session,
        contract_id.as_deref(),
    )
    .await;

    let on_approve = pipeline
        .session_key
        .map(|session_key| ApproveAction::SavePendingReport {
            session_key,
            commands: pipeline.commands,
        })
        .unwrap_or(ApproveAction::None);

    respond(
        writer,
        config,
        id,
        pipeline.verdict,
        resolve_mode,
        on_approve,
    )
    .await
}

enum ApproveAction {
    SavePendingReport {
        session_key: SessionKey,
        commands: Vec<Vec<String>>,
    },
    None,
}

async fn respond(
    writer: &mut OwnedWriteHalf,
    config: &Arc<ServerConfig>,
    id: String,
    verdict: Verdict,
    resolve_mode: ResolveMode,
    on_approve: ApproveAction,
) -> io::Result<()> {
    match verdict {
        Verdict::InScope => {
            apply_approve_action(on_approve, &config.state, &id).await;
            let response = json!({"type": "verdict", "id": id, "action": "allow"});
            emit(config, response.clone());
            write_json(writer, &response).await
        }
        Verdict::ScopeViolation(proofs) => {
            let clause = violated_clause(&proofs);
            let response = blocked_response(&id, handoff::scope_violation(&clause), &proofs);
            emit(
                config,
                json!({"type": "blocked", "id": id, "proofs": proof_values(&proofs)}),
            );
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
        Verdict::NeedsHuman(reason) => {
            let receiver = if resolve_mode == ResolveMode::Daemon {
                let (sender, receiver) = oneshot::channel();
                config
                    .state
                    .lock()
                    .await
                    .pending_holds
                    .insert(id.clone(), sender);
                Some(receiver)
            } else {
                None
            };
            let hold = json!({
                "type": "verdict",
                "id": id,
                "action": "hold",
                "reason": needs_human_reason(&reason),
            });
            emit(config, hold.clone());
            write_json(writer, &hold).await?;

            let Some(receiver) = receiver else {
                return Ok(());
            };

            let decision = timeout(HOLD_TIMEOUT, receiver).await;
            config.state.lock().await.pending_holds.remove(&id);
            match decision {
                Ok(Ok(HoldDecision::ApproveOnce)) => {
                    apply_approve_action(on_approve, &config.state, &id).await;
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
                    emit(
                        config,
                        json!({"type": "blocked", "id": id, "reason": "needs-human"}),
                    );
                    write_json(writer, &response).await
                }
            }
        }
    }
}

async fn apply_approve_action(action: ApproveAction, state: &SharedState, id: &str) {
    match action {
        ApproveAction::SavePendingReport {
            session_key,
            commands,
        } => save_pending_report(state, id, session_key, commands).await,
        ApproveAction::None => {}
    }
}

fn needs_human_reason(reason: &Reason) -> &'static str {
    match reason {
        Reason::Opaque => "Tractus could not safely interpret this command.",
        Reason::TwinTimeout => "Tractus speculation timed out.",
        Reason::UnresolvedVar => "Tractus could not resolve a command variable.",
        Reason::ContractAmbiguous => "Tractus has no active contract for this command.",
    }
}

fn normalize_contract_id(contract_id: Option<String>) -> io::Result<Option<String>> {
    match contract_id {
        Some(contract_id) if contract_id.trim().is_empty() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "contract_id must not be empty when supplied",
        )),
        Some(contract_id) => Ok(Some(contract_id)),
        None => Ok(None),
    }
}

/// Compare roots using the filesystem when possible, but retain lexical
/// equality for isolated protocol tests and for a daemon that starts before a
/// workspace path becomes available. The launcher canonicalizes before it
/// sends, so a true cross-workspace socket reuse cannot pass this check.
fn workspace_roots_match(expected: &Path, actual: &Path) -> bool {
    canonical_or_original(expected) == canonical_or_original(actual)
}

fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

struct PipelineResult {
    verdict: Verdict,
    commands: Vec<Vec<String>>,
    session_key: Option<SessionKey>,
}

async fn evaluate(
    config: &Arc<ServerConfig>,
    raw_command: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    agent_session: &str,
    contract_id: Option<&str>,
) -> PipelineResult {
    let (contract, history, session_key) = {
        let mut state = config.state.lock().await;
        let Some((contract, session_key)) = state.resolve_contract(agent_session, contract_id)
        else {
            return PipelineResult {
                verdict: Verdict::NeedsHuman(Reason::ContractAmbiguous),
                commands: Vec::new(),
                session_key: None,
            };
        };
        let history = state
            .histories
            .entry(session_key.clone())
            .or_insert_with(History::default)
            .clone();
        (contract, history, session_key)
    };

    match parse_with_env(raw_command, env) {
        ParseOutcome::Opaque(_) => PipelineResult {
            verdict: Verdict::NeedsHuman(Reason::Opaque),
            commands: Vec::new(),
            session_key: Some(session_key),
        },
        ParseOutcome::NeedsHuman(reason) => PipelineResult {
            verdict: Verdict::NeedsHuman(reason),
            commands: Vec::new(),
            session_key: Some(session_key),
        },
        ParseOutcome::Commands(commands) => {
            let mut verdicts = Vec::with_capacity(commands.len());
            let report_commands = commands
                .iter()
                .map(|command| command.argv.clone())
                .collect();

            for command in &commands {
                let classification = classify(command, cwd, &contract.workspace_root);
                let verdict = match classification {
                    Classification::Effects(_) => {
                        let effects =
                            assemble(command, classification, cwd, &contract.workspace_root);
                        check(&effects, &contract, &history)
                    }
                    Classification::Unclassified => match config.twin.speculate(command, cwd).await
                    {
                        TwinOutcome::Effects(effects) => check(&effects, &contract, &history),
                        TwinOutcome::NeedsHuman(reason) => Verdict::NeedsHuman(reason),
                    },
                };
                verdicts.push(verdict);
            }

            PipelineResult {
                verdict: combine_verdicts(verdicts),
                commands: report_commands,
                session_key: Some(session_key),
            }
        }
    }
}

async fn evaluate_edit_effects(
    config: &Arc<ServerConfig>,
    cwd: &Path,
    writes: &[PathBuf],
    deletes: &[PathBuf],
    agent_session: &str,
    contract_id: Option<&str>,
) -> Verdict {
    let contract = {
        let mut state = config.state.lock().await;
        state
            .resolve_contract(agent_session, contract_id)
            .map(|(contract, _)| contract)
    };
    let Some(contract) = contract else {
        return Verdict::NeedsHuman(Reason::ContractAmbiguous);
    };
    let effects = Effects {
        family: Some("apply_patch".to_owned()),
        writes: writes
            .iter()
            .map(|path| normalize_path(cwd, &contract.workspace_root, path).path)
            .collect(),
        deletes: deletes
            .iter()
            .map(|path| normalize_path(cwd, &contract.workspace_root, path).path)
            .collect(),
        op: OpClass::Edit,
        ..Effects::default()
    };

    // Native editor calls have no report/exit status, so they cannot contribute
    // a loop signature or inherit one from prior shell executions.
    check(&effects, &contract, &History::default())
}

async fn save_pending_report(
    state: &SharedState,
    id: &str,
    session_key: SessionKey,
    commands: Vec<Vec<String>>,
) {
    state.lock().await.pending_reports.insert(
        id.to_owned(),
        PendingReport {
            session_key,
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
        .entry(report.session_key)
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ResolveMode {
    #[default]
    Daemon,
    Client,
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
        #[serde(default)]
        contract_id: Option<String>,
        #[serde(default)]
        resolve_mode: ResolveMode,
    },
    ProposeEdit {
        id: String,
        cwd: PathBuf,
        #[serde(default)]
        writes: Vec<PathBuf>,
        #[serde(default)]
        deletes: Vec<PathBuf>,
        #[serde(default)]
        agent_session: Option<String>,
        #[serde(default)]
        contract_id: Option<String>,
        #[serde(default)]
        resolve_mode: ResolveMode,
    },
    Report {
        id: String,
        #[serde(default)]
        exit_code: Option<i32>,
    },
    Subscribe,
    SetContract {
        contract: ContractSpec,
        #[serde(default)]
        contract_id: Option<String>,
        #[serde(default)]
        workspace_root: Option<PathBuf>,
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
    use serde_json::json;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tractus_core::contract::{GitOp, GitOpSet, OpClass, OpSet};

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

    async fn send_and_read(client: &mut BufReader<UnixStream>, message: Value) -> Value {
        let encoded = serde_json::to_string(&message).unwrap();
        client
            .get_mut()
            .write_all(encoded.as_bytes())
            .await
            .unwrap();
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
            "tractus-server-test-{}-{suffix}.sock",
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

        let mut events = config.events.subscribe();
        let mut client_owned = client(&socket).await;
        let client_hold = send_and_read(
            &mut client_owned,
            json!({
                "type": "propose",
                "id": "client-hold-1",
                "cmd": "unclassified-tool --preview",
                "cwd": "/workspace/repo",
                "agent_session": "agent-a",
                "resolve_mode": "client",
            }),
        )
        .await;
        assert_eq!(client_hold["action"], "hold");
        assert!(config.state.lock().await.pending_holds.is_empty());

        let proposed = events.recv().await.unwrap();
        assert_eq!(proposed["type"], "proposed");
        let hold_event = events.recv().await.unwrap();
        assert_eq!(hold_event["action"], "hold");
        assert!(
            timeout(Duration::from_millis(100), events.recv())
                .await
                .is_err(),
            "client-mode holds must not emit a deferred blocked event"
        );

        daemon.abort();
        let _ = std::fs::remove_file(socket);
    }

    #[tokio::test]
    async fn named_contracts_do_not_fallback_to_the_legacy_contract() {
        let suffix = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        let socket = std::env::temp_dir().join(format!(
            "tractus-named-contract-test-{}-{suffix}.sock",
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
        send_and_read(
            &mut control,
            json!({"type": "set_contract", "contract": test_contract()}),
        )
        .await;

        let mut read_only = test_contract();
        read_only.allowed_ops = OpSet::empty();
        read_only.allowed_ops.insert(OpClass::Read);
        let wrong_workspace = send_and_read(
            &mut control,
            json!({
                "type": "set_contract",
                "contract_id": "wrong-workspace-contract",
                "workspace_root": "/another/workspace",
                "contract": read_only,
            }),
        )
        .await;
        assert_eq!(wrong_workspace["type"], "error");
        assert_eq!(wrong_workspace["code"], "workspace_mismatch");
        assert!(config
            .state
            .lock()
            .await
            .contracts
            .get("wrong-workspace-contract")
            .is_none());

        let mut read_only = test_contract();
        read_only.allowed_ops = OpSet::empty();
        read_only.allowed_ops.insert(OpClass::Read);
        let named_response = send_and_read(
            &mut control,
            json!({
                "type": "set_contract",
                "contract_id": "read-only-contract",
                "workspace_root": "/workspace/repo",
                "contract": read_only,
            }),
        )
        .await;
        assert_eq!(named_response["contract_id"], "read-only-contract");

        let mut shim = client(&socket).await;
        let named_block = send_and_read(
            &mut shim,
            json!({
                "type": "propose",
                "id": "named-block",
                "cmd": "cargo test",
                "cwd": "/workspace/repo",
                "agent_session": "same-codex-session",
                "contract_id": "read-only-contract",
                "resolve_mode": "client",
            }),
        )
        .await;
        assert_eq!(named_block["action"], "block");
        assert_eq!(named_block["proofs"][0]["rule"], "R-OP-01");

        let same_session_without_id = send_and_read(
            &mut shim,
            json!({
                "type": "propose",
                "id": "sticky-named-block",
                "cmd": "cargo test",
                "cwd": "/workspace/repo",
                "agent_session": "same-codex-session",
                "resolve_mode": "client",
            }),
        )
        .await;
        assert_eq!(same_session_without_id["action"], "block");
        assert_eq!(same_session_without_id["proofs"][0]["rule"], "R-OP-01");

        let unknown = send_and_read(
            &mut shim,
            json!({
                "type": "propose",
                "id": "unknown-contract",
                "cmd": "cargo test",
                "cwd": "/workspace/repo",
                "agent_session": "same-codex-session",
                "contract_id": "not-registered",
                "resolve_mode": "client",
            }),
        )
        .await;
        assert_eq!(unknown["action"], "hold");
        assert_eq!(
            unknown["reason"],
            "Tractus has no active contract for this command."
        );

        let legacy_allow = send_and_read(
            &mut shim,
            json!({
                "type": "propose",
                "id": "legacy-allow",
                "cmd": "cargo test",
                "cwd": "/workspace/repo",
                "agent_session": "different-legacy-session",
            }),
        )
        .await;
        assert_eq!(legacy_allow["action"], "allow");

        let mut edit_only = test_contract();
        edit_only.allowed_paths = vec!["tests/**".to_owned()];
        edit_only.allowed_ops = OpSet::empty();
        edit_only.allowed_ops.insert(OpClass::Edit);
        let edit_response = send_and_read(
            &mut control,
            json!({
                "type": "set_contract",
                "contract_id": "tests-only-edit",
                "workspace_root": "/workspace/repo",
                "contract": edit_only,
            }),
        )
        .await;
        assert_eq!(edit_response["contract_id"], "tests-only-edit");

        let edit_block = send_and_read(
            &mut shim,
            json!({
                "type": "propose_edit",
                "id": "named-edit-block",
                "cwd": "/workspace/repo",
                "writes": ["src/forbidden.rs"],
                "agent_session": "same-codex-session",
                "contract_id": "tests-only-edit",
                "resolve_mode": "client",
            }),
        )
        .await;
        assert_eq!(edit_block["action"], "block");
        assert_eq!(edit_block["proofs"][0]["rule"], "R-PATH-01");

        daemon.abort();
        let _ = std::fs::remove_file(socket);
    }

    #[test]
    fn workspace_identity_uses_canonical_paths_when_available() {
        let root = std::env::temp_dir().join(format!(
            "tractus-workspace-identity-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        assert!(workspace_roots_match(
            &root,
            &fs::canonicalize(&root).unwrap()
        ));
        assert!(!workspace_roots_match(
            &root,
            Path::new("/another/workspace")
        ));
        let _ = std::fs::remove_dir_all(root);
    }
}
