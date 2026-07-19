use ct_shim::{request_verdict, ShimVerdict};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::io::{self, Read, Write};
use std::panic;
use std::path::Path;

const UNAVAILABLE_REASON: &str = "Chaos Twin unavailable so approve manually or start chaosd.";

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
    let encoded = serde_json::to_string(&response)
        .unwrap_or_else(|_| unavailable_response().to_string());
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
    if payload.tool_name != "Bash" {
        return continue_response();
    }
    let Some(command) = payload.tool_input.get("command").and_then(Value::as_str) else {
        return unavailable_response();
    };

    let environment = env::vars().collect::<HashMap<_, _>>();
    match request_verdict(
        command,
        Path::new(&payload.cwd),
        &payload.session_id,
        environment,
    ) {
        Ok(ShimVerdict::Allow { .. }) => continue_response(),
        Ok(ShimVerdict::Block(message)) => decision_response("deny", message),
        Ok(ShimVerdict::Hold { reason, .. }) => decision_response("ask", reason),
        Err(()) => unavailable_response(),
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
