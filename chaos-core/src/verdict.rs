use crate::classify::Classification;
use crate::contract::{Contract, Effects, ProofTrace, Verdict};
use crate::history::History;
use crate::parse::{normalize_path, SimpleCommand};
use std::path::Path;

/// Combines classifier-declared effects with parser-observed redirections before
/// deterministic contract checking. Unclassified commands carry only observed
/// effects and are routed to the twin by the daemon.
pub fn assemble(
    command: &SimpleCommand,
    classification: Classification,
    cwd: &Path,
    workspace_root: &Path,
) -> Effects {
    let mut effects = match classification {
        Classification::Effects(effects) => effects,
        Classification::Unclassified => Effects::default(),
    };

    effects.reads.extend(
        command
            .redirect_reads
            .iter()
            .map(|path| normalize_path(cwd, workspace_root, path).path),
    );
    effects.writes.extend(
        command
            .redirect_writes
            .iter()
            .map(|path| normalize_path(cwd, workspace_root, path).path),
    );
    effects
}

/// Checks all effects against a contract. Violations deliberately accumulate so
/// the UI can show every breached clause in one proof trace.
pub fn check(effects: &Effects, contract: &Contract, history: &History) -> Verdict {
    if let Some((n, signature)) = history.loop_detected() {
        return Verdict::Loop { n, signature };
    }

    let mut proofs = Vec::new();
    if effects.privileged {
        proofs.push(trace(
            "R-PRIV-01",
            "privileged = false",
            format!("{} requests privileged execution", command_label(effects)),
        ));
    }
    if effects.network && !contract.network {
        proofs.push(trace(
            "R-NET-01",
            "network = false",
            format!("{} accesses the network", command_label(effects)),
        ));
    }
    if let Some(dependency) = &effects.dep_change {
        if !contract.deps_may_change {
            proofs.push(trace(
                "R-DEP-01",
                "deps_may_change = false",
                format!(
                    "{} modifies {} ({})",
                    command_label(effects),
                    dependency.manifest.display(),
                    dependency.summary
                ),
            ));
        }
    }
    if let Some(git) = effects.git {
        if !contract.git_ops.contains(git) {
            proofs.push(trace(
                "R-GIT-01",
                format!("git_ops contains {git:?}"),
                format!("{} performs git {git:?}", command_label(effects)),
            ));
        }
    }
    for path in &effects.writes {
        if !contract.allowed_paths.is_match(path) {
            proofs.push(trace(
                "R-PATH-01",
                "path matches allowed_paths",
                format!("{} modifies {}", command_label(effects), path.display()),
            ));
        }
    }
    for path in &effects.deletes {
        if !contract.allowed_paths.is_match(path) {
            proofs.push(trace(
                "R-PATH-01",
                "path matches allowed_paths",
                format!("{} deletes {}", command_label(effects), path.display()),
            ));
        }
    }
    if !contract.allowed_ops.contains(effects.op) {
        proofs.push(trace(
            "R-OP-01",
            format!("allowed_ops contains {:?}", effects.op),
            format!("{} has operation {:?}", command_label(effects), effects.op),
        ));
    }

    if proofs.is_empty() {
        Verdict::InScope
    } else {
        Verdict::ScopeViolation(proofs)
    }
}

fn trace(rule: &'static str, clause: impl Into<String>, effect: impl Into<String>) -> ProofTrace {
    let clause = clause.into();
    let effect = effect.into();
    ProofTrace {
        rule,
        rendered: format!("{clause} ∧ {effect} ⇒ SCOPE_VIOLATION"),
        clause,
        effect,
    }
}

fn command_label(effects: &Effects) -> String {
    let mut label = effects
        .family
        .clone()
        .unwrap_or_else(|| "unclassified".to_owned());
    let mut flags = Vec::new();
    if effects.recursive {
        flags.push("recursive");
    }
    if effects.forced {
        flags.push("forced");
    }
    if !flags.is_empty() {
        label.push('(');
        label.push_str(&flags.join(","));
        label.push(')');
    }
    label
}

