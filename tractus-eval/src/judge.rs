//! The LLM-as-judge: grades an extracted contract against the request on a
//! rubric of atomic criteria, using structured (JSON-schema) output.
//!
//! Per LLM-as-judge best practice, each criterion is scored independently 1–5
//! with a one-sentence rationale, and the schema is strict to curb verbosity and
//! formatting drift. The judge is advisory tooling only — it never touches the
//! deterministic enforcement path.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use tractus_console::intent::ContractSpec;

const RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";

const INSTRUCTIONS: &str = "You are a strict evaluator of Tractus Intent Contracts. An Intent Contract is a least-privilege scope extracted from a developer's natural-language request, with fields: allowed_paths (globs), allowed_ops (read/edit/create/delete/test/build/run), deps_may_change, git_ops, network.

Grade how well the contract captures the request. Score each criterion 1-5 (5 = ideal), with a one-sentence rationale:
- faithfulness: the contract covers what the request asks for; nothing required is missing.
- least_privilege: no grants beyond what the request needs. In particular network, deps_may_change, the run op, and write-level git ops must NOT be granted unless the request implies them.
- path_scope: allowed_paths govern which files the agent may WRITE or DELETE — reads and running a program are NOT path-gated by Tractus. Grade whether the paths cover exactly the files the task modifies, neither too broad nor too narrow. A task that only runs or only reads legitimately needs few or no path grants; do not penalize a minimal or empty path set in that case.

The system always appends build-artifact globs (target/**, node_modules/**, **/__pycache__/**, .venv/**); ignore those when judging breadth. Editing implies the test and build ops; do not penalize their presence. When `evaluation_notes` are provided, treat them as guidance on what ideal scoping looks like for this case. Be conservative: reserve 5 for genuinely ideal scoping.";

const CRITERIA: [&str; 3] = ["faithfulness", "least_privilege", "path_scope"];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Criterion {
    pub name: String,
    pub score: u8,
    pub rationale: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Judgment {
    pub criteria: Vec<Criterion>,
}

impl Judgment {
    /// The weakest criterion score — the gate compares this to the threshold.
    pub fn min_score(&self) -> u8 {
        self.criteria.iter().map(|c| c.score).min().unwrap_or(0)
    }
}

pub async fn judge_contract(
    http: &reqwest::Client,
    request: &str,
    contract: &ContractSpec,
    evaluation_notes: &str,
) -> Result<Judgment, String> {
    let api_key = env::var("OPENAI_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| "OPENAI_API_KEY not set".to_owned())?;
    let model = env::var("JUDGE_MODEL").unwrap_or_else(|_| "gpt-5.6-sol".to_owned());

    let mut input_object = json!({ "request": request, "contract": contract });
    if !evaluation_notes.trim().is_empty() {
        input_object["evaluation_notes"] = json!(evaluation_notes);
    }
    let input = input_object.to_string();
    let body = json!({
        "model": model,
        "instructions": INSTRUCTIONS,
        "input": input,
        "text": { "format": {
            "type": "json_schema",
            "name": "contract_judgment",
            "strict": true,
            "schema": judgment_schema(),
        }},
    });

    let response = http
        .post(RESPONSES_ENDPOINT)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("judge request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "judge model returned status {}",
            response.status().as_u16()
        ));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|error| format!("judge response decode failed: {error}"))?;
    let text = output_text(&payload).ok_or_else(|| "judge returned no output".to_owned())?;
    serde_json::from_str(&text).map_err(|error| format!("judge output parse failed: {error}"))
}

fn judgment_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["criteria"],
        "properties": {
            "criteria": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "score", "rationale"],
                    "properties": {
                        "name": { "type": "string", "enum": CRITERIA },
                        "score": { "type": "integer", "minimum": 1, "maximum": 5 },
                        "rationale": { "type": "string" }
                    }
                }
            }
        }
    })
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
