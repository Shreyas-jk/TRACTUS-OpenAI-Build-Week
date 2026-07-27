//! The fail-closed `tractus codex` launcher.
//!
//! It turns the selected durable document into a named daemon contract before
//! Codex starts, then exports the matching socket and contract id to Codex and
//! all of its hooks. A workspace-local socket is essential: `chaosd` owns a
//! workspace root, so sharing one per-user socket across repositories would
//! make path enforcement ambiguous.

use chaos_core::contract::ContractError;
use ct_shim::set_contract_at;
use serde_json::Value;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use tractus::store::{ContractDocument, ContractStore, StoreError};

const DAEMON_STARTUP_ATTEMPTS: usize = 40;
const DAEMON_STARTUP_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
pub struct LaunchConfig {
    pub socket_path: PathBuf,
    pub chaosd_bin: PathBuf,
    pub codex_bin: PathBuf,
}

impl LaunchConfig {
    pub fn from_environment(workspace_root: &Path) -> Self {
        Self {
            socket_path: env::var_os("TRACTUS_SOCK")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| workspace_socket_path(workspace_root)),
            chaosd_bin: binary_from_environment("TRACTUS_CHAOSD_BIN", "chaosd"),
            codex_bin: binary_from_environment("TRACTUS_CODEX_BIN", "codex"),
        }
    }
}

/// Launch Codex with the active durable document for `workspace_root`.
pub fn launch_codex(workspace_root: &Path, codex_args: &[String]) -> Result<i32, LauncherError> {
    let workspace_root = canonical_workspace(workspace_root)?;
    let config = LaunchConfig::from_environment(&workspace_root);
    launch_codex_with_config(&workspace_root, codex_args, &config)
}

fn launch_codex_with_config(
    workspace_root: &Path,
    codex_args: &[String],
    config: &LaunchConfig,
) -> Result<i32, LauncherError> {
    let workspace_root = canonical_workspace(workspace_root)?;
    let store = ContractStore::open(&workspace_root)?;
    let document = store
        .load_active()?
        .ok_or(LauncherError::NoActiveContract {
            workspace_root: workspace_root.clone(),
        })?;
    validate_document(&document, &workspace_root)?;

    let contract = serde_json::to_value(document.spec()).map_err(LauncherError::Encode)?;
    register_contract(
        &workspace_root,
        &config.socket_path,
        &config.chaosd_bin,
        &document.id,
        &contract,
    )?;

    let status = Command::new(&config.codex_bin)
        .args(codex_args)
        .current_dir(&workspace_root)
        .env("TRACTUS_SOCK", &config.socket_path)
        .env("TRACTUS_CONTRACT_ID", &document.id)
        .env("TRACTUS_WORKSPACE_ROOT", &workspace_root)
        .status()
        .map_err(|source| LauncherError::Io {
            path: config.codex_bin.clone(),
            source,
        })?;
    Ok(status.code().unwrap_or(1))
}

fn register_contract(
    workspace_root: &Path,
    socket_path: &Path,
    chaosd_bin: &Path,
    contract_id: &str,
    contract: &Value,
) -> Result<(), LauncherError> {
    if socket_is_live(socket_path) {
        return set_contract_at(socket_path, workspace_root, contract_id, contract).map_err(|()| {
            LauncherError::ExistingDaemonRejectedContract {
                socket_path: socket_path.to_path_buf(),
            }
        });
    }

    let mut daemon = Command::new(chaosd_bin)
        .current_dir(workspace_root)
        .env("TRACTUS_SOCK", socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| LauncherError::Io {
            path: chaosd_bin.to_path_buf(),
            source,
        })?;

    for _ in 0..DAEMON_STARTUP_ATTEMPTS {
        if set_contract_at(socket_path, workspace_root, contract_id, contract).is_ok() {
            return Ok(());
        }
        if let Some(status) = daemon.try_wait().map_err(|source| LauncherError::Io {
            path: chaosd_bin.to_path_buf(),
            source,
        })? {
            return Err(LauncherError::DaemonExited {
                program: chaosd_bin.to_path_buf(),
                status: status.code(),
            });
        }
        thread::sleep(DAEMON_STARTUP_INTERVAL);
    }

    // A slow or malformed daemon must not survive as a detached process after
    // the fail-closed startup timeout.
    let _ = daemon.kill();
    let _ = daemon.wait();
    Err(LauncherError::DaemonStartupTimedOut {
        socket_path: socket_path.to_path_buf(),
    })
}

