use ct_shim::{request_edit_verdict, request_verdict, ShimVerdict};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::io::{self, Read, Write};
use std::panic;
use std::path::Path;

const UNAVAILABLE_REASON: &str = "Chaos Twin unavailable so approve manually or start chaosd.";
const UNKNOWN_PATCH_PATHS_REASON: &str =
    "Chaos Twin could not determine which files this patch touches, so approve manually.";

#[derive(Deserialize)]
struct PreToolUsePayload {
    session_id: String,
    cwd: String,
    hook_event_name: String,
    tool_name: String,
    tool_use_id: String,
    tool_input: Value,
}

fn main() {
    let response = panic::catch_unwind(run).unwrap_or_else(|_| unavailable_response());
    let encoded =
        serde_json::to_string(&response).unwrap_or_else(|_| unavailable_response().to_string());
    let _ = writeln!(io::stdout(), "{encoded}");
}

fn run() -> Value {
    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        return unavailable_response();
    }
    let payload: PreToolUsePayload = match serde_json::from_str(&input) {
        Ok(payload) => payload,
        Err(_) => return unavailable_response(),
    };

    let _ = (&payload.hook_event_name, &payload.tool_use_id);
    match payload.tool_name.as_str() {
        "Bash" => {
            let Some(command) = payload.tool_input.get("command").and_then(Value::as_str) else {
                return unavailable_response();
            };
            let environment = env::vars().collect::<HashMap<_, _>>();
            request_verdict(
                command,
                Path::new(&payload.cwd),
                &payload.session_id,
                environment,
            )
            .map(verdict_response)
            .unwrap_or_else(|()| unavailable_response())
        }
        "apply_patch" => {
            let Some(writes) = touched_paths(&payload.tool_input) else {
                return decision_response("ask", UNKNOWN_PATCH_PATHS_REASON.to_owned());
            };
            request_edit_verdict(&writes, Path::new(&payload.cwd), &payload.session_id)
                .map(verdict_response)
                .unwrap_or_else(|()| unavailable_response())
        }
        _ => continue_response(),
    }
}

fn verdict_response(verdict: ShimVerdict) -> Value {
    match verdict {
        ShimVerdict::Allow { .. } => continue_response(),
        ShimVerdict::Block(message) => decision_response("deny", message),
        ShimVerdict::Hold { reason, .. } => decision_response("ask", reason),
    }
}

fn touched_paths(tool_input: &Value) -> Option<Vec<String>> {
    let patch = ["patch", "diff", "input"]
        .into_iter()
        .find_map(|field| tool_input.get(field).and_then(Value::as_str))?;
    let mut paths = Vec::new();

    for line in patch.lines() {
        let path = [
            "*** Update File: ",
            "*** Add File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ]
        .into_iter()
        .find_map(|prefix| line.strip_prefix(prefix));
        let Some(path) = path.map(str::trim).filter(|path| !path.is_empty()) else {
            continue;
        };
        if !paths.iter().any(|existing| existing == path) {
            paths.push(path.to_owned());
        }
    }

    (!paths.is_empty()).then_some(paths)
}

fn continue_response() -> Value {
    json!({"continue": true})
}

fn unavailable_response() -> Value {
    decision_response("ask", UNAVAILABLE_REASON.to_owned())
}

fn decision_response(decision: &str, reason: String) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_all_native_apply_patch_paths() {
        let paths = touched_paths(&json!({
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n*** Move to: src/core.rs\n*** Add File: tests/core.rs\n*** Delete File: old.rs\n*** End Patch"
        }))
        .unwrap();

        assert_eq!(
            paths,
            ["src/lib.rs", "src/core.rs", "tests/core.rs", "old.rs"]
        );
    }
}
