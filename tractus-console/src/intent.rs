//! Structured intent extraction, defensive normalization, and the toggle card.
//!
//! Mirrors the former Python `intent.py` + `toggle_card`. The console keeps a
//! *named* contract representation for structured extraction and the UI, then
//! converts to `tractusd`'s bitset wire format at the daemon boundary. The bit
//! layout must match `tractus-core`'s `OpSet`/`GitOpSet` (documented as `1 << n`
//! in declaration order); [`daemon_wire`](ContractSpec::daemon_wire) is the only
//! place that encoding lives.

use crate::ConsoleError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Read,
    Edit,
    Create,
    Delete,
    Test,
    Build,
    Run,
}

impl Operation {
    pub const ALL: [Operation; 7] = [
        Operation::Read,
        Operation::Edit,
        Operation::Create,
        Operation::Delete,
        Operation::Test,
        Operation::Build,
        Operation::Run,
    ];

    fn bit(self) -> u32 {
        1 << (self as u32)
    }

    fn label(self) -> &'static str {
        match self {
            Operation::Read => "read",
            Operation::Edit => "edit",
            Operation::Create => "create",
            Operation::Delete => "delete",
            Operation::Test => "test",
            Operation::Build => "build",
            Operation::Run => "run",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitOperation {
    Status,
    Diff,
    Log,
    Add,
    Commit,
    Checkout,
    Push,
    ForcePush,
    ResetHard,
    Clean,
}

impl GitOperation {
    pub const ALL: [GitOperation; 10] = [
        GitOperation::Status,
        GitOperation::Diff,
        GitOperation::Log,
        GitOperation::Add,
        GitOperation::Commit,
        GitOperation::Checkout,
        GitOperation::Push,
        GitOperation::ForcePush,
        GitOperation::ResetHard,
        GitOperation::Clean,
    ];

    fn bit(self) -> u32 {
        1 << (self as u32)
    }

    fn label(self) -> &'static str {
        match self {
            GitOperation::Status => "status",
            GitOperation::Diff => "diff",
            GitOperation::Log => "log",
            GitOperation::Add => "add",
            GitOperation::Commit => "commit",
            GitOperation::Checkout => "checkout",
            GitOperation::Push => "push",
            GitOperation::ForcePush => "force_push",
            GitOperation::ResetHard => "reset_hard",
            GitOperation::Clean => "clean",
        }
    }
}

/// Build-artifact globs the control plane always grants; the model is told never
/// to emit them so they are appended exactly once after extraction.
pub const ARTIFACT_PATHS: [&str; 4] = [
    "target/**",
    "node_modules/**",
    "**/__pycache__/**",
    ".venv/**",
];

const SYSTEM_PROMPT: &str = "You translate a user's coding task into a least-privilege Tractus Intent Contract.\n\nReturn only the structured contract. Apply these rules exactly:\n- NEVER emit build-artifact directories (`target/`, `node_modules/`, `**/__pycache__/`, `.venv/`) in `allowed_paths`. The control plane owns those paths and adds them after extraction.\n- Any code-editing task implies the `test` and `build` op grants.\n- `run` stays a separate, explicit grant: never grant it unless the user's request explicitly needs it.\n- Network access is never granted unless the user's request explicitly needs it.\n- `deps_may_change` defaults to false; set it true only when the request explicitly asks to add, remove, update, or upgrade dependencies.\n- Scope `allowed_paths` to the specific files or directories the task concerns. Never use a whole-repository glob such as `**`, `./**`, `.`, or `*`; if the task touches several areas, list the concrete directories rather than widening to the repo root.
- Keep every other path, operation, and Git permission narrowly scoped to the request.";

const RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";

/// Human-readable mirror of `tractus-core`'s `ContractSpec`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ContractSpec {
    pub task: String,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub allowed_ops: Vec<Operation>,
    #[serde(default)]
    pub deps_may_change: bool,
    #[serde(default)]
    pub git_ops: Vec<GitOperation>,
    #[serde(default)]
    pub network: bool,
}

impl ContractSpec {
    /// Serialize to the exact bitset payload `tractusd` expects.
    pub fn daemon_wire(&self) -> Value {
        let allowed_ops = self
            .allowed_ops
            .iter()
            .fold(0u32, |bits, operation| bits | operation.bit());
        let git_ops = self
            .git_ops
            .iter()
            .fold(0u32, |bits, operation| bits | operation.bit());
        json!({
            "task": self.task,
            "allowed_paths": self.allowed_paths,
            "allowed_ops": allowed_ops,
            "deps_may_change": self.deps_may_change,
            "git_ops": git_ops,
            "network": self.network,
        })
    }