/// Returns the most severe verdict from a compound command. Scope violations
/// retain every proof trace across all segments.
pub fn combine_verdicts(verdicts: impl IntoIterator<Item = Verdict>) -> Verdict {
    let mut proofs = Vec::new();
    let mut needs_human = None;

    for verdict in verdicts {
        match verdict {
            Verdict::Loop { n, signature } => return Verdict::Loop { n, signature },
            Verdict::ScopeViolation(mut segment_proofs) => proofs.append(&mut segment_proofs),
            Verdict::NeedsHuman(reason) if needs_human.is_none() => needs_human = Some(reason),
            Verdict::NeedsHuman(_) | Verdict::InScope => {}
        }
    }

    if !proofs.is_empty() {
        Verdict::ScopeViolation(proofs)
    } else if let Some(reason) = needs_human {
        Verdict::NeedsHuman(reason)
    } else {
        Verdict::InScope
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::classify;
    use crate::contract::{ContractSpec, GitOp, GitOpSet, OpClass, OpSet, Reason};
    use crate::parse::{parse, ParseOutcome};
    use insta::assert_snapshot;
    use proptest::prelude::*;
    use std::path::{Path, PathBuf};

    const ROOT: &str = "/workspace/repo";

    fn command(raw: &str) -> SimpleCommand {
        match parse(raw) {
            ParseOutcome::Commands(mut commands) => commands.remove(0),
            outcome => panic!("expected command, got {outcome:?}"),
        }
    }

    fn example_contract() -> Contract {
        let mut allowed_ops = OpSet::empty();
        for operation in [OpClass::Read, OpClass::Edit, OpClass::Test, OpClass::Build] {
            allowed_ops.insert(operation);
        }
        let mut git_ops = GitOpSet::empty();
        for operation in [GitOp::Status, GitOp::Diff] {
            git_ops.insert(operation);
        }
        ContractSpec {
            task: "fix the failing test in tests/api_test.rs".to_owned(),
            allowed_paths: vec![
                "tests/**".to_owned(),
                "src/api/**".to_owned(),
                "target/**".to_owned(),
            ],
            allowed_ops,
            deps_may_change: false,
            git_ops,
            network: false,
        }
        .compile(ROOT)
        .unwrap()
    }

    fn permissive_contract() -> Contract {
        let mut allowed_ops = OpSet::empty();
        for operation in [
            OpClass::Read,
            OpClass::Edit,
            OpClass::Create,
            OpClass::Delete,
            OpClass::Test,
            OpClass::Build,
            OpClass::Run,
        ] {
            allowed_ops.insert(operation);
        }
        let mut git_ops = GitOpSet::empty();
        for operation in [
            GitOp::Status,
            GitOp::Diff,
            GitOp::Log,
            GitOp::Add,
            GitOp::Commit,
            GitOp::Checkout,
            GitOp::Push,
            GitOp::ForcePush,
            GitOp::ResetHard,
            GitOp::Clean,
        ] {
            git_ops.insert(operation);
        }
        ContractSpec {
            task: "unrestricted".to_owned(),
            allowed_paths: vec!["**".to_owned()],
            allowed_ops,
            deps_may_change: true,
            git_ops,
            network: true,
        }
        .compile(ROOT)
        .unwrap()
    }

    fn classified_effects(raw: &str) -> Effects {
        let command = command(raw);
        assemble(
            &command,
            classify(&command, Path::new(ROOT), Path::new(ROOT)),
            Path::new(ROOT),
            Path::new(ROOT),
        )
    }

    fn rules(verdict: Verdict) -> String {
        match verdict {
            Verdict::InScope => "IN_SCOPE".to_owned(),
            Verdict::ScopeViolation(proofs) => proofs
                .into_iter()
                .map(|proof| proof.rule)
                .collect::<Vec<_>>()
                .join(","),
            Verdict::NeedsHuman(reason) => format!("NEEDS_HUMAN({reason:?})"),
            Verdict::Loop { .. } => "LOOP".to_owned(),
        }
    }

    #[test]
    fn family_contract_matrix() {
        let commands = [
            "cargo build",
            "cargo test",
            "cargo run",
            "cargo add axios",
            "cargo remove axios",
            "git status",
            "git diff",
            "git log",
            "git add tests/api_test.rs",
            "git commit -m fix",
            "git checkout main",
            "git push origin main",
            "rm -rf tmp",
            "mv src/a src/b",
            "cp src/a src/b",
            "npm install express",
            "pip install requests",
            "curl https://example.com",
        ];
        let example = example_contract();
        let permissive = permissive_contract();
        let history = History::default();
        let mut matrix = String::new();

        for command in commands {
            let effects = classified_effects(command);
            matrix.push_str(&format!(
                "{command}\n  example: {}\n  permissive: {}\n",
                rules(check(&effects, &example, &history)),
                rules(check(&effects, &permissive, &history)),
            ));
        }

        assert_snapshot!("family_contract_matrix", matrix);
    }

    #[test]
    fn redirect_effects_are_assembled_and_blocked() {
        let command = command("cargo run > ../../out.txt");
        let effects = assemble(
            &command,
            classify(&command, Path::new(ROOT), Path::new(ROOT)),
            Path::new(ROOT),
            Path::new(ROOT),
        );
        let verdict = check(&effects, &example_contract(), &History::default());
        let Verdict::ScopeViolation(proofs) = verdict else {
            panic!("out-of-scope redirect must violate the contract");
        };

        let redirect_proof = proofs
            .iter()
            .find(|proof| proof.rule == "R-PATH-01" && proof.effect.contains("/out.txt"))
            .expect("redirect path proof");
        assert!(redirect_proof.rendered.contains("cargo-run"));
    }

    #[test]
    fn proof_rendering_includes_family_and_rm_flags() {
        let effects = classified_effects("rm -rf tmp");
        let Verdict::ScopeViolation(proofs) =
            check(&effects, &example_contract(), &History::default())
        else {
            panic!("rm must violate the example contract");
        };

        assert!(proofs
            .iter()
            .all(|proof| proof.rendered.contains("rm(recursive,forced)")));
        assert!(proofs
            .iter()
            .any(|proof| proof.effect.contains(" deletes ")));
    }

    #[test]
    fn composite_verdicts_follow_severity_order() {
        let violation = Verdict::ScopeViolation(vec![ProofTrace {
            rule: "R-PATH-01",
            clause: "path matches allowed_paths".to_owned(),
            effect: "test".to_owned(),
            rendered: "test".to_owned(),
        }]);
        assert!(matches!(
            combine_verdicts([Verdict::NeedsHuman(Reason::Opaque), violation]),
            Verdict::ScopeViolation(_)
        ));
        assert!(matches!(
            combine_verdicts([
                Verdict::NeedsHuman(Reason::Opaque),
                Verdict::Loop {
                    n: 3,
                    signature: "cargo-build".to_owned(),
                },
            ]),
            Verdict::Loop { .. }
        ));
    }

    #[test]
    fn history_loop_takes_precedence_over_effect_violations() {
        let effects = classified_effects("cargo add axios");
        assert!(matches!(
            check(
                &effects,
                &example_contract(),
                &History::with_detected_loop(3, "cargo-add"),
            ),
            Verdict::Loop { n: 3, .. }
        ));
    }

    #[test]
    fn assemble_normalizes_redirect_paths_from_real_cwd() {
        let command = command("cat < ./tests/input.txt > ./tests/output.txt");
        let effects = assemble(
            &command,
            Classification::Unclassified,
            Path::new(ROOT),
            Path::new(ROOT),
        );

        assert_eq!(
            effects.reads,
            [PathBuf::from("/workspace/repo/tests/input.txt")]
        );
        assert_eq!(
            effects.writes,
            [PathBuf::from("/workspace/repo/tests/output.txt")]
        );
    }

    proptest! {
        #[test]
        fn never_allows_unparsed_or_unclassified_commands(raw in ".{0,200}") {
            let contract = example_contract();
            let history = History::default();
            let (verdict, parse_succeeded, every_segment_classified) = match parse(&raw) {
                ParseOutcome::Commands(commands) => {
                    let mut every_segment_classified = true;
                    let mut segment_verdicts = Vec::with_capacity(commands.len());

                    for command in commands {
                        let classification = classify(
                            &command,
                            Path::new(ROOT),
                            Path::new(ROOT),
                        );
                        every_segment_classified &= matches!(
                            &classification,
                            Classification::Effects(_)
                        );
                        let effects = assemble(
                            &command,
                            classification,
                            Path::new(ROOT),
                            Path::new(ROOT),
                        );
                        segment_verdicts.push(check(&effects, &contract, &history));
                    }

                    (
                        combine_verdicts(segment_verdicts),
                        true,
                        every_segment_classified,
                    )
                }
                ParseOutcome::Opaque(_) => (Verdict::NeedsHuman(Reason::Opaque), false, false),
                ParseOutcome::NeedsHuman(reason) => (Verdict::NeedsHuman(reason), false, false),
            };

            if matches!(verdict, Verdict::InScope) {
                prop_assert!(parse_succeeded);
                prop_assert!(every_segment_classified);
            }
        }
    }
}
