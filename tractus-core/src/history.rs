use std::collections::{HashMap, VecDeque};

const WINDOW_SIZE: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitClass {
    Success,
    NonZero,
    Signal,
}

impl ExitClass {
    const fn is_failure(self) -> bool {
        !matches!(self, Self::Success)
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Success => "0",
            Self::NonZero => "nonzero",
            Self::Signal => "signal",
        }
    }
}

#[derive(Clone, Debug)]
struct ExecutedCommand {
    signature: String,
    failed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct History {
    entries: VecDeque<ExecutedCommand>,
    // Retains the P6 construction hook until daemon-driven history reporting lands.
    injected_loop: Option<(u32, String)>,
}

impl History {
    /// Records one completed command for this agent session.
    pub fn record_execution(&mut self, argv: &[String], exit_class: ExitClass) {
        self.entries.push_back(ExecutedCommand {
            signature: command_signature(argv, exit_class),
            failed: exit_class.is_failure(),
        });
        if self.entries.len() > WINDOW_SIZE {
            self.entries.pop_front();
        }
    }

    pub fn loop_detected(&self) -> Option<(u32, String)> {
        if let Some(loop_result) = &self.injected_loop {
            return Some(loop_result.clone());
        }

        let mut failures = HashMap::<&str, u32>::new();
        for command in &self.entries {
            if !command.failed {
                continue;
            }
            let count = failures.entry(&command.signature).or_default();
            *count += 1;
            if *count >= 3 {
                return Some((*count, command.signature.clone()));
            }
        }
        None
    }

    /// Compatibility hook used by the P6 verdict test. Real callers should use
    /// `record_execution`; this method will disappear once daemon tests replace it.
    pub fn with_detected_loop(n: u32, signature: impl Into<String>) -> Self {
        Self {
            injected_loop: Some((n, signature.into())),
            ..Self::default()
        }
    }
}

fn command_signature(argv: &[String], exit_class: ExitClass) -> String {
    let argv0 = argv.first().map(String::as_str).unwrap_or_default();
    let subcommand = argv
        .get(1)
        .filter(|argument| !argument.starts_with('-'))
        .map(String::as_str)
        .unwrap_or_default();
    let mut flags = argv
        .iter()
        .skip(1)
        .filter(|argument| argument.starts_with('-'))
        .map(String::as_str)
        .collect::<Vec<_>>();
    flags.sort_unstable();

    let mut hasher = blake3::Hasher::new();
    add_signature_field(&mut hasher, argv0);
    add_signature_field(&mut hasher, subcommand);
    for flag in flags {
        add_signature_field(&mut hasher, flag);
    }
    add_signature_field(&mut hasher, exit_class.label());
    hasher.finalize().to_hex().to_string()
}

fn add_signature_field(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(arguments: &[&str]) -> Vec<String> {
        arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect()
    }

    fn record(history: &mut History, arguments: &[&str], exit_class: ExitClass) {
        history.record_execution(&command(arguments), exit_class);
    }

    #[test]
    fn three_identical_failing_cargo_builds_halt() {
        let mut history = History::default();
        for _ in 0..3 {
            record(&mut history, &["cargo", "build"], ExitClass::NonZero);
        }

        let (count, signature) = history.loop_detected().expect("loop detected");
        assert_eq!(count, 3);
        assert!(!signature.is_empty());
    }

    #[test]
    fn two_identical_failures_do_not_halt() {
        let mut history = History::default();
        for _ in 0..2 {
            record(&mut history, &["cargo", "build"], ExitClass::NonZero);
        }

        assert_eq!(history.loop_detected(), None);
    }

    #[test]
    fn successful_commands_never_count_toward_a_loop() {
        let mut history = History::default();
        for _ in 0..3 {
            record(&mut history, &["cargo", "build"], ExitClass::Success);
        }

        assert_eq!(history.loop_detected(), None);
    }

    #[test]
    fn interleaved_distinct_failures_do_not_halt() {
        let mut history = History::default();
        for arguments in [
            ["cargo", "build"],
            ["cargo", "test"],
            ["cargo", "build"],
            ["git", "status"],
            ["cargo", "test"],
        ] {
            record(&mut history, &arguments, ExitClass::NonZero);
        }

        assert_eq!(history.loop_detected(), None);
    }

    #[test]
    fn success_between_failures_does_not_reset_the_window() {
        let mut history = History::default();
        record(&mut history, &["cargo", "build"], ExitClass::NonZero);
        record(&mut history, &["cargo", "build"], ExitClass::Success);
        record(&mut history, &["cargo", "build"], ExitClass::NonZero);
        record(&mut history, &["cargo", "build"], ExitClass::Success);
        record(&mut history, &["cargo", "build"], ExitClass::NonZero);

        assert!(history.loop_detected().is_some());
    }

    #[test]
    fn flags_are_order_independent_but_exit_class_is_part_of_the_signature() {
        let first = command_signature(&command(&["rm", "-r", "-f", "tmp"]), ExitClass::NonZero);
        let reordered =
            command_signature(&command(&["rm", "-f", "-r", "other"]), ExitClass::NonZero);
        let signal = command_signature(&command(&["rm", "-r", "-f", "tmp"]), ExitClass::Signal);

        assert_eq!(first, reordered);
        assert_ne!(first, signal);
    }
}
