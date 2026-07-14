use crate::contract::Reason;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseOutcome {
    Commands(Vec<SimpleCommand>),
    Opaque(String),
    NeedsHuman(Reason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandOperator {
    Sequence,
    And,
    Or,
    Pipe,
    Background,
}

#[derive(Clone)]
pub struct SimpleCommand {
    pub argv: Vec<String>,
    pub redirect_writes: Vec<PathBuf>,
    pub redirect_reads: Vec<PathBuf>,
    pub env: HashMap<String, String>,
    /// The top-level operator joining this command to the next command, if any.
    pub operator_after: Option<CommandOperator>,
}

impl std::fmt::Debug for SimpleCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SimpleCommand")
            .field("argv", &self.argv)
            .field("redirect_writes", &self.redirect_writes)
            .field("redirect_reads", &self.redirect_reads)
            .field("env", &self.env)
            .field("operator_after", &self.operator_after)
            .finish()
    }
}

impl PartialEq for SimpleCommand {
    fn eq(&self, other: &Self) -> bool {
        self.argv == other.argv
            && self.redirect_writes == other.redirect_writes
            && self.redirect_reads == other.redirect_reads
            && self.env == other.env
            && self.operator_after == other.operator_after
    }
}

impl Eq for SimpleCommand {}

/// Parses only the deterministic stages that are safe to classify. Any shell form
/// outside this subset is marked opaque for the Docker twin.
pub fn parse(raw: &str) -> ParseOutcome {
    parse_with_env(raw, &HashMap::new())
}

/// Parses a command using the environment snapshot captured by the shim.
pub fn parse_with_env(raw: &str, env_snapshot: &HashMap<String, String>) -> ParseOutcome {
    if let Some(reason) = explicit_opacity_reason(raw) {
        return ParseOutcome::Opaque(reason);
    }

    let segments = match split_top_level(raw) {
        Ok(segments) => segments,
        Err(reason) => return ParseOutcome::Opaque(reason),
    };

    if segments.is_empty() || segments.iter().any(|segment| segment.text.trim().is_empty()) {
        return ParseOutcome::Opaque("empty command segment".to_owned());
    }

    if segments.iter().any(|segment| is_opaque_command(&segment.text)) {
        return ParseOutcome::Opaque("eval, source, and dot commands require twin execution".to_owned());
    }

    let mut commands = Vec::with_capacity(segments.len());
    for segment in segments {
        let mut redirects = match extract_redirects(&segment.text) {
            Ok(redirects) => redirects,
            Err(reason) => return ParseOutcome::Opaque(reason),
        };

        let mut argv = match shell_words::split(&redirects.token_stream) {
            Ok(argv) if !argv.is_empty() => argv,
            Ok(_) => return ParseOutcome::Opaque("empty command segment".to_owned()),
            Err(error) => return ParseOutcome::Opaque(format!("shell tokenization failed: {error}")),
        };

        if substitute_all(&mut argv, env_snapshot).is_err()
            || substitute_paths(&mut redirects.writes, env_snapshot).is_err()
            || substitute_paths(&mut redirects.reads, env_snapshot).is_err()
        {
            return ParseOutcome::NeedsHuman(Reason::UnresolvedVar);
        }

        let env = strip_environment_prefix(&mut argv);

        commands.push(SimpleCommand {
            argv,
            redirect_writes: redirects.writes,
            redirect_reads: redirects.reads,
            env,
            operator_after: segment.operator_after,
        });
    }

    ParseOutcome::Commands(commands)
}

fn substitute_all(
    tokens: &mut [String],
    env_snapshot: &HashMap<String, String>,
) -> Result<(), ()> {
    for token in tokens {
        *token = substitute_variables(token, env_snapshot)?;
    }
    Ok(())
}

fn substitute_paths(
    paths: &mut [PathBuf],
    env_snapshot: &HashMap<String, String>,
) -> Result<(), ()> {
    for path in paths {
        *path = PathBuf::from(substitute_variables(&path.to_string_lossy(), env_snapshot)?);
    }
    Ok(())
}

