use ct_shim::{request_edit_verdict, request_verdict, ShimVerdict};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::panic;
use std::path::Path;

const UNAVAILABLE_REASON: &str = "Chaos Twin unavailable so approve manually or start chaosd.";
const UNKNOWN_PATCH_PATHS_REASON: &str =
    "Chaos Twin could not determine which files this patch touches, so approve manually.";

#[derive(Deserialize)]
struct PreToolUsePayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    tool_name: String,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_input: Value,
}

fn main() {
    let response = panic::catch_unwind(run).unwrap_or_else(|_| unavailable_response());
    let encoded =
        serde_json::to_string(&response).unwrap_or_else(|_| unavailable_response().to_string());
    let _ = writeln!(io::stdout(), "{encoded}");
}

fn run() -> Value {
    let mut input = Vec::new();
    let read_result = io::stdin().read_to_end(&mut input);
    capture_raw_input(&input);
    if read_result.is_err() {
        return unavailable_response();
    }
    let payload: PreToolUsePayload = match serde_json::from_slice(&input) {
        Ok(payload) => payload,
        Err(_) => return unavailable_response(),
    };

    let _ = (&payload.hook_event_name, &payload.tool_use_id);
    let agent_session = payload
        .session_id
        .as_deref()
        .filter(|session_id| !session_id.is_empty())
        .unwrap_or("default");
    match payload.tool_name.as_str() {
        "Bash" => {
            let Some(command) = payload.tool_input.get("command").and_then(Value::as_str) else {
                return unavailable_response();
            };
            let environment = env::vars().collect::<HashMap<_, _>>();
            let cwd = payload_cwd(&payload);
            request_verdict(command, cwd, agent_session, environment)
                .map(verdict_response)
                .unwrap_or_else(|()| unavailable_response())
        }
        "apply_patch" => {
            let Some(writes) = touched_paths(&payload.tool_input) else {
                return decision_response("ask", UNKNOWN_PATCH_PATHS_REASON.to_owned());
            };
            let cwd = payload_cwd(&payload);
            request_edit_verdict(&writes, cwd, agent_session)
                .map(verdict_response)
                .unwrap_or_else(|()| unavailable_response())
        }
        _ => continue_response(),
    }
}

fn payload_cwd(payload: &PreToolUsePayload) -> &Path {
    match payload.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => Path::new(cwd),
        None => {
            eprintln!("Chaos Twin ct-hook: missing cwd in PreToolUse payload; using .");
            Path::new(".")
        }
    }
}

fn capture_raw_input(input: &[u8]) {
    let Ok(path) = env::var("CHAOSTWIN_HOOK_LOG") else {
        return;
    };
    if let Err(error) = append_raw_capture(Path::new(&path), input) {
        eprintln!("Chaos Twin ct-hook: failed to write raw hook capture: {error}");
    }
}

fn append_raw_capture(path: &Path, input: &[u8]) -> io::Result<()> {
    let mut log = OpenOptions::new().create(true).append(true).open(path)?;
    log.write_all(input)?;
    log.write_all(b"\n")
}

fn verdict_response(verdict: ShimVerdict) -> Value {
    match verdict {
        ShimVerdict::Allow { .. } => continue_response(),
        ShimVerdict::Block(message) => decision_response("deny", message),
        ShimVerdict::Hold { reason, .. } => decision_response("ask", reason),
    }
}

fn touched_paths(tool_input: &Value) -> Option<Vec<String>> {
    let mut paths = Vec::new();

    if let Some(patch) = tool_input.as_str() {
        append_patch_paths(patch, &mut paths);
    }
    for field in ["patch", "diff", "input"] {
        if let Some(patch) = tool_input.get(field).and_then(Value::as_str) {
            append_patch_paths(patch, &mut paths);
        }
    }
    for field in ["changes", "files"] {
        if let Some(entries) = tool_input.get(field).and_then(Value::as_array) {
            for entry in entries {
                match entry {
                    Value::String(path) => append_path(path, &mut paths),
                    Value::Object(_) => {
                        if let Some(path) = entry.get("path").and_then(Value::as_str) {
                            append_path(path, &mut paths);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    (!paths.is_empty()).then_some(paths)
}

fn append_patch_paths(patch: &str, paths: &mut Vec<String>) {
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
        append_path(path, paths);
    }
}

fn append_path(path: &str, paths: &mut Vec<String>) {
    let path = path.trim();
    if !path.is_empty() && !paths.iter().any(|existing| existing == path) {
        paths.push(path.to_owned());
    }
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

    #[test]
    fn extracts_structured_apply_patch_paths() {
        let paths = touched_paths(&json!({
            "changes": [
                {"path": "src/lib.rs", "kind": "update"},
                {"path": "tests/hook.rs", "kind": "add"}
            ],
            "files": [
                {"path": "src/lib.rs", "kind": "delete"},
                "README.md"
            ]
        }))
        .unwrap();

        assert_eq!(paths, ["src/lib.rs", "tests/hook.rs", "README.md"]);
    }

    #[test]
    fn only_tool_name_is_required_in_payload() {
        let payload: PreToolUsePayload = serde_json::from_value(json!({"tool_name": "Bash"}))
            .expect("tool_name-only payload should deserialize");

        assert!(payload.session_id.is_none());
        assert!(payload.cwd.is_none());
        assert!(payload.tool_input.is_null());
    }

    #[test]
    fn raw_capture_preserves_bytes_and_appends_a_newline() {
        let path = std::env::temp_dir().join(format!(
            "chaostwin-hook-capture-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let raw = br#"{"tool_name":"Bash"}"#;

        append_raw_capture(&path, raw).unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            [raw.as_slice(), b"\n"].concat()
        );
        let _ = std::fs::remove_file(path);
    }
}
