//! Durable contract documents for one workspace.
//!
//! The on-disk contract is intentionally the same [`ContractSpec`] consumed by
//! `chaos-core` and `chaosd`; a compiled `Contract` is runtime-only. Documents
//! live under `<workspace>/.tractus/contracts`, while `state.json` records the
//! selected document and a bounded LRU retention policy.

use chaos_core::contract::{ContractError, ContractSpec};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 1;
const TRACTUS_DIRECTORY: &str = ".tractus";
const CONTRACTS_DIRECTORY: &str = "contracts";
const STATE_FILE: &str = "state.json";

/// Smallest permitted number of retained contract documents per workspace.
pub const MIN_RETENTION_LIMIT: usize = 10;
/// Largest permitted number of retained contract documents per workspace.
pub const MAX_RETENTION_LIMIT: usize = 30;
/// Default number of most-recently-used documents retained per workspace.
pub const DEFAULT_RETENTION_LIMIT: usize = 20;

/// A durable, user-approved intent contract.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContractDocument {
    pub schema_version: u32,
    pub id: String,
    pub created_at_ms: u64,
    pub last_used_at_ms: u64,
    pub contract: ContractSpec,
}

impl ContractDocument {
    /// Returns the exact contract payload accepted by `chaos-core`.
    pub fn spec(&self) -> ContractSpec {
        self.contract.clone()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoreState {
    schema_version: u32,
    active_contract_id: Option<String>,
    retention_limit: usize,
}

impl Default for StoreState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            active_contract_id: None,
            retention_limit: DEFAULT_RETENTION_LIMIT,
        }
    }
}

/// Persistent contract storage scoped to one workspace.
#[derive(Clone, Debug)]
pub struct ContractStore {
    workspace_root: PathBuf,
    store_root: PathBuf,
}

impl ContractStore {
    /// Opens (and, if necessary, creates) this workspace's `.tractus` store.
    pub fn open(workspace_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let workspace_root =
            fs::canonicalize(workspace_root.as_ref()).map_err(|source| StoreError::Io {
                path: workspace_root.as_ref().to_path_buf(),
                source,
            })?;
        let store = Self {
            store_root: workspace_root.join(TRACTUS_DIRECTORY),
            workspace_root,
        };
        store.ensure_layout()?;
        Ok(store)
    }

    /// Creates, activates, and persists a new contract document.
    pub fn create(&self, contract: ContractSpec) -> Result<ContractDocument, StoreError> {
        let mut state = self.read_state()?;
        let mut documents = self.read_documents()?;
        self.validate_spec(&contract, "new")?;

        let timestamp = next_timestamp(&documents);
        let id = self.allocate_id(timestamp)?;
        let document = ContractDocument {
            schema_version: SCHEMA_VERSION,
            id: id.clone(),
            created_at_ms: timestamp,
            last_used_at_ms: timestamp,
            contract,
        };

        self.write_document(&document)?;
        documents.push(document.clone());
        state.active_contract_id = Some(id);
        self.prune_lru(
            &mut documents,
            state.active_contract_id.as_deref(),
            state.retention_limit,
        )?;
        self.write_state(&state)?;

        Ok(document)
    }

