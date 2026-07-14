pub fn scope_violation(clause: &str) -> String {
    format!(
        "Command blocked by Chaos Twin: {clause}. This action is outside the approved task scope. Continue within scope, or ask the user to approve this specific action."
    )
}

pub fn needs_human() -> &'static str {
    "Command deferred by Chaos Twin: awaiting user approval. Do not retry this command; proceed with other in-scope work or ask the user."
}

pub fn loop_halt(n: u32) -> String {
    format!(
        "Halted by Chaos Twin: this command has failed {n} times with the same error. Stop retrying and report the blocker to the user."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_violation_handoff_is_verbatim() {
        assert_eq!(
            scope_violation("R-DEP-01: deps_may_change = false"),
            "Command blocked by Chaos Twin: R-DEP-01: deps_may_change = false. This action is outside the approved task scope. Continue within scope, or ask the user to approve this specific action."
        );
    }

    #[test]
    fn needs_human_handoff_is_verbatim() {
        assert_eq!(
            needs_human(),
            "Command deferred by Chaos Twin: awaiting user approval. Do not retry this command; proceed with other in-scope work or ask the user."
        );
    }

    #[test]
    fn loop_halt_handoff_is_verbatim() {
        assert_eq!(
            loop_halt(3),
            "Halted by Chaos Twin: this command has failed 3 times with the same error. Stop retrying and report the blocker to the user."
        );
    }
}