    fn has_op(&self, operation: Operation) -> bool {
        self.allowed_ops.contains(&operation)
    }
}

/// Call the Responses API with structured outputs, then enforce the defaults.
pub async fn extract_intent(
    http: &reqwest::Client,
    request: &str,
) -> Result<ContractSpec, ConsoleError> {
    let api_key = env::var("OPENAI_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .ok_or(ConsoleError::NoCredentials)?;
    let model = env::var("INTENT_MODEL").unwrap_or_else(|_| "gpt-5.6-sol".to_owned());

    let body = json!({
        "model": model,
        "instructions": SYSTEM_PROMPT,
        "input": request,
        "text": { "format": {
            "type": "json_schema",
            "name": "intent_contract",
            "strict": true,
            "schema": contract_schema(),
        }},
    });

    let response = http
        .post(RESPONSES_ENDPOINT)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(ConsoleError::Upstream)?;
    if !response.status().is_success() {
        return Err(ConsoleError::UpstreamStatus(response.status().as_u16()));
    }
    let payload: Value = response.json().await.map_err(ConsoleError::Upstream)?;
    let text = output_text(&payload).ok_or(ConsoleError::EmptyCompletion)?;
    let parsed: ContractSpec =
        serde_json::from_str(&text).map_err(|_| ConsoleError::EmptyCompletion)?;
    Ok(normalize_contract(parsed))
}

fn contract_schema() -> Value {
    let operations: Vec<&str> = Operation::ALL.iter().map(|op| op.label()).collect();
    let git_operations: Vec<&str> = GitOperation::ALL.iter().map(|op| op.label()).collect();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["task", "allowed_paths", "allowed_ops", "deps_may_change", "git_ops", "network"],
        "properties": {
            "task": { "type": "string" },
            "allowed_paths": { "type": "array", "items": { "type": "string" } },
            "allowed_ops": { "type": "array", "items": { "type": "string", "enum": operations } },
            "deps_may_change": { "type": "boolean" },
            "git_ops": { "type": "array", "items": { "type": "string", "enum": git_operations } },
            "network": { "type": "boolean" },
        },
    })
}

/// Concatenate every `output_text` fragment the Responses API returned.
fn output_text(payload: &Value) -> Option<String> {
    if let Some(text) = payload.get("output_text").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return Some(text.to_owned());
        }
    }
    let mut collected = String::new();
    for item in payload.get("output")?.as_array()? {
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in content {
            if part.get("type").and_then(Value::as_str) == Some("output_text") {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    collected.push_str(text);
                }
            }
        }
    }
    (!collected.trim().is_empty()).then_some(collected)
}

/// Apply the deterministic post-processing the model is told to leave out:
/// append build-artifact paths, imply `test`/`build` for editing tasks, and
/// canonicalize path globs.
///
/// Scope grants (`deps_may_change`, `network`, the `run` op) come from the
/// model — which is prompted to be conservative — and the user's confirmation of
/// the toggle card. We deliberately do NOT strip a granted permission based on
/// request-text keyword matching: that heuristic produced false negatives (a
/// request to "bump the tokio version" failed the dependency keywords and had
/// its grant silently removed, then blocked the very work the user asked for).
pub fn normalize_contract(contract: ContractSpec) -> ContractSpec {
    let mut allowed_paths = contract.allowed_paths.clone();
    allowed_paths.extend(ARTIFACT_PATHS.iter().map(|path| (*path).to_owned()));
    let allowed_paths = deduplicated(&allowed_paths);

    let edits = contract.has_op(Operation::Edit)
        || contract.has_op(Operation::Create)
        || contract.has_op(Operation::Delete);
    let allowed_ops = Operation::ALL
        .into_iter()
        .filter(|operation| {
            if edits && matches!(operation, Operation::Test | Operation::Build) {
                return true;
            }
            contract.has_op(*operation)
        })
        .collect();

    let git_ops = GitOperation::ALL
        .into_iter()
        .filter(|operation| contract.git_ops.contains(operation))
        .collect();

    ContractSpec {
        task: contract.task,
        allowed_paths,
        allowed_ops,
        deps_may_change: contract.deps_may_change,
        git_ops,
        network: contract.network,
    }
}