fn validate_document(
    document: &ContractDocument,
    workspace_root: &Path,
) -> Result<(), LauncherError> {
    document
        .spec()
        .compile(workspace_root)
        .map(|_| ())
        .map_err(LauncherError::InvalidContract)
}

fn canonical_workspace(workspace_root: &Path) -> Result<PathBuf, LauncherError> {
    fs::canonicalize(workspace_root).map_err(|source| LauncherError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })
}

fn workspace_socket_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".tractus").join("chaosd.sock")
}

fn socket_is_live(socket_path: &Path) -> bool {
    UnixStream::connect(socket_path).is_ok()
}

fn binary_from_environment(variable: &str, fallback: &str) -> PathBuf {
    if let Some(path) = env::var_os(variable).filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(current_exe) = env::current_exe() {
        if let Some(directory) = current_exe.parent() {
            // Tests live in target/debug/deps while executable workspace
            // binaries are in target/debug. Check both layouts so
            // `cargo run -p tractus -- codex` works without PATH setup.
            for candidate in [
                directory.join(fallback),
                directory
                    .parent()
                    .map(|parent| parent.join(fallback))
                    .unwrap_or_default(),
            ] {
                if candidate.exists() {
                    return candidate;
                }
            }
        }
    }
    PathBuf::from(fallback)
}

#[derive(Debug)]
pub enum LauncherError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Store(StoreError),
    NoActiveContract {
        workspace_root: PathBuf,
    },
    InvalidContract(ContractError),
    Encode(serde_json::Error),
    ExistingDaemonRejectedContract {
        socket_path: PathBuf,
    },
    DaemonExited {
        program: PathBuf,
        status: Option<i32>,
    },
    DaemonStartupTimedOut {
        socket_path: PathBuf,
    },
}

impl fmt::Display for LauncherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "I/O error at {}: {source}", path.display())
            }
            Self::Store(error) => write!(formatter, "contract store error: {error}"),
            Self::NoActiveContract { workspace_root } => write!(
                formatter,
                "no active Tractus contract for {}; run `tractus new` first",
                workspace_root.display()
            ),
            Self::InvalidContract(error) => {
                write!(formatter, "active contract is invalid: {error}")
            }
            Self::Encode(error) => write!(formatter, "could not encode active contract: {error}"),
            Self::ExistingDaemonRejectedContract { socket_path } => write!(
                formatter,
                "the daemon at {} rejected the active contract; Codex was not started",
                socket_path.display()
            ),
            Self::DaemonExited { program, status } => write!(
                formatter,
                "{} exited before accepting the active contract{}",
                program.display(),
                status
                    .map(|status| format!(" (status {status})"))
                    .unwrap_or_default()
            ),
            Self::DaemonStartupTimedOut { socket_path } => write!(
                formatter,
                "daemon did not become ready at {} before the startup timeout",
                socket_path.display()
            ),
        }
    }
}

impl Error for LauncherError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Store(error) => Some(error),
            Self::InvalidContract(error) => Some(error),
            Self::Encode(error) => Some(error),
            _ => None,
        }
    }
}

impl From<StoreError> for LauncherError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chaos_core::contract::{GitOp, GitOpSet, OpClass, OpSet};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestWorkspace {
        root: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "tractus-launch-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn create_active_document(&self) -> ContractDocument {
            let mut operations = OpSet::empty();
            operations.insert(OpClass::Read);
            operations.insert(OpClass::Edit);
            operations.insert(OpClass::Test);
            let mut git_ops = GitOpSet::empty();
            git_ops.insert(GitOp::Status);
            ContractStore::open(&self.root)
                .unwrap()
                .create(chaos_core::contract::ContractSpec {
                    task: "fix the flaky test".to_owned(),
                    allowed_paths: vec!["src/**".to_owned(), "target/**".to_owned()],
                    allowed_ops: operations,
                    deps_may_change: false,
                    git_ops,
                    network: false,
                })
                .unwrap()
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fake_codex(workspace: &TestWorkspace, marker: &Path) -> PathBuf {
        let program = workspace.root.join("fake-codex.sh");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$TRACTUS_SOCK\" > '{}'\nprintf '%s\\n' \"$TRACTUS_CONTRACT_ID\" >> '{}'\npwd >> '{}'\n",
                marker.display(),
                marker.display(),
                marker.display(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).unwrap();
        program
    }

