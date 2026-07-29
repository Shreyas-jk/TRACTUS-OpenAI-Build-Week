//! Best-effort, advisory explanations for blocked Tractus events.
//!
//! Advisory only: every API failure (missing key, timeout, non-200) falls back
//! to a deterministic one-line summary derived from the proof trace, so the
//! event bridge never blocks or fails on the explainer.

use serde_json::{json, Value};
use std::env;
use std::time::Duration;
use tokio::time::timeout;

const EXPLAIN_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const INSTRUCTIONS: &str = "Explain a Tractus block in exactly one plain-English sentence.\nIt is advisory only: describe the contract mismatch, do not recommend bypassing enforcement.";

/// Return one sentence, using a deterministic fallback on every API failure.
pub async fn explain_divergence(
    http: &reqwest::Client,
    task: &str,
    command: &str,
    evidence: &Value,
) -> String {
    let fallback = format!("Blocked: {}.", first_proof_clause(evidence));
    match timeout(
        EXPLAIN_TIMEOUT,
        request_sentence(http, task, command, evidence),
    )
    .await
    {
        Ok(Some(sentence)) => sentence,
        _ => fallback,
    }
}

async fn request_sentence(
    http: &reqwest::Client,
    task: &str,
    command: &str,
    evidence: &Value,
) -> Option<String> {
    let api_key = env::var("OPENAI_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())?;
    let model = env::var("EXPLAIN_MODEL").unwrap_or_else(|_| "gpt-5.6-luna".to_owned());
    let input = json!({
        "contract_task": task,
        "command": command,
        "proofs_or_twin_diff": evidence,
    })
    .to_string();

    let response = http
        .post(RESPONSES_ENDPOINT)
        .bearer_auth(api_key)
        .json(&json!({ "model": model, "instructions": INSTRUCTIONS, "input": input }))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload: Value = response.json().await.ok()?;
    let text = output_text(&payload)?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| one_sentence(trimmed))
}

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

fn first_proof_clause(evidence: &Value) -> String {
    match evidence {
        Value::Object(map) => map
            .get("clause")
            .or_else(|| map.get("rendered"))
            .and_then(Value::as_str)
            .unwrap_or("scope contract violated")
            .to_owned(),
        Value::Array(items) => items
            .first()
            .map(first_proof_clause)
            .unwrap_or_else(|| "scope contract violated".to_owned()),
        Value::String(text) if !text.is_empty() => text.clone(),
        _ => "scope contract violated".to_owned(),
    }
}

fn one_sentence(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut clause = first.to_owned();
    for delimiter in [". ", "! ", "? "] {
        if let Some(index) = first.find(delimiter) {
            clause = format!("{}{}", &first[..index], &delimiter[..1]);
            break;
        }
    }
    if clause.ends_with(['.', '!', '?']) {
        clause
    } else {
        format!("{clause}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_reads_the_first_proof_clause() {
        let evidence =
            json!([{ "rule": "R-DEP-01", "clause": "contract forbids dependency changes" }]);
        assert_eq!(
            first_proof_clause(&evidence),
            "contract forbids dependency changes"
        );
        assert_eq!(first_proof_clause(&json!({})), "scope contract violated");
        assert_eq!(first_proof_clause(&json!("raw reason")), "raw reason");
    }

    #[test]
    fn one_sentence_trims_to_the_first_clause() {
        assert_eq!(
            one_sentence("This is blocked. Extra detail follows."),
            "This is blocked."
        );
        assert_eq!(
            one_sentence("No trailing punctuation"),
            "No trailing punctuation."
        );
    }
}
