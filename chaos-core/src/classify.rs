use crate::contract::{DepChange, Effects, GitOp, OpClass};
use crate::parse::{normalize_path, SimpleCommand};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug)]
pub enum Classification {
    Effects(Effects),
    Unclassified,
}

pub fn classify(command: &SimpleCommand, cwd: &Path, workspace_root: &Path) -> Classification {
    let Some(argv0) = command.argv.first() else {
        return Classification::Unclassified;
    };
    let argv0 = Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(argv0);

    let Some(entry) = corpus().iter().find(|entry| entry.matches(argv0, &command.argv)) else {
        return Classification::Unclassified;
    };

    let positional = positional_args(command, entry.subcommand.is_some());
    let mut effects = entry.effects.to_effects(&positional, cwd, workspace_root);
    effects.family = Some(entry.family.clone());

    if let Some(escalations) = &entry.flag_escalations {
        for flag in command.argv.iter().skip(1) {
            apply_flag_escalation(flag, escalations, &mut effects);
        }
    }

    Classification::Effects(effects)
}

fn apply_flag_escalation(
    flag: &str,
    escalations: &HashMap<String, EffectEscalation>,
    effects: &mut Effects,
) {
    if let Some(escalation) = escalations.get(flag) {
        escalation.apply(effects);
        return;
    }

    if flag.starts_with('-') && !flag.starts_with("--") {
        for short_flag in flag[1..].chars() {
            let key = format!("-{short_flag}");
            if let Some(escalation) = escalations.get(&key) {
                escalation.apply(effects);
            }
        }
    }
}

fn positional_args(command: &SimpleCommand, has_subcommand: bool) -> Vec<&str> {
    let start = usize::from(has_subcommand) + 1;
    command
        .argv
        .iter()
        .skip(start)
        .filter(|argument| !argument.starts_with('-'))
        .map(String::as_str)
        .collect()
}

fn corpus() -> &'static [CorpusEntry] {
    static CORPUS: OnceLock<Vec<CorpusEntry>> = OnceLock::new();
    CORPUS
        .get_or_init(|| {
            ron::from_str(include_str!("corpus.ron"))
                .expect("classifier corpus.ron must be valid RON")
        })
        .as_slice()
}

#[derive(Debug, Deserialize)]
struct CorpusEntry {
    family: String,
    argv0: String,
    subcommand: Option<String>,
    effects: EffectsTemplate,
    #[serde(default)]
    flag_escalations: Option<HashMap<String, EffectEscalation>>,
}

impl CorpusEntry {
    fn matches(&self, argv0: &str, argv: &[String]) -> bool {
        self.argv0 == argv0
            && self
                .subcommand
                .as_deref()
                .is_none_or(|subcommand| argv.get(1).is_some_and(|argument| argument == subcommand))
    }
}

#[derive(Debug, Default, Deserialize)]
struct EffectsTemplate {
    #[serde(default)]
    op: OpClass,
    #[serde(default)]
    paths: Vec<PathEffectTemplate>,
    #[serde(default)]
    dep_change: Option<DepChangeTemplate>,
    #[serde(default)]
    git: Option<GitOp>,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    privileged: bool,
}

impl EffectsTemplate {
    fn to_effects(&self, positional: &[&str], cwd: &Path, workspace_root: &Path) -> Effects {
        let mut effects = Effects {
            dep_change: self.dep_change.as_ref().map(|template| DepChange {
                manifest: normalize_template_path(&template.manifest, cwd, workspace_root),
                summary: dependency_summary(&template.summary, positional),
            }),
            git: self.git,
            network: self.network,
            privileged: self.privileged,
            op: self.op,
            ..Effects::default()
        };

        for path_effect in &self.paths {
            for path in path_effect.source.resolve(positional, cwd, workspace_root) {
                match path_effect.kind {
                    PathEffectKind::Read => effects.reads.push(path),
                    PathEffectKind::Write => effects.writes.push(path),
                    PathEffectKind::Delete => effects.deletes.push(path),
                }
            }
        }
        effects
    }
}