    fn serve_contract_once(socket: &Path) -> thread::JoinHandle<Value> {
        let socket = socket.to_path_buf();
        thread::spawn(move || {
            let listener = UnixListener::bind(&socket).unwrap();
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                // `socket_is_live` intentionally connects before registration
                // so a stale socket can be replaced by a new daemon.
                if request.trim().is_empty() {
                    continue;
                }
                let request: Value = serde_json::from_str(&request).unwrap();
                let contract_id = request["contract_id"].as_str().unwrap();
                let workspace_root = request["workspace_root"].as_str().unwrap();
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({
                        "type": "contract",
                        "action": "set",
                        "contract_id": contract_id,
                        "workspace_root": workspace_root,
                    })
                )
                .unwrap();
                return request;
            }
        })
    }

    #[test]
    fn active_contract_is_registered_before_codex_receives_its_environment() {
        let workspace = TestWorkspace::new();
        let document = workspace.create_active_document();
        let socket = workspace_socket_path(&workspace.root);
        let server = serve_contract_once(&socket);
        let marker = workspace.root.join("codex-environment.txt");
        let config = LaunchConfig {
            socket_path: socket.clone(),
            chaosd_bin: PathBuf::from("/bin/false"),
            codex_bin: fake_codex(&workspace, &marker),
        };

        assert_eq!(
            launch_codex_with_config(&workspace.root, &[], &config).unwrap(),
            0
        );
        let request = server.join().unwrap();
        assert_eq!(request["type"], "set_contract");
        assert_eq!(request["contract_id"], document.id);
        assert_eq!(request["contract"]["task"], "fix the flaky test");
        assert_eq!(
            PathBuf::from(request["workspace_root"].as_str().unwrap()),
            fs::canonicalize(&workspace.root).unwrap()
        );

        let environment = fs::read_to_string(marker).unwrap();
        let lines = environment.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], socket.to_string_lossy());
        assert_eq!(lines[1], document.id);
        assert_eq!(
            PathBuf::from(lines[2]),
            fs::canonicalize(&workspace.root).unwrap()
        );
    }

    #[test]
    fn missing_active_document_never_starts_codex() {
        let workspace = TestWorkspace::new();
        let marker = workspace.root.join("codex-should-not-run.txt");
        let config = LaunchConfig {
            socket_path: workspace_socket_path(&workspace.root),
            chaosd_bin: PathBuf::from("/bin/false"),
            codex_bin: fake_codex(&workspace, &marker),
        };

        assert!(matches!(
            launch_codex_with_config(&workspace.root, &[], &config),
            Err(LauncherError::NoActiveContract { .. })
        ));
        assert!(!marker.exists());
    }

    #[test]
    fn mismatched_daemon_workspace_acknowledgment_blocks_codex() {
        let workspace = TestWorkspace::new();
        workspace.create_active_document();
        let socket = workspace_socket_path(&workspace.root);
        let socket_for_server = socket.clone();
        let server = thread::spawn(move || {
            let listener = UnixListener::bind(socket_for_server).unwrap();
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                if request.trim().is_empty() {
                    continue;
                }
                let request: Value = serde_json::from_str(&request).unwrap();
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({
                        "type": "contract",
                        "action": "set",
                        "contract_id": request["contract_id"],
                        "workspace_root": "/another/workspace",
                    })
                )
                .unwrap();
                return;
            }
        });
        let marker = workspace.root.join("codex-should-not-run.txt");
        let config = LaunchConfig {
            socket_path: socket,
            chaosd_bin: PathBuf::from("/bin/false"),
            codex_bin: fake_codex(&workspace, &marker),
        };

        assert!(matches!(
            launch_codex_with_config(&workspace.root, &[], &config),
            Err(LauncherError::ExistingDaemonRejectedContract { .. })
        ));
        server.join().unwrap();
        assert!(!marker.exists());
    }

    #[test]
    fn daemon_start_failure_never_starts_codex() {
        let workspace = TestWorkspace::new();
        workspace.create_active_document();
        let marker = workspace.root.join("codex-should-not-run.txt");
        let config = LaunchConfig {
            socket_path: workspace_socket_path(&workspace.root),
            chaosd_bin: PathBuf::from("/bin/false"),
            codex_bin: fake_codex(&workspace, &marker),
        };

        assert!(launch_codex_with_config(&workspace.root, &[], &config).is_err());
        assert!(!marker.exists());
    }
}