    /// Returns all documents ordered from most recently used to least recently used.
    pub fn list(&self) -> Result<Vec<ContractDocument>, StoreError> {
        let mut documents = self.read_documents()?;
        documents.sort_by(|left, right| {
            right
                .last_used_at_ms
                .cmp(&left.last_used_at_ms)
                .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(documents)
    }

    /// Returns a document without changing which document is active.
    pub fn get(&self, id: &str) -> Result<ContractDocument, StoreError> {
        self.read_document(id)
    }

    /// Selects a prior document and refreshes its LRU position.
    pub fn activate(&self, id: &str) -> Result<ContractDocument, StoreError> {
        let mut state = self.read_state()?;
        let documents = self.read_documents()?;
        let timestamp = next_timestamp(&documents);
        let mut document = self.read_document(id)?;
        document.last_used_at_ms = timestamp;
        self.write_document(&document)?;
        state.active_contract_id = Some(document.id.clone());
        self.write_state(&state)?;
        Ok(document)
    }

    /// Loads the selected document and refreshes its LRU position.
    ///
    /// A selected-but-missing document is an error rather than an implicit
    /// empty contract, so callers can fail closed.
    pub fn load_active(&self) -> Result<Option<ContractDocument>, StoreError> {
        let state = self.read_state()?;
        match state.active_contract_id {
            Some(id) => self.activate(&id).map(Some),
            None => Ok(None),
        }
    }

    /// Changes the bounded LRU capacity and immediately removes excess
    /// least-recently-used inactive documents.
    pub fn set_retention_limit(&self, retention_limit: usize) -> Result<(), StoreError> {
        validate_retention_limit(retention_limit)?;

        let mut state = self.read_state()?;
        let mut documents = self.read_documents()?;
        state.retention_limit = retention_limit;
        self.prune_lru(
            &mut documents,
            state.active_contract_id.as_deref(),
            state.retention_limit,
        )?;
        self.write_state(&state)
    }

    /// Returns the current bounded LRU capacity.
    pub fn retention_limit(&self) -> Result<usize, StoreError> {
        Ok(self.read_state()?.retention_limit)
    }

    /// Returns the workspace this store is scoped to.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    fn ensure_layout(&self) -> Result<(), StoreError> {
        fs::create_dir_all(self.contracts_dir()).map_err(|source| StoreError::Io {
            path: self.contracts_dir(),
            source,
        })
    }

    fn contracts_dir(&self) -> PathBuf {
        self.store_root.join(CONTRACTS_DIRECTORY)
    }

    fn state_path(&self) -> PathBuf {
        self.store_root.join(STATE_FILE)
    }

    fn document_path(&self, id: &str) -> Result<PathBuf, StoreError> {
        validate_id(id)?;
        Ok(self.contracts_dir().join(format!("{id}.json")))
    }

    fn read_state(&self) -> Result<StoreState, StoreError> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(StoreState::default());
        }

        let state: StoreState = read_json(&path)?;
        if state.schema_version != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema {
                path,
                found: state.schema_version,
            });
        }
        validate_retention_limit(state.retention_limit)?;
        if let Some(id) = &state.active_contract_id {
            validate_id(id)?;
        }
        Ok(state)
    }

    fn write_state(&self, state: &StoreState) -> Result<(), StoreError> {
        write_json_atomic(&self.state_path(), state)
    }

    fn read_documents(&self) -> Result<Vec<ContractDocument>, StoreError> {
        let directory = self.contracts_dir();
        let entries = fs::read_dir(&directory).map_err(|source| StoreError::Io {
            path: directory.clone(),
            source,
        })?;
        let mut documents = Vec::new();

        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            documents.push(self.read_document_path(&path)?);
        }
        Ok(documents)
    }

    fn read_document(&self, id: &str) -> Result<ContractDocument, StoreError> {
        let path = self.document_path(id)?;
        if !path.exists() {
            return Err(StoreError::MissingContract { id: id.to_owned() });
        }
        self.read_document_path(&path)
    }

    fn read_document_path(&self, path: &Path) -> Result<ContractDocument, StoreError> {
        let document: ContractDocument = read_json(path)?;
        if document.schema_version != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema {
                path: path.to_path_buf(),
                found: document.schema_version,
            });
        }
        validate_id(&document.id)?;
        let expected_path = self.document_path(&document.id)?;
        if expected_path != path {
            return Err(StoreError::DocumentPathMismatch {
                path: path.to_path_buf(),
                id: document.id,
            });
        }
        self.validate_spec(&document.contract, &document.id)?;
        Ok(document)
    }

    fn write_document(&self, document: &ContractDocument) -> Result<(), StoreError> {
        let path = self.document_path(&document.id)?;
        write_json_atomic(&path, document)
    }

    fn validate_spec(&self, contract: &ContractSpec, id: &str) -> Result<(), StoreError> {
        contract
            .clone()
            .compile(&self.workspace_root)
            .map(|_| ())
            .map_err(|source| StoreError::InvalidContract {
                id: id.to_owned(),
                source,
            })
    }

    fn allocate_id(&self, timestamp: u64) -> Result<String, StoreError> {
        for sequence in 0..u32::MAX {
            let id = if sequence == 0 {
                format!("contract-{timestamp}-{}", process::id())
            } else {
                format!("contract-{timestamp}-{}-{sequence}", process::id())
            };
            if !self.document_path(&id)?.exists() {
                return Ok(id);
            }
        }
        Err(StoreError::IdAllocationExhausted)
    }

    fn prune_lru(
        &self,
        documents: &mut Vec<ContractDocument>,
        active_contract_id: Option<&str>,
        retention_limit: usize,
    ) -> Result<(), StoreError> {
        while documents.len() > retention_limit {
            let victim = documents
                .iter()
                .enumerate()
                .filter(|(_, document)| Some(document.id.as_str()) != active_contract_id)
                .min_by(|(_, left), (_, right)| {
                    left.last_used_at_ms
                        .cmp(&right.last_used_at_ms)
                        .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                        .then_with(|| left.id.cmp(&right.id))
                })
                .map(|(index, _)| index)
                .ok_or(StoreError::CannotEvictActiveContract)?;
            let document = documents.remove(victim);
            let path = self.document_path(&document.id)?;
            fs::remove_file(&path).map_err(|source| StoreError::Io { path, source })?;
        }
        Ok(())
    }
}