fn substitute_variables(
    token: &str,
    env_snapshot: &HashMap<String, String>,
) -> Result<String, ()> {
    let bytes = token.as_bytes();
    let mut result = String::with_capacity(token.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'$' {
            let character = token[index..].chars().next().expect("valid UTF-8");
            result.push(character);
            index += character.len_utf8();
            continue;
        }

        let (name, next_index) = if bytes.get(index + 1) == Some(&b'{') {
            let Some(relative_end) = bytes[index + 2..].iter().position(|byte| *byte == b'}') else {
                result.push('$');
                index += 1;
                continue;
            };
            let end = index + 2 + relative_end;
            if end == index + 2 {
                result.push('$');
                index += 1;
                continue;
            }
            (&token[index + 2..end], end + 1)
        } else {
            let start = index + 1;
            let mut end = start;
            while let Some(byte) = bytes.get(end) {
                if byte == &b'_' || byte.is_ascii_alphanumeric() {
                    end += 1;
                } else {
                    break;
                }
            }
            if end == start {
                result.push('$');
                index += 1;
                continue;
            }
            (&token[start..end], end)
        };

        let Some(value) = env_snapshot.get(name) else {
            return Err(());
        };
        result.push_str(value);
        index = next_index;
    }

    Ok(result)
}

fn strip_environment_prefix(argv: &mut Vec<String>) -> HashMap<String, String> {
    let prefix_length = argv
        .iter()
        .take_while(|token| is_environment_assignment(token))
        .count();
    let mut env = HashMap::with_capacity(prefix_length);

    for token in argv.drain(..prefix_length) {
        let (key, value) = token.split_once('=').expect("validated environment assignment");
        env.insert(key.to_owned(), value.to_owned());
    }
    env
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedPath {
    pub path: PathBuf,
    pub escapes: bool,
}

/// Resolves `.` and `..` lexically without touching the filesystem.
pub fn normalize_path(
    cwd: impl AsRef<Path>,
    workspace_root: impl AsRef<Path>,
    raw: impl AsRef<Path>,
) -> NormalizedPath {
    let cwd = cwd.as_ref();
    let workspace_root = lexical_normalize(workspace_root.as_ref());
    let raw = raw.as_ref();
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let path = lexical_normalize(&joined);
    let escapes = !path.starts_with(&workspace_root);

    NormalizedPath { path, escapes }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push("..");
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn explicit_opacity_reason(raw: &str) -> Option<String> {
    let opaque_constructs = [
        ("$(", "command substitution"),
        ("`", "backtick command substitution"),
        ("<(", "process substitution"),
        ("<<", "here document"),
    ];

    opaque_constructs
        .into_iter()
        .find_map(|(needle, description)| raw.contains(needle).then(|| description.to_owned()))
}

fn is_opaque_command(segment: &str) -> bool {
    let Ok(argv) = shell_words::split(segment) else {
        return false;
    };

    let command = argv
        .iter()
        .skip_while(|token| is_environment_assignment(token))
        .next();

    matches!(command.map(String::as_str), Some("eval" | "source" | "."))
}

fn is_environment_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };

    !name.is_empty()
        && name
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit()))
}

#[derive(Debug)]
struct Segment {
    text: String,
    operator_after: Option<CommandOperator>,
}

/// A one-pass quote-aware split of top-level shell operators.
fn split_top_level(raw: &str) -> Result<Vec<Segment>, String> {
    let bytes = raw.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut quote = None;

    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                }
                index += 1;
            }
            Some(b'"') => {
                if byte == b'\\' {
                    index = (index + 2).min(bytes.len());
                } else {
                    if byte == b'"' {
                        quote = None;
                    }
                    index += 1;
                }
            }
            Some(_) => unreachable!(),
            None => {
                if byte == b'\\' {
                    index += 2;
                    continue;
                }
                if matches!(byte, b'\'' | b'"') {
                    quote = Some(byte);
                    index += 1;
                    continue;
                }

                let (operator, width) = match byte {
                    b';' => (Some(CommandOperator::Sequence), 1),
                    b'&' if bytes.get(index + 1) == Some(&b'&') => (Some(CommandOperator::And), 2),
                    b'&' if bytes.get(index + 1) == Some(&b'>') => (None, 1),
                    b'&' => (Some(CommandOperator::Background), 1),
                    b'|' if bytes.get(index + 1) == Some(&b'|') => (Some(CommandOperator::Or), 2),
                    b'|' => (Some(CommandOperator::Pipe), 1),
                    _ => (None, 1),
                };

                if let Some(operator_after) = operator {
                    segments.push(Segment {
                        text: raw[start..index].to_owned(),
                        operator_after: Some(operator_after),
                    });
                    index += width;
                    start = index;
                } else {
                    index += width;
                }
            }
        }
    }

    if quote.is_some() {
        return Err("unbalanced quotes".to_owned());
    }

    segments.push(Segment {
        text: raw[start..].to_owned(),
        operator_after: None,
    });
    Ok(segments)
}