/// Canonicalize each path grant to one glob before deduplicating.
fn deduplicated(values: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values {
        let Some(path) = normalize_path_glob(value) else {
            continue;
        };
        if !normalized.contains(&path) {
            normalized.push(path);
        }
    }
    normalized
}

/// Turn a raw path grant into a glob. Directories become recursive (`dir/**`),
/// but a file grant stays literal so the file itself matches — appending `/**`
/// to a file would only match a phantom subtree and silently block editing it.
fn normalize_path_glob(raw: &str) -> Option<String> {
    let input = raw.trim();
    let had_trailing_slash = input.ends_with('/');
    let path = input.trim_start_matches('/').trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    if path.ends_with("/**") {
        return Some(path.to_owned());
    }
    if path.contains(['*', '?', '[']) {
        // Existing glob: only make an anchored `**/…` prefix recursive.
        return Some(if path.starts_with("**/") {
            format!("{path}/**")
        } else {
            path.to_owned()
        });
    }
    if had_trailing_slash || !looks_like_file(path) {
        Some(format!("{path}/**"))
    } else {
        Some(path.to_owned())
    }
}

/// A trailing path segment is treated as a file when it carries an interior
/// extension dot (`api_test.rs`), or is a well-known extensionless file or
/// dotfile (`Makefile`, `.gitignore`), but not a leading-dot directory (`.venv`).
fn looks_like_file(path: &str) -> bool {
    let last = path.rsplit('/').next().unwrap_or(path);
    if is_known_file_name(last) {
        return true;
    }
    match last.rfind('.') {
        Some(index) => index > 0 && index < last.len() - 1,
        None => false,
    }
}

/// Common extensionless files and no-extension dotfiles the interior-dot
/// heuristic would otherwise mistake for directories.
fn is_known_file_name(name: &str) -> bool {
    const KNOWN: &[&str] = &[
        "Makefile",
        "makefile",
        "GNUmakefile",
        "Dockerfile",
        "Containerfile",
        "Rakefile",
        "Gemfile",
        "Procfile",
        "Jenkinsfile",
        "Vagrantfile",
        "Brewfile",
        "LICENSE",
        "LICENCE",
        "README",
        "CHANGELOG",
        "NOTICE",
        "AUTHORS",
        "COPYING",
        "CODEOWNERS",
        ".gitignore",
        ".gitattributes",
        ".dockerignore",
        ".env",
        ".editorconfig",
        ".npmrc",
        ".nvmrc",
        ".eslintrc",
        ".prettierrc",
        ".babelrc",
    ];
    KNOWN.contains(&name)
}

