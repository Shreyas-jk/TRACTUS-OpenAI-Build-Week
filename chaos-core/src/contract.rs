use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

pub struct Contract {
    pub task: String,
    pub allowed_paths: GlobSet,
    pub allowed_ops: OpSet,
    pub deps_may_change: bool,
    pub git_ops: GitOpSet,
    pub network: bool,
    pub workspace_root: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum OpClass {
    Read = 0,
    Edit = 1,
    Create = 2,
    Delete = 3,
    Test = 4,
    Build = 5,
    #[default]
    Run = 6,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum GitOp {
    Status = 0,
    Diff = 1,
    Log = 2,
    Add = 3,
    Commit = 4,
    Checkout = 5,
    Push = 6,
    ForcePush = 7,
    ResetHard = 8,
    #[default]
    Clean = 9,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct OpSet(u8);

impl OpSet {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, op: OpClass) -> bool {
        self.0 & (1 << op as u8) != 0
    }

    pub fn insert(&mut self, op: OpClass) {
        self.0 |= 1 << op as u8;
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct GitOpSet(u16);

impl GitOpSet {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, op: GitOp) -> bool {
        self.0 & (1u16 << op as u8) != 0
    }

    pub fn insert(&mut self, op: GitOp) {
        self.0 |= 1u16 << op as u8;
    }
}

/// What a command will do, either declared by the classifier or observed by the twin.
#[derive(Debug, Default)]
pub struct Effects {
    pub family: Option<String>,
    pub reads: Vec<PathBuf>,
    pub writes: Vec<PathBuf>,
    pub deletes: Vec<PathBuf>,
    pub dep_change: Option<DepChange>,
    pub git: Option<GitOp>,
    pub network: bool,
    pub privileged: bool,
    pub recursive: bool,
    pub forced: bool,
    pub op: OpClass,
}

#[derive(Debug)]
pub struct DepChange {
    pub manifest: PathBuf,
    pub summary: String,
}

#[derive(Debug)]
pub enum Verdict {
    InScope,
    ScopeViolation(Vec<ProofTrace>),
    NeedsHuman(Reason),
    Loop { n: u32, signature: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reason {
    Opaque,
    TwinTimeout,
    UnresolvedVar,
    ContractAmbiguous,
}

#[derive(Debug)]
pub struct ProofTrace {
    pub rule: &'static str,
    pub clause: String,
    pub effect: String,
    pub rendered: String,
}

/// Serde-friendly contract representation received from the control plane.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContractSpec {
    pub task: String,
    pub allowed_paths: Vec<String>,
    pub allowed_ops: OpSet,
    pub deps_may_change: bool,
    pub git_ops: GitOpSet,
    pub network: bool,
}

impl ContractSpec {
    pub fn compile(self, workspace_root: impl AsRef<Path>) -> Result<Contract, ContractError> {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let mut allowed_paths = GlobSetBuilder::new();

        for pattern in &self.allowed_paths {
            let rooted_pattern = workspace_root.join(pattern);
            let rooted_pattern = rooted_pattern.to_string_lossy();
            let glob = Glob::new(&rooted_pattern).map_err(|error| ContractError::InvalidGlob {
                pattern: pattern.clone(),
                message: error.to_string(),
            })?;
            allowed_paths.add(glob);

            if let Some(directory) = pattern.strip_suffix("/**") {
                let directory = workspace_root.join(directory);
                let directory = directory.to_string_lossy();
                let glob = Glob::new(&directory).map_err(|error| ContractError::InvalidGlob {
                    pattern: pattern.clone(),
                    message: error.to_string(),
                })?;
                allowed_paths.add(glob);
            }
        }

        let allowed_paths = allowed_paths.build().map_err(|error| ContractError::BuildGlobSet {
            message: error.to_string(),
        })?;

        Ok(Contract {
            task: self.task,
            allowed_paths,
            allowed_ops: self.allowed_ops,
            deps_may_change: self.deps_may_change,
            git_ops: self.git_ops,
            network: self.network,
            workspace_root,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    InvalidGlob { pattern: String, message: String },
    BuildGlobSet { message: String },
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGlob { pattern, message } => {
                write!(formatter, "invalid allowed-path glob {pattern:?}: {message}")
            }
            Self::BuildGlobSet { message } => write!(formatter, "failed to build glob set: {message}"),
        }
    }
}

impl Error for ContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ContractSpec {
        let mut allowed_ops = OpSet::empty();
        allowed_ops.insert(OpClass::Read);
        allowed_ops.insert(OpClass::Edit);

        let mut git_ops = GitOpSet::empty();
        git_ops.insert(GitOp::Status);
        git_ops.insert(GitOp::Diff);

        ContractSpec {
            task: "fix the failing API test".to_owned(),
            allowed_paths: vec!["tests/**".to_owned(), "src/api/**".to_owned()],
            allowed_ops,
            deps_may_change: false,
            git_ops,
            network: false,
        }
    }

    #[test]
    fn contract_spec_json_round_trip() {
        let expected = spec();
        let json = serde_json::to_string(&expected).unwrap();
        let actual: ContractSpec = serde_json::from_str(&json).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn compile_accepts_valid_globs() {
        let workspace_root = PathBuf::from("/workspace/project");
        let contract = spec().compile(&workspace_root).unwrap();

        assert!(contract
            .allowed_paths
            .is_match(workspace_root.join("tests/api_test.rs")));
        assert!(contract
            .allowed_paths
            .is_match(workspace_root.join("src/api/handler.rs")));
        assert!(!contract
            .allowed_paths
            .is_match(workspace_root.join("Cargo.toml")));
    }

    #[test]
    fn compile_rejects_invalid_globs() {
        let mut invalid = spec();
        invalid.allowed_paths = vec!["[unterminated".to_owned()];

        assert!(matches!(
            invalid.compile("/workspace/project"),
            Err(ContractError::InvalidGlob { .. })
        ));
    }

    #[test]
    fn directory_glob_also_matches_its_root() {
        let mut artifact_spec = spec();
        artifact_spec.allowed_paths = vec!["target/**".to_owned()];
        let contract = artifact_spec.compile("/workspace/project").unwrap();

        assert!(contract
            .allowed_paths
            .is_match("/workspace/project/target"));
    }

    #[test]
    fn op_set_contains_inserted_operations() {
        let mut operations = OpSet::empty();

        assert!(!operations.contains(OpClass::Edit));
        operations.insert(OpClass::Edit);
        assert!(operations.contains(OpClass::Edit));
        assert!(!operations.contains(OpClass::Delete));
    }
}