struct Redirects {
    token_stream: String,
    writes: Vec<PathBuf>,
    reads: Vec<PathBuf>,
}

/// Removes top-level redirections before argv tokenization while retaining their
/// filesystem effects for the contract checker.
fn extract_redirects(segment: &str) -> Result<Redirects, String> {
    let bytes = segment.as_bytes();
    let mut token_stream = String::new();
    let mut writes = Vec::new();
    let mut reads = Vec::new();
    let mut copied_until = 0;
    let mut index = 0;
    let mut quote = None;

    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                }
                index += 1;
                continue;
            }
            Some(b'"') => {
                if byte == b'\\' {
                    index += 2;
                } else {
                    if byte == b'"' {
                        quote = None;
                    }
                    index += 1;
                }
                continue;
            }
            Some(_) => unreachable!(),
            None => {}
        }

        if byte == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            index += 1;
            continue;
        }

        let redirect = match byte {
            b'&' if bytes.get(index + 1) == Some(&b'>') => Some((index, 2, true)),
            b'2' if bytes.get(index + 1) == Some(&b'>') => Some((index, 2, true)),
            b'>' if bytes.get(index + 1) == Some(&b'>') => Some((index, 2, true)),
            b'>' => Some((index, 1, true)),
            b'<' => Some((index, 1, false)),
            _ => None,
        };

        let Some((redirect_start, redirect_width, is_write)) = redirect else {
            index += 1;
            continue;
        };

        let target_start = skip_whitespace(bytes, redirect_start + redirect_width);
        let target_end = shell_word_end(bytes, target_start)?;
        if target_start == target_end {
            return Err("redirection is missing a target".to_owned());
        }
        let target = shell_words::split(&segment[target_start..target_end])
            .map_err(|error| format!("redirection target tokenization failed: {error}"))?;
        if target.len() != 1 {
            return Err("redirection target is not a single path".to_owned());
        }

        token_stream.push_str(&segment[copied_until..redirect_start]);
        if is_write {
            writes.push(PathBuf::from(&target[0]));
        } else {
            reads.push(PathBuf::from(&target[0]));
        }
        copied_until = target_end;
        index = target_end;
    }

    token_stream.push_str(&segment[copied_until..]);
    Ok(Redirects {
        token_stream,
        writes,
        reads,
    })
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn shell_word_end(bytes: &[u8], mut index: usize) -> Result<usize, String> {
    let start = index;
    let mut quote = None;

    while let Some(&byte) = bytes.get(index) {
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                }
                index += 1;
            }
            Some(b'"') => {
                if byte == b'\\' {
                    index = (index + 2).min(bytes.len());
                } else {
                    if byte == b'"' {
                        quote = None;
                    }
                    index += 1;
                }
            }
            Some(_) => unreachable!(),
            None if byte == b'\\' => index = (index + 2).min(bytes.len()),
            None if matches!(byte, b'\'' | b'"') => {
                quote = Some(byte);
                index += 1;
            }
            None if byte.is_ascii_whitespace() || matches!(byte, b';' | b'&' | b'|') => break,
            None => index += 1,
        }
    }

    if quote.is_some() {
        return Err("unbalanced quotes in redirection target".to_owned());
    }
    Ok(if start == index { start } else { index })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(raw: &str) -> Vec<SimpleCommand> {
        match parse(raw) {
            ParseOutcome::Commands(commands) => commands,
            ParseOutcome::Opaque(reason) => panic!("expected commands, got opaque: {reason}"),
            ParseOutcome::NeedsHuman(reason) => {
                panic!("expected commands, got needs-human: {reason:?}")
            }
        }
    }

    #[test]
    fn splits_and_chain_into_two_commands() {
        let parsed = commands("cargo test && rm -rf /");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].argv, ["cargo", "test"]);
        assert_eq!(parsed[0].operator_after, Some(CommandOperator::And));
        assert_eq!(parsed[1].argv, ["rm", "-rf", "/"]);
    }

    #[test]
    fn ignores_operators_inside_quotes() {
        let parsed = commands("echo \"a && b\"");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].argv, ["echo", "a && b"]);
    }

    #[test]
    fn command_substitution_is_opaque() {
        assert!(matches!(parse("echo `date`"), ParseOutcome::Opaque(_)));
    }

    #[test]
    fn unbalanced_quotes_are_opaque() {
        assert!(matches!(parse("echo \"unclosed"), ParseOutcome::Opaque(_)));
    }

    #[test]
    fn extracts_output_redirection() {
        let parsed = commands("cargo run > out.txt");

        assert_eq!(parsed[0].argv, ["cargo", "run"]);
        assert_eq!(parsed[0].redirect_writes, [PathBuf::from("out.txt")]);
    }

    #[test]
    fn trailing_redirect_escape_is_opaque_instead_of_panicking() {
        assert!(matches!(parse("<\\"), ParseOutcome::Opaque(_)));
    }

    #[test]
    fn parses_redirects_in_pipeline_segments() {
        let parsed = commands("cat < in.txt | grep x");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].argv, ["cat"]);
        assert_eq!(parsed[0].redirect_reads, [PathBuf::from("in.txt")]);
        assert_eq!(parsed[0].operator_after, Some(CommandOperator::Pipe));
        assert_eq!(parsed[1].argv, ["grep", "x"]);
    }

    #[test]
    fn undefined_variable_needs_human() {
        let environment = HashMap::new();

        assert!(matches!(
            parse_with_env("rm -rf $DIR/build", &environment),
            ParseOutcome::NeedsHuman(Reason::UnresolvedVar)
        ));
    }

    #[test]
    fn empty_defined_variable_is_substituted_and_escapes_workspace() {
        let mut environment = HashMap::new();
        environment.insert("DIR".to_owned(), String::new());

        let parsed = commands_with_env("rm -rf $DIR/build", &environment);
        assert_eq!(parsed[0].argv, ["rm", "-rf", "/build"]);

        let normalized = normalize_path("/workspace/repo", "/workspace/repo", &parsed[0].argv[2]);
        assert_eq!(normalized.path, PathBuf::from("/build"));
        assert!(normalized.escapes);
    }

    #[test]
    fn parent_components_can_escape_workspace() {
        let normalized = normalize_path(
            "/workspace/repo/src",
            "/workspace/repo",
            "../../etc/passwd",
        );

        assert_eq!(normalized.path, PathBuf::from("/workspace/etc/passwd"));
        assert!(normalized.escapes);
    }

    #[test]
    fn lexical_normalization_keeps_internal_paths_inside_workspace() {
        let normalized = normalize_path(
            "/workspace/repo",
            "/workspace/repo",
            "./src/../tests/x",
        );

        assert_eq!(normalized.path, PathBuf::from("/workspace/repo/tests/x"));
        assert!(!normalized.escapes);
    }

    #[test]
    fn leading_environment_assignments_are_stored_on_the_command() {
        let parsed = commands("RUST_LOG=debug MODE=test cargo test");

        assert_eq!(parsed[0].argv, ["cargo", "test"]);
        assert_eq!(parsed[0].env.get("RUST_LOG"), Some(&"debug".to_owned()));
        assert_eq!(parsed[0].env.get("MODE"), Some(&"test".to_owned()));
    }

    fn commands_with_env(raw: &str, environment: &HashMap<String, String>) -> Vec<SimpleCommand> {
        match parse_with_env(raw, environment) {
            ParseOutcome::Commands(commands) => commands,
            ParseOutcome::Opaque(reason) => panic!("expected commands, got opaque: {reason}"),
            ParseOutcome::NeedsHuman(reason) => {
                panic!("expected commands, got needs-human: {reason:?}")
            }
        }
    }
}