fn next_timestamp(documents: &[ContractDocument]) -> u64 {
    let most_recent = documents
        .iter()
        .map(|document| document.created_at_ms.max(document.last_used_at_ms))
        .max()
        .unwrap_or_default();
    now_millis().max(most_recent.saturating_add(1))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn validate_retention_limit(retention_limit: usize) -> Result<(), StoreError> {
    if (MIN_RETENTION_LIMIT..=MAX_RETENTION_LIMIT).contains(&retention_limit) {
        Ok(())
    } else {
        Err(StoreError::InvalidRetentionLimit {
            requested: retention_limit,
        })
    }
}

fn validate_id(id: &str) -> Result<(), StoreError> {
    if !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        Ok(())
    } else {
        Err(StoreError::InvalidContractId { id: id.to_owned() })
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, StoreError> {
    let contents = fs::read_to_string(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&contents).map_err(|source| StoreError::Decode {
        path: path.to_path_buf(),
        source,
    })
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let encoded = serde_json::to_vec_pretty(value).map_err(|source| StoreError::Encode {
        path: path.to_path_buf(),
        source,
    })?;
    let parent = path.parent().ok_or_else(|| StoreError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory"),
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| StoreError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"),
        })?;
    let temporary = parent.join(format!(
        ".{file_name}-{}-{}.tmp",
        process::id(),
        now_millis()
    ));

    fs::write(&temporary, encoded).map_err(|source| StoreError::Io {
        path: temporary.clone(),
        source,
    })?;
    fs::rename(&temporary, path).map_err(|source| {
        let _ = fs::remove_file(&temporary);
        StoreError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[derive(Debug)]
pub enum StoreError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Encode {
        path: PathBuf,
        source: serde_json::Error,
    },
    Decode {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidContract {
        id: String,
        source: ContractError,
    },
    UnsupportedSchema {
        path: PathBuf,
        found: u32,
    },
    InvalidRetentionLimit {
        requested: usize,
    },
    InvalidContractId {
        id: String,
    },
    MissingContract {
        id: String,
    },
    DocumentPathMismatch {
        path: PathBuf,
        id: String,
    },
    CannotEvictActiveContract,
    IdAllocationExhausted,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "I/O error at {}: {source}", path.display()),
            Self::Encode { path, source } => {
                write!(formatter, "could not encode contract store file {}: {source}", path.display())
            }
            Self::Decode { path, source } => {
                write!(formatter, "could not decode contract store file {}: {source}", path.display())
            }
            Self::InvalidContract { id, source } => {
                write!(formatter, "contract {id:?} is invalid: {source}")
            }
            Self::UnsupportedSchema { path, found } => write!(
                formatter,
                "contract store file {} uses unsupported schema version {found}",
                path.display()
            ),
            Self::InvalidRetentionLimit { requested } => write!(
                formatter,
                "retention limit {requested} is outside the supported range {MIN_RETENTION_LIMIT}..={MAX_RETENTION_LIMIT}"
            ),
            Self::InvalidContractId { id } => write!(formatter, "invalid contract id {id:?}"),
            Self::MissingContract { id } => write!(formatter, "contract {id:?} does not exist"),
            Self::DocumentPathMismatch { path, id } => write!(
                formatter,
                "contract document {id:?} was stored at unexpected path {}",
                path.display()
            ),
            Self::CannotEvictActiveContract => {
                write!(formatter, "cannot evict the currently active contract")
            }
            Self::IdAllocationExhausted => write!(formatter, "could not allocate a unique contract id"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Encode { source, .. } | Self::Decode { source, .. } => Some(source),
            Self::InvalidContract { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chaos_core::contract::{GitOp, GitOpSet, OpClass, OpSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestWorkspace {
        root: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("tractus-store-test-{}-{sequence}", process::id()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn contract(task: impl Into<String>) -> ContractSpec {
        let mut allowed_ops = OpSet::empty();
        allowed_ops.insert(OpClass::Read);
        allowed_ops.insert(OpClass::Edit);
        allowed_ops.insert(OpClass::Test);

        let mut git_ops = GitOpSet::empty();
        git_ops.insert(GitOp::Status);
        git_ops.insert(GitOp::Diff);

        ContractSpec {
            task: task.into(),
            allowed_paths: vec!["src/**".to_owned(), "tests/**".to_owned()],
            allowed_ops,
            deps_may_change: false,
            git_ops,
            network: false,
        }
    }

    #[test]
    fn creates_and_reopens_the_active_document() {
        let workspace = TestWorkspace::new();
        let store = ContractStore::open(&workspace.root).unwrap();
        let created = store.create(contract("fix the flaky test")).unwrap();

        let persisted = fs::read_to_string(store.document_path(&created.id).unwrap()).unwrap();
        assert!(persisted.contains("\"contract\""));
        assert!(persisted.contains("\"allowed_paths\""));

        let reopened = ContractStore::open(&workspace.root).unwrap();
        let active = reopened.load_active().unwrap().unwrap();
        assert_eq!(active.id, created.id);
        assert_eq!(active.spec(), created.spec());
        assert!(active.last_used_at_ms > created.last_used_at_ms);
    }

    #[test]
    fn list_is_most_recently_used_first() {
        let workspace = TestWorkspace::new();
        let store = ContractStore::open(&workspace.root).unwrap();
        let first = store.create(contract("first")).unwrap();
        let second = store.create(contract("second")).unwrap();

        store.activate(&first.id).unwrap();
        let documents = store.list().unwrap();

        assert_eq!(documents[0].id, first.id);
        assert_eq!(documents[1].id, second.id);
    }

    #[test]
    fn lru_eviction_keeps_a_reactivated_document() {
        let workspace = TestWorkspace::new();
        let store = ContractStore::open(&workspace.root).unwrap();
        store.set_retention_limit(MIN_RETENTION_LIMIT).unwrap();

        let mut ids = Vec::new();
        for number in 0..MIN_RETENTION_LIMIT {
            ids.push(
                store
                    .create(contract(format!("contract {number}")))
                    .unwrap()
                    .id,
            );
        }
        store.activate(&ids[0]).unwrap();
        let newest = store.create(contract("newest")).unwrap();

        assert!(store.get(&ids[0]).is_ok());
        assert!(matches!(
            store.get(&ids[1]),
            Err(StoreError::MissingContract { .. })
        ));
        assert_eq!(store.list().unwrap().len(), MIN_RETENTION_LIMIT);
        assert_eq!(store.load_active().unwrap().unwrap().id, newest.id);
    }

    #[test]
    fn retention_limit_is_bounded() {
        let workspace = TestWorkspace::new();
        let store = ContractStore::open(&workspace.root).unwrap();

        assert_eq!(store.retention_limit().unwrap(), DEFAULT_RETENTION_LIMIT);
        assert!(matches!(
            store.set_retention_limit(MIN_RETENTION_LIMIT - 1),
            Err(StoreError::InvalidRetentionLimit { .. })
        ));
        assert!(matches!(
            store.set_retention_limit(MAX_RETENTION_LIMIT + 1),
            Err(StoreError::InvalidRetentionLimit { .. })
        ));
    }

    #[test]
    fn corrupt_state_fails_closed() {
        let workspace = TestWorkspace::new();
        let store = ContractStore::open(&workspace.root).unwrap();
        fs::write(store.state_path(), "not valid JSON").unwrap();

        assert!(matches!(
            store.load_active(),
            Err(StoreError::Decode { .. })
        ));
    }

    #[test]
    fn unsafe_contract_ids_cannot_escape_the_store() {
        let workspace = TestWorkspace::new();
        let store = ContractStore::open(&workspace.root).unwrap();

        assert!(matches!(
            store.get("../outside"),
            Err(StoreError::InvalidContractId { .. })
        ));
    }
}
