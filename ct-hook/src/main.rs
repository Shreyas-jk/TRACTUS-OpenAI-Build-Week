use ct_shim::{
    request_edit_verdict_with_resolve_mode, request_verdict_with_resolve_mode, ResolveMode,
    ShimVerdict,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::panic;
use std::path::Path;

const UNAVAILABLE_REASON: &str =
    "Tractus unavailable; command denied. Start chaosd and retry, or amend the contract explicitly.";
const UNKNOWN_PATCH_PATHS_REASON: &str =
    "Tractus could not determine which files this patch touches; command denied until the paths are explicit.";

#[derive(Debug, Default, Eq, PartialEq)]
struct TouchedPaths {
    writes: Vec<String>,
    deletes: Vec<String>,
}

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
            request_verdict_with_resolve_mode(
                command,
                cwd,
                agent_session,
                environment,
                ResolveMode::Client,
            )
            .map(verdict_response)
            .unwrap_or_else(|()| unavailable_response())
        }
        "apply_patch" => {
            let Some(paths) = touched_paths(&payload.tool_input) else {
                return decision_response("deny", UNKNOWN_PATCH_PATHS_REASON.to_owned());
            };
            let cwd = payload_cwd(&payload);
            request_edit_verdict_with_resolve_mode(
                &paths.writes,
                &paths.deletes,
                cwd,
                agent_session,
                ResolveMode::Client,
            )
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
            eprintln!("Tractus ct-hook: missing cwd in PreToolUse payload; using .");
            Path::new(".")
        }
    }
}

fn capture_raw_input(input: &[u8]) {
    let Ok(path) = env::var("TRACTUS_HOOK_LOG") else {
        return;
    };
    if let Err(error) = append_raw_capture(Path::new(&path), input) {
        eprintln!("Tractus ct-hook: failed to write raw hook capture: {error}");
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
        // Codex v0.145 parses `ask` for PreToolUse but treats it as a hook
        // failure and runs the tool anyway. A client-owned hold must therefore
        // fail closed until the user amends the contract and retries.
        ShimVerdict::Hold { reason, .. } => decision_response(
            "deny",
            format!("Tractus requires manual review; command denied: {reason}"),
        ),
    }
}

fn touched_paths(tool_input: &Value) -> Option<TouchedPaths> {
    let mut paths = TouchedPaths::default();

    if let Some(patch) = tool_input.as_str() {
        append_patch_paths(patch, &mut paths);
    }
    // Codex's actual PreToolUse payload puts native apply_patch text here.
    // Keep the older aliases for compatibility with earlier adapters.
    for field in ["command", "patch", "diff", "input"] {
        if let Some(patch) = tool_input.get(field).and_then(Value::as_str) {
            append_patch_paths(patch, &mut paths);
        }
    }
    for field in ["changes", "files"] {
        if let Some(entries) = tool_input.get(field).and_then(Value::as_array) {
            for entry in entries {
                match entry {
                    Value::String(path) => append_path(path, &mut paths.writes),
                    Value::Object(_) => {
                        if let Some(path) = entry.get("path").and_then(Value::as_str) {
                            let kind = entry.get("kind").and_then(Value::as_str);
                            let target = if structured_kind_is_delete(kind) {
                                &mut paths.deletes
                            } else {
                                &mut paths.writes
                            };
                            append_path(path, target);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    (!paths.writes.is_empty() || !paths.deletes.is_empty()).then_some(paths)
}

fn append_patch_paths(patch: &str, paths: &mut TouchedPaths) {
    let mut pending_update = None;

    for line in patch.lines() {
        if let Some(path) = patch_path(line, "*** Update File: ") {
            flush_pending_update(&mut pending_update, paths);
            pending_update = Some(path.to_owned());
        } else if let Some(path) = patch_path(line, "*** Add File: ") {
            flush_pending_update(&mut pending_update, paths);
            append_path(path, &mut paths.writes);
        } else if let Some(path) = patch_path(line, "*** Delete File: ") {
            flush_pending_update(&mut pending_update, paths);
            append_path(path, &mut paths.deletes);
        } else if let Some(path) = patch_path(line, "*** Move to: ") {
            if let Some(source) = pending_update.take() {
                append_path(&source, &mut paths.deletes);
            }
            append_path(path, &mut paths.writes);
        }
    }

    flush_pending_update(&mut pending_update, paths);
}

fn patch_path<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.strip_prefix(prefix)
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn flush_pending_update(pending_update: &mut Option<String>, paths: &mut TouchedPaths) {
    if let Some(path) = pending_update.take() {
        append_path(&path, &mut paths.writes);
    }
}

fn structured_kind_is_delete(kind: Option<&str>) -> bool {
    let Some(kind) = kind else {
        return false;
    };
    let kind = kind.to_ascii_lowercase();
    kind.contains("delete")
        || kind.contains("remove")
        || matches!(kind.as_str(), "move_source" | "rename_source")
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
    decision_response("deny", UNAVAILABLE_REASON.to_owned())
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

        assert_eq!(paths.writes, ["src/core.rs", "tests/core.rs"]);
        assert_eq!(paths.deletes, ["src/lib.rs", "old.rs"]);
    }

    #[test]
    fn extracts_paths_from_the_real_codex_apply_patch_command_field() {
        let payload: PreToolUsePayload = serde_json::from_value(json!({
            "session_id": "captured-session",
            "cwd": "/workspace",
            "hook_event_name": "PreToolUse",
            "model": "gpt-5.6-terra",
            "permission_mode": "default",
            "tool_name": "apply_patch",
            "tool_use_id": "exec-captured",
            "turn_id": "turn-captured",
            "tool_input": {
                "command": "*** Begin Patch\n*** Add File: forbidden-smoke.txt\n+blocked\n*** End Patch"
            }
        }))
        .expect("captured Codex payload should deserialize");

        let paths = touched_paths(&payload.tool_input).expect("patch paths should be extracted");

        assert_eq!(paths.writes, ["forbidden-smoke.txt"]);
        assert!(paths.deletes.is_empty());
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

        assert_eq!(paths.writes, ["src/lib.rs", "tests/hook.rs", "README.md"]);
        assert_eq!(paths.deletes, ["src/lib.rs"]);
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
    fn unavailable_and_hold_responses_are_fail_closed_denials() {
        assert_eq!(
            unavailable_response()["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        assert_eq!(
            verdict_response(ShimVerdict::Hold {
                connection: std::os::unix::net::UnixStream::pair().unwrap().0,
                id: "unused".to_owned(),
                reason: "opaque command".to_owned(),
            })["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
    }

    #[test]
    fn raw_capture_preserves_bytes_and_appends_a_newline() {
        let path = std::env::temp_dir().join(format!(
            "tractus-hook-capture-{}-{}.log",
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
