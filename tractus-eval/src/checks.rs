//! The versioned dataset and the deterministic (non-LLM) checks.
//!
//! These assertions are cheap, reproducible guards on the extracted contract:
//! they catch obvious regressions (a dependency grant that should be off, a
//! missing path) without an LLM. The nuanced "is this least-privilege and
//! faithful" judgment is left to the LLM judge in [`crate::judge`].

use serde::Deserialize;
use serde_json::Value;
use tractus_console::intent::{ContractSpec, Operation};

/// One evaluation case: a request plus the deterministic properties its
/// extracted contract must satisfy.
#[derive(Clone, Debug, Deserialize)]
pub struct Case {
    pub name: String,
    pub request: String,
    #[serde(default)]
    pub expect_deps: Option<bool>,
    #[serde(default)]
    pub expect_network: Option<bool>,
    #[serde(default)]
    pub expect_run: Option<bool>,
    /// Substrings that must appear in at least one `allowed_paths` entry.
    #[serde(default)]
    pub must_include_paths: Vec<String>,
    /// Git operations (snake_case, e.g. "commit") that must be granted.
    #[serde(default)]
    pub must_include_git: Vec<String>,
    /// Guidance shown to the LLM judge; never asserted deterministically.
    #[serde(default)]
    pub rubric_notes: String,
}

/// The dataset, embedded at compile time so the eval binary is self-contained.
pub fn load_cases() -> Vec<Case> {
    serde_json::from_str(include_str!("../cases.json")).expect("cases.json is valid")
}

#[derive(Clone, Debug)]
pub struct CheckResult {
    pub label: String,
    pub passed: bool,
    pub detail: String,
}

pub fn deterministic_checks(case: &Case, contract: &ContractSpec) -> Vec<CheckResult> {
    let mut results = Vec::new();

    if let Some(expected) = case.expect_deps {
        results.push(bool_check(
            "deps_may_change",
            expected,
            contract.deps_may_change,
        ));
    }
    if let Some(expected) = case.expect_network {
        results.push(bool_check("network", expected, contract.network));
    }
    if let Some(expected) = case.expect_run {
        let granted = contract.allowed_ops.contains(&Operation::Run);
        results.push(bool_check("op:run", expected, granted));
    }
    for needle in &case.must_include_paths {
        let found = contract
            .allowed_paths
            .iter()
            .any(|path| path.contains(needle.as_str()));
        results.push(CheckResult {
            label: format!("path~{needle}"),
            passed: found,
            detail: if found {
                "present".to_owned()
            } else {
                format!("no allowed_path contains {needle:?}")
            },
        });
    }
    for git in &case.must_include_git {
        let found = contract
            .git_ops
            .iter()
            .any(|op| git_label(op).as_deref() == Some(git.as_str()));
        results.push(CheckResult {
            label: format!("git:{git}"),
            passed: found,
            detail: if found {
                "granted".to_owned()
            } else {
                "not granted".to_owned()
            },
        });
    }
    results
}

pub fn all_passed(results: &[CheckResult]) -> bool {
    results.iter().all(|result| result.passed)
}

fn bool_check(label: &str, expected: bool, actual: bool) -> CheckResult {
    CheckResult {
        label: label.to_owned(),
        passed: expected == actual,
        detail: format!("expected {expected}, got {actual}"),
    }
}

/// Serialize a GitOperation to its snake_case wire label without reaching into
/// tractus-console internals.
fn git_label<T: serde::Serialize>(op: &T) -> Option<String> {
    match serde_json::to_value(op) {
        Ok(Value::String(label)) => Some(label),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tractus_console::intent::{ContractSpec, GitOperation, Operation};

    fn contract() -> ContractSpec {
        ContractSpec {
            task: "t".to_owned(),
            allowed_paths: vec!["src/parse.rs".to_owned(), "target/**".to_owned()],
            allowed_ops: vec![Operation::Read, Operation::Edit],
            deps_may_change: false,
            git_ops: vec![GitOperation::Status, GitOperation::Commit],
            network: false,
        }
    }

    #[test]
    fn passing_case_has_all_green_checks() {
        let case = Case {
            name: "c".to_owned(),
            request: "r".to_owned(),
            expect_deps: Some(false),
            expect_network: Some(false),
            expect_run: Some(false),
            must_include_paths: vec!["src/parse".to_owned()],
            must_include_git: vec!["commit".to_owned()],
            rubric_notes: String::new(),
        };
        assert!(all_passed(&deterministic_checks(&case, &contract())));
    }

    #[test]
    fn wrong_expectations_fail_the_relevant_checks() {
        let case = Case {
            name: "c".to_owned(),
            request: "r".to_owned(),
            expect_deps: Some(true), // contract has false
            expect_network: None,
            expect_run: Some(true),                       // contract lacks run
            must_include_paths: vec!["tests".to_owned()], // not in contract
            must_include_git: vec!["push".to_owned()],    // not granted
            rubric_notes: String::new(),
        };
        let results = deterministic_checks(&case, &contract());
        assert!(!all_passed(&results));
        assert_eq!(results.iter().filter(|r| !r.passed).count(), 4);
    }

    #[test]
    fn dataset_parses_and_is_nonempty() {
        let cases = load_cases();
        assert!(!cases.is_empty());
        assert!(cases.iter().all(|case| !case.request.is_empty()));
    }
}