fn dependency_summary(prefix: &str, positional: &[&str]) -> String {
    if positional.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix} {}", positional.join(" "))
    }
}

#[derive(Debug, Deserialize)]
struct DepChangeTemplate {
    manifest: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
struct PathEffectTemplate {
    kind: PathEffectKind,
    source: PathSource,
}

#[derive(Clone, Copy, Debug, Deserialize)]
enum PathEffectKind {
    Read,
    Write,
    Delete,
}

#[derive(Debug, Deserialize)]
enum PathSource {
    Literal(String),
    FromArgs,
    FromArgsExceptLast,
    LastArg,
}

impl PathSource {
    fn resolve(&self, positional: &[&str], cwd: &Path, workspace_root: &Path) -> Vec<PathBuf> {
        match self {
            Self::Literal(path) => vec![normalize_template_path(path, cwd, workspace_root)],
            Self::FromArgs => positional
                .iter()
                .map(|path| normalize_template_path(path, cwd, workspace_root))
                .collect(),
            Self::FromArgsExceptLast => positional
                .split_last()
                .map(|(_, paths)| {
                    paths
                        .iter()
                        .map(|path| normalize_template_path(path, cwd, workspace_root))
                        .collect()
                })
                .unwrap_or_default(),
            Self::LastArg => positional
                .last()
                .map(|path| vec![normalize_template_path(path, cwd, workspace_root)])
                .unwrap_or_default(),
        }
    }
}

fn normalize_template_path(path: impl AsRef<Path>, cwd: &Path, workspace_root: &Path) -> PathBuf {
    normalize_path(cwd, workspace_root, path).path
}

#[derive(Debug, Default, Deserialize)]
struct EffectEscalation {
    #[serde(default)]
    op: Option<OpClass>,
    #[serde(default)]
    git: Option<GitOp>,
    #[serde(default)]
    network: Option<bool>,
    #[serde(default)]
    privileged: Option<bool>,
    #[serde(default)]
    recursive: Option<bool>,
    #[serde(default)]
    forced: Option<bool>,
}

impl EffectEscalation {
    fn apply(&self, effects: &mut Effects) {
        if let Some(op) = self.op {
            effects.op = op;
        }
        if let Some(git) = self.git {
            effects.git = Some(git);
        }
        if let Some(network) = self.network {
            effects.network = network;
        }
        if let Some(privileged) = self.privileged {
            effects.privileged = privileged;
        }
        if let Some(recursive) = self.recursive {
            effects.recursive = recursive;
        }
        if let Some(forced) = self.forced {
            effects.forced = forced;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse, ParseOutcome};

    fn classify_command(raw: &str) -> Effects {
        let command = match parse(raw) {
            ParseOutcome::Commands(mut commands) => commands.remove(0),
            outcome => panic!("expected a command, got {outcome:?}"),
        };
        match classify(&command, Path::new("/workspace/repo"), Path::new("/workspace/repo")) {
            Classification::Effects(effects) => effects,
            Classification::Unclassified => panic!("expected {raw:?} to be classified"),
        }
    }

    #[test]
    fn classifies_cargo_build() {
        let effects = classify_command("cargo build");
        assert_eq!(effects.op, OpClass::Build);
        assert_eq!(effects.writes, [PathBuf::from("/workspace/repo/target")]);
    }

    #[test]
    fn classifies_cargo_test() {
        let effects = classify_command("cargo test");
        assert_eq!(effects.op, OpClass::Test);
        assert_eq!(effects.writes, [PathBuf::from("/workspace/repo/target")]);
    }

    #[test]
    fn classifies_cargo_run() {
        let effects = classify_command("cargo run");
        assert_eq!(effects.op, OpClass::Run);
        assert_eq!(effects.writes, [PathBuf::from("/workspace/repo/target")]);
    }

    #[test]
    fn classifies_cargo_add() {
        let effects = classify_command("cargo add axios@1.6");
        assert_eq!(effects.op, OpClass::Edit);
        assert!(effects.network);
        assert_eq!(effects.dep_change.unwrap().summary, "add axios@1.6");
    }

    #[test]
    fn classifies_cargo_remove() {
        let effects = classify_command("cargo remove axios");
        assert_eq!(effects.op, OpClass::Edit);
        assert_eq!(effects.dep_change.unwrap().summary, "remove axios");
    }

    #[test]
    fn classifies_git_status() {
        let effects = classify_command("git status");
        assert_eq!(effects.op, OpClass::Read);
        assert_eq!(effects.git, Some(GitOp::Status));
    }

    #[test]
    fn classifies_git_diff() {
        let effects = classify_command("git diff");
        assert_eq!(effects.git, Some(GitOp::Diff));
    }

    #[test]
    fn classifies_git_log() {
        let effects = classify_command("git log");
        assert_eq!(effects.git, Some(GitOp::Log));
    }

    #[test]
    fn classifies_git_add() {
        let effects = classify_command("git add src/lib.rs");
        assert_eq!(effects.git, Some(GitOp::Add));
    }

    #[test]
    fn classifies_git_commit() {
        let effects = classify_command("git commit -m message");
        assert_eq!(effects.git, Some(GitOp::Commit));
    }

    #[test]
    fn classifies_git_checkout() {
        let effects = classify_command("git checkout main");
        assert_eq!(effects.git, Some(GitOp::Checkout));
    }

    #[test]
    fn classifies_git_push() {
        let effects = classify_command("git push origin main");
        assert_eq!(effects.git, Some(GitOp::Push));
        assert!(effects.network);
    }

    #[test]
    fn git_push_force_escalates_to_force_push() {
        let effects = classify_command("git push --force");
        assert_eq!(effects.git, Some(GitOp::ForcePush));
    }

    #[test]
    fn classifies_rm() {
        let effects = classify_command("rm -rf tmp/cache");
        assert_eq!(effects.op, OpClass::Delete);
        assert_eq!(effects.deletes, [PathBuf::from("/workspace/repo/tmp/cache")]);
        assert!(effects.recursive);
        assert!(effects.forced);
    }

    #[test]
    fn rm_short_flags_escalate_in_any_combination() {
        for command in ["rm -r cache", "rm -R cache", "rm -fr cache", "rm -r -f cache"] {
            let effects = classify_command(command);
            assert!(effects.recursive, "{command}");
            assert!(effects.forced || !command.contains('f'), "{command}");
        }

        let forced_only = classify_command("rm -f cache");
        assert!(!forced_only.recursive);
        assert!(forced_only.forced);
    }

    #[test]
    fn classifies_mv() {
        let effects = classify_command("mv source destination");
        assert_eq!(effects.deletes, [PathBuf::from("/workspace/repo/source")]);
        assert_eq!(effects.writes, [PathBuf::from("/workspace/repo/destination")]);
    }

    #[test]
    fn classifies_cp() {
        let effects = classify_command("cp source destination");
        assert_eq!(effects.reads, [PathBuf::from("/workspace/repo/source")]);
        assert_eq!(effects.writes, [PathBuf::from("/workspace/repo/destination")]);
    }

    #[test]
    fn classifies_npm_install() {
        let effects = classify_command("npm install express");
        assert!(effects.network);
        assert_eq!(
            effects.dep_change.unwrap().manifest,
            PathBuf::from("/workspace/repo/package.json")
        );
    }

    #[test]
    fn classifies_pip_install() {
        let effects = classify_command("pip install requests");
        assert!(effects.network);
        assert_eq!(
            effects.dep_change.unwrap().manifest,
            PathBuf::from("/workspace/repo/requirements.txt")
        );
        assert!(effects.writes.is_empty());
    }

    #[test]
    fn classifies_curl() {
        let effects = classify_command("curl https://example.com");
        assert_eq!(effects.op, OpClass::Run);
        assert!(effects.network);
    }

    #[test]
    fn unknown_binary_is_unclassified() {
        let command = match parse("unknown-binary --flag") {
            ParseOutcome::Commands(mut commands) => commands.remove(0),
            outcome => panic!("expected a command, got {outcome:?}"),
        };

        assert!(matches!(
            classify(&command, Path::new("/workspace/repo"), Path::new("/workspace/repo")),
            Classification::Unclassified
        ));
    }
}