/// Render the user-facing, plain-language representation of a contract.
pub fn toggle_card(contract: &ContractSpec) -> Value {
    let implied_test_build = contract.has_op(Operation::Edit);
    let path_clauses: Vec<Value> = contract
        .allowed_paths
        .iter()
        .map(|path| {
            json!({
                "kind": "path",
                "value": path,
                "enabled": true,
                "de_emphasized": ARTIFACT_PATHS.contains(&path.as_str()),
            })
        })
        .collect();
    let operation_clauses: Vec<Value> = Operation::ALL
        .into_iter()
        .map(|operation| {
            let implied =
                implied_test_build && matches!(operation, Operation::Test | Operation::Build);
            json!({
                "kind": "operation",
                "value": operation.label(),
                "enabled": contract.has_op(operation),
                "de_emphasized": implied,
            })
        })
        .collect();
    let git_clauses: Vec<Value> = GitOperation::ALL
        .into_iter()
        .map(|operation| {
            json!({
                "kind": "git",
                "value": operation.label(),
                "enabled": contract.git_ops.contains(&operation),
                "de_emphasized": false,
            })
        })
        .collect();

    json!({
        "contract": contract,
        "groups": [
            { "id": "paths", "label": "May edit files in", "clauses": path_clauses },
            { "id": "operations", "label": "May perform", "clauses": operation_clauses },
            { "id": "dependencies", "label": "May change dependencies", "clauses": [{
                "kind": "deps_may_change",
                "value": contract.deps_may_change,
                "enabled": contract.deps_may_change,
                "de_emphasized": false,
            }] },
            { "id": "git", "label": "Git permissions", "clauses": git_clauses },
            { "id": "network", "label": "May access network", "clauses": [{
                "kind": "network",
                "value": contract.network,
                "enabled": contract.network,
                "de_emphasized": false,
            }] },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(allowed_ops: Vec<Operation>, network: bool, deps: bool) -> ContractSpec {
        ContractSpec {
            task: "fix the flaky API test".to_owned(),
            allowed_paths: vec!["src".to_owned(), "tests/api/".to_owned()],
            allowed_ops,
            deps_may_change: deps,
            git_ops: vec![GitOperation::Status],
            network,
        }
    }

    #[test]
    fn editing_implies_test_and_build_and_appends_artifacts() {
        let normalized = normalize_contract(spec(vec![Operation::Edit], false, false));
        assert!(normalized.allowed_ops.contains(&Operation::Test));
        assert!(normalized.allowed_ops.contains(&Operation::Build));
        assert_eq!(
            normalized.allowed_paths,
            vec![
                "src/**",
                "tests/api/**",
                "target/**",
                "node_modules/**",
                "**/__pycache__/**",
                ".venv/**",
            ]
        );
    }

    #[test]
    fn model_grants_pass_through_without_keyword_stripping() {
        // The model (prompted to be conservative) and the user's confirmation own
        // scope; normalization no longer strips grants on request phrasing, which
        // used to false-block valid work like "bump the tokio version".
        let granted = spec(vec![Operation::Edit, Operation::Run], true, true);
        let normalized = normalize_contract(granted);
        assert!(normalized.allowed_ops.contains(&Operation::Run));
        assert!(normalized.network);
        assert!(normalized.deps_may_change);

        // A contract the model kept minimal stays minimal.
        let minimal = normalize_contract(spec(vec![Operation::Edit], false, false));
        assert!(!minimal.allowed_ops.contains(&Operation::Run));
        assert!(!minimal.network);
        assert!(!minimal.deps_may_change);
    }

    #[test]
    fn file_targets_stay_literal_while_directories_recurse() {
        // A file grant must match the file itself, not a phantom `file/**` subtree.
        assert_eq!(
            normalize_path_glob("tests/api_test.rs").as_deref(),
            Some("tests/api_test.rs")
        );
        assert_eq!(normalize_path_glob("src").as_deref(), Some("src/**"));
        assert_eq!(normalize_path_glob("src/").as_deref(), Some("src/**"));
        // Extensionless files and dotfiles stay literal; .venv is a known dir.
        assert_eq!(normalize_path_glob("Makefile").as_deref(), Some("Makefile"));
        assert_eq!(
            normalize_path_glob("Dockerfile").as_deref(),
            Some("Dockerfile")
        );
        assert_eq!(
            normalize_path_glob(".gitignore").as_deref(),
            Some(".gitignore")
        );
        assert_eq!(normalize_path_glob(".venv").as_deref(), Some(".venv/**"));
        assert_eq!(normalize_path_glob("src/*.rs").as_deref(), Some("src/*.rs"));
        assert_eq!(
            normalize_path_glob("target/**").as_deref(),
            Some("target/**")
        );
        assert_eq!(
            normalize_path_glob("**/__pycache__").as_deref(),
            Some("**/__pycache__/**")
        );
    }

    #[test]
    fn daemon_wire_encodes_the_expected_bitsets() {
        let contract = ContractSpec {
            task: "t".to_owned(),
            allowed_paths: vec!["src/**".to_owned()],
            allowed_ops: vec![Operation::Read, Operation::Edit, Operation::Test],
            deps_may_change: false,
            git_ops: vec![GitOperation::Status, GitOperation::Diff],
            network: false,
        };
        let wire = contract.daemon_wire();
        // Read|Edit|Test = 1 | 2 | 16 = 19; Status|Diff = 1 | 2 = 3.
        assert_eq!(wire["allowed_ops"], json!(19));
        assert_eq!(wire["git_ops"], json!(3));
    }

    #[test]
    fn toggle_card_marks_implied_and_artifact_clauses() {
        let normalized = normalize_contract(spec(vec![Operation::Edit], false, false));
        let card = toggle_card(&normalized);
        let groups = card["groups"].as_array().unwrap();
        let operations = &groups[1]["clauses"];
        let build = operations
            .as_array()
            .unwrap()
            .iter()
            .find(|clause| clause["value"] == "build")
            .unwrap();
        assert_eq!(build["enabled"], json!(true));
        assert_eq!(build["de_emphasized"], json!(true));
    }
}
