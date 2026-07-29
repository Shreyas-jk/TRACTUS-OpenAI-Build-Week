//! `tractus init` — one-time, idempotent setup.
//!
//! Two steps are otherwise manual and easy to get wrong: registering the Codex
//! hook and enabling the experimental hook feature flag. `init` installs the
//! hook **globally** in `~/.codex/hooks.json` (merging into any existing hooks)
//! so it protects every project you launch through `tractus`, enables the flag
//! in `~/.codex/config.toml`, verifies the sibling binaries, and prints exactly
//! what it changed. Safe to re-run: unchanged files are left untouched and any
//! file it edits is backed up first. The hook itself no-ops for Codex sessions
//! Tractus did not launch, so a global install never disturbs ordinary work.

use serde_json::{json, Map, Value};
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use toml_edit::{value, DocumentMut, Item, Table};

const WRAPPER_RELATIVE: &str = ".codex/run-tractus-hook.sh";
const WRAPPER_NAME: &str = "run-tractus-hook.sh";
const REQUIRED_BINARIES: &[&str] = &["tractusd", "tractus-hook"];

/// Resolve the repository and Codex home, then run setup.
pub fn run_init<W: Write>(output: &mut W) -> Result<(), InitError> {
    let repo_root = locate_repo_root()?;
    let codex_home = codex_home()?;
    init_with_paths(&repo_root, &codex_home, output)
}

fn init_with_paths<W: Write>(
    repo_root: &Path,
    codex_home: &Path,
    output: &mut W,
) -> Result<(), InitError> {
    writeln!(output, "TRACTUS ▸ init")?;
    writeln!(output, "Repository: {}", repo_root.display())?;
    writeln!(output, "Codex home: {}", codex_home.display())?;
    writeln!(output)?;

    let wrapper = repo_root.join(WRAPPER_RELATIVE);
    let hook = install_hook(codex_home, &wrapper)?;
    match (&hook.backup, hook.changed) {
        (Some(backup), _) => writeln!(
            output,
            "✓ Codex hook installed globally: {} (backup at {})",
            hook.hooks_file.display(),
            backup.display()
        )?,
        (None, true) => writeln!(
            output,
            "✓ Codex hook installed globally: {}",
            hook.hooks_file.display()
        )?,
        (None, false) => writeln!(
            output,
            "✓ Codex hook already installed: {}",
            hook.hooks_file.display()
        )?,
    }

    let flag = enable_hook_feature(&codex_home.join("config.toml"))?;
    match &flag {
        FlagOutcome::AlreadyEnabled { path } => {
            writeln!(output, "✓ Hook feature already enabled: {}", path.display())?
        }
        FlagOutcome::Created { path } => writeln!(
            output,
            "✓ Hook feature enabled (created {})",
            path.display()
        )?,
        FlagOutcome::Updated { path, backup } => writeln!(
            output,
            "✓ Hook feature enabled: {} (backup at {})",
            path.display(),
            backup.display()
        )?,
    }

    let binaries = check_binaries(repo_root);
    let mut missing = Vec::new();
    for binary in &binaries {
        match &binary.path {
            Some(path) => writeln!(output, "✓ {} present: {}", binary.name, path.display())?,
            None => {
                writeln!(output, "• {} not built yet", binary.name)?;
                missing.push(binary.name);
            }
        }
    }

    writeln!(output)?;
    if missing.is_empty() {
        writeln!(
            output,
            "Setup complete. Next: run `tractus` to create or pick a contract and launch Codex."
        )?;
    } else {
        writeln!(
            output,
            "Almost there — build the missing binaries with `cargo build --release`, then run `tractus`."
        )?;
    }
    writeln!(
        output,
        "On first launch Codex will ask you to trust this hook — approve it once, or it is skipped."
    )?;
    Ok(())
}

/// Walk up from the running executable and the working directory looking for the
/// committed hook wrapper, which marks the repository root.
fn locate_repo_root() -> Result<PathBuf, InitError> {
    let mut starts: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            starts.push(dir.to_path_buf());
        }
    }
    if let Ok(cwd) = env::current_dir() {
        starts.push(cwd);
    }
    for start in starts {
        let mut current: Option<&Path> = Some(start.as_path());
        while let Some(dir) = current {
            if dir.join(WRAPPER_RELATIVE).is_file() {
                return Ok(dir.to_path_buf());
            }
            current = dir.parent();
        }
    }
    Err(InitError::RepoRootNotFound)
}

struct HookOutcome {
    hooks_file: PathBuf,
    changed: bool,
    backup: Option<PathBuf>,
}

/// Install the Tractus hook into `<codex_home>/hooks.json`, merging into any
/// existing hooks. An existing file is backed up before it is rewritten, and an
/// unparseable one is preserved (backed up) rather than silently discarded.
fn install_hook(codex_home: &Path, wrapper: &Path) -> Result<HookOutcome, InitError> {
    if !wrapper.is_file() {
        return Err(InitError::WrapperMissing {
            path: wrapper.to_path_buf(),
        });
    }
    ensure_executable(wrapper)?;
    fs::create_dir_all(codex_home).map_err(|source| InitError::Io {
        path: codex_home.to_path_buf(),
        source,
    })?;

    let hooks_file = codex_home.join("hooks.json");
    let wrapper_command = wrapper.to_string_lossy().into_owned();

    let existed = hooks_file.exists();
    let mut document = if existed {
        let text = fs::read_to_string(&hooks_file).map_err(|source| InitError::Io {
            path: hooks_file.clone(),
            source,
        })?;
        // A file we cannot parse is preserved via the backup below, not merged.
        serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    let original = document.clone();
    ensure_tractus_hook(&mut document, &wrapper_command);
    let changed = document != original;

    let mut backup = None;
    if changed {
        if existed {
            let path = backup_path(&hooks_file);
            fs::copy(&hooks_file, &path).map_err(|source| InitError::Io {
                path: path.clone(),
                source,
            })?;
            backup = Some(path);
        }
        write_file(&hooks_file, &to_pretty(&document))?;
    }
    Ok(HookOutcome {
        hooks_file,
        changed,
        backup,
    })
}

/// Ensure exactly one Tractus `PreToolUse` entry exists, pointing at `wrapper`,
/// without disturbing any other hooks the user has configured.
fn ensure_tractus_hook(document: &mut Value, wrapper: &str) {
    let root = ensure_object(document);
    let hooks = ensure_object(root.entry("hooks").or_insert_with(|| json!({})));
    let pre_tool_use = ensure_array(hooks.entry("PreToolUse").or_insert_with(|| json!([])));
    let desired = tractus_entry(wrapper);
    match pre_tool_use
        .iter_mut()
        .find(|entry| entry_is_tractus(entry))
    {
        Some(existing) => *existing = desired,
        None => pre_tool_use.push(desired),
    }
}

fn entry_is_tractus(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.ends_with(WRAPPER_NAME))
            })
        })
}

fn tractus_entry(wrapper: &str) -> Value {
    json!({
        "matcher": "Bash|apply_patch",
        "hooks": [ { "type": "command", "command": wrapper } ],
    })
}

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("coerced to object")
}

fn ensure_array(value: &mut Value) -> &mut Vec<Value> {
    if !value.is_array() {
        *value = Value::Array(Vec::new());
    }
    value.as_array_mut().expect("coerced to array")
}

fn to_pretty(document: &Value) -> String {
    let mut rendered =
        serde_json::to_string_pretty(document).expect("a JSON value always serializes");
    rendered.push('\n');
    rendered
}

fn ensure_executable(path: &Path) -> Result<(), InitError> {
    let metadata = fs::metadata(path).map_err(|source| InitError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut permissions = metadata.permissions();
    let mode = permissions.mode();
    if mode & 0o100 == 0 {
        permissions.set_mode(mode | 0o755);
        fs::set_permissions(path, permissions).map_err(|source| InitError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

enum FlagOutcome {
    AlreadyEnabled { path: PathBuf },
    Created { path: PathBuf },
    Updated { path: PathBuf, backup: PathBuf },
}

fn enable_hook_feature(config_path: &Path) -> Result<FlagOutcome, InitError> {
    if !config_path.exists() {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent).map_err(|source| InitError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        write_file(config_path, "[features]\nhooks = true\n")?;
        return Ok(FlagOutcome::Created {
            path: config_path.to_path_buf(),
        });
    }

    let existing = fs::read_to_string(config_path).map_err(|source| InitError::Io {
        path: config_path.to_path_buf(),
        source,
    })?;
    let mut document =
        existing
            .parse::<DocumentMut>()
            .map_err(|source| InitError::ConfigParse {
                path: config_path.to_path_buf(),
                source,
            })?;

    if hooks_already_enabled(&document) {
        return Ok(FlagOutcome::AlreadyEnabled {
            path: config_path.to_path_buf(),
        });
    }

    set_hooks_flag(&mut document);
    let backup = backup_path(config_path);
    fs::copy(config_path, &backup).map_err(|source| InitError::Io {
        path: backup.clone(),
        source,
    })?;
    write_file(config_path, &document.to_string())?;
    Ok(FlagOutcome::Updated {
        path: config_path.to_path_buf(),
        backup,
    })
}

fn hooks_already_enabled(document: &DocumentMut) -> bool {
    document
        .get("features")
        .and_then(Item::as_table)
        .and_then(|features| features.get("hooks"))
        .and_then(Item::as_bool)
        == Some(true)
}

fn set_hooks_flag(document: &mut DocumentMut) {
    let table = document.as_table_mut();
    let features = table
        .entry("features")
        .or_insert_with(|| Item::Table(Table::new()));
    match features.as_table_mut() {
        Some(features_table) => {
            features_table.insert("hooks", value(true));
        }
        None => {
            let mut features_table = Table::new();
            features_table.insert("hooks", value(true));
            *features = Item::Table(features_table);
        }
    }
}

fn backup_path(config_path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    config_path.with_file_name(format!("{name}.tractus-bak-{stamp}"))
}

struct BinaryStatus {
    name: &'static str,
    path: Option<PathBuf>,
}

fn check_binaries(repo_root: &Path) -> Vec<BinaryStatus> {
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));

    REQUIRED_BINARIES
        .iter()
        .map(|name| {
            let mut candidates = Vec::new();
            if let Some(dir) = &exe_dir {
                candidates.push(dir.join(name));
            }
            candidates.push(repo_root.join("target/release").join(name));
            candidates.push(repo_root.join("target/debug").join(name));
            let path = candidates.into_iter().find(|candidate| candidate.is_file());
            BinaryStatus { name, path }
        })
        .collect()
}

fn codex_home() -> Result<PathBuf, InitError> {
    if let Some(dir) = env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or(InitError::HomeNotFound)?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn write_file(path: &Path, contents: &str) -> Result<(), InitError> {
    fs::write(path, contents).map_err(|source| InitError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug)]
pub enum InitError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Output(io::Error),
    RepoRootNotFound,
    HomeNotFound,
    WrapperMissing {
        path: PathBuf,
    },
    ConfigParse {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
}

impl fmt::Display for InitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "I/O error at {}: {source}", path.display())
            }
            Self::Output(source) => write!(formatter, "could not write init output: {source}"),
            Self::RepoRootNotFound => write!(
                formatter,
                "could not find the Tractus repository (looked for {WRAPPER_RELATIVE}); run init from inside the project"
            ),
            Self::HomeNotFound => write!(
                formatter,
                "neither CODEX_HOME nor HOME is set; cannot locate ~/.codex"
            ),
            Self::WrapperMissing { path } => write!(
                formatter,
                "hook wrapper is missing: {}; the repository checkout is incomplete",
                path.display()
            ),
            Self::ConfigParse { path, source } => write!(
                formatter,
                "could not parse Codex config {}: {source}",
                path.display()
            ),
        }
    }
}

impl From<io::Error> for InitError {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

impl Error for InitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Output(source) => Some(source),
            Self::ConfigParse { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestRepo {
        root: PathBuf,
    }

    impl TestRepo {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "tractus-init-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(root.join(".codex")).unwrap();
            fs::write(root.join(WRAPPER_RELATIVE), "#!/bin/sh\n").unwrap();
            let mut permissions = fs::metadata(root.join(WRAPPER_RELATIVE))
                .unwrap()
                .permissions();
            permissions.set_mode(0o644);
            fs::set_permissions(root.join(WRAPPER_RELATIVE), permissions).unwrap();
            Self { root }
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn count_backups(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("tractus-bak"))
            .count()
    }

    #[test]
    fn installs_hook_globally_and_creates_config_when_absent() {
        let repo = TestRepo::new();
        let home = repo.root.join("codex-home");
        let mut output = Vec::new();

        init_with_paths(&repo.root, &home, &mut output).unwrap();

        let hooks = fs::read_to_string(home.join("hooks.json")).unwrap();
        assert!(hooks.contains(
            &repo
                .root
                .join(WRAPPER_RELATIVE)
                .to_string_lossy()
                .into_owned()
        ));
        assert!(hooks.contains("Bash|apply_patch"));
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            "[features]\nhooks = true\n"
        );
        // The wrapper must end up executable for Codex to run it.
        let mode = fs::metadata(repo.root.join(WRAPPER_RELATIVE))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o100, 0);
    }

    #[test]
    fn is_idempotent_on_second_run() {
        let repo = TestRepo::new();
        let home = repo.root.join("codex-home");
        let mut first = Vec::new();
        init_with_paths(&repo.root, &home, &mut first).unwrap();

        let hooks_before = fs::read_to_string(home.join("hooks.json")).unwrap();
        let config_before = fs::read_to_string(home.join("config.toml")).unwrap();

        let mut second = Vec::new();
        init_with_paths(&repo.root, &home, &mut second).unwrap();

        assert_eq!(
            fs::read_to_string(home.join("hooks.json")).unwrap(),
            hooks_before
        );
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            config_before
        );
        assert!(String::from_utf8(second).unwrap().contains("already"));
        assert_eq!(count_backups(&home), 0);
    }

    #[test]
    fn merges_into_existing_hooks_and_preserves_other_entries() {
        let repo = TestRepo::new();
        let home = repo.root.join("codex-home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{"type":"command","command":"/usr/local/bin/other-hook"}]}]}}"#,
        )
        .unwrap();

        init_with_paths(&repo.root, &home, &mut Vec::new()).unwrap();

        let hooks: Value =
            serde_json::from_str(&fs::read_to_string(home.join("hooks.json")).unwrap()).unwrap();
        let entries = hooks["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry["hooks"][0]["command"] == "/usr/local/bin/other-hook"));
        assert!(entries.iter().any(|entry| entry["hooks"][0]["command"]
            .as_str()
            .is_some_and(|command| command.ends_with(WRAPPER_NAME))));
        assert_eq!(count_backups(&home), 1);

        // A second run neither duplicates the entry nor writes another backup.
        init_with_paths(&repo.root, &home, &mut Vec::new()).unwrap();
        let hooks: Value =
            serde_json::from_str(&fs::read_to_string(home.join("hooks.json")).unwrap()).unwrap();
        let tractus_entries = hooks["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry["hooks"][0]["command"]
                    .as_str()
                    .is_some_and(|command| command.ends_with(WRAPPER_NAME))
            })
            .count();
        assert_eq!(tractus_entries, 1);
        assert_eq!(count_backups(&home), 1);
    }

    #[test]
    fn preserves_existing_config_and_backs_up_before_editing() {
        let repo = TestRepo::new();
        let home = repo.root.join("codex-home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            "# my settings\nmodel = \"gpt-5.6-terra\"\n\n[features]\nother = true\n",
        )
        .unwrap();

        init_with_paths(&repo.root, &home, &mut Vec::new()).unwrap();

        let updated = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(updated.contains("model = \"gpt-5.6-terra\""));
        assert!(updated.contains("other = true"));
        assert!(updated.contains("hooks = true"));
        assert!(updated.contains("# my settings"));
        // hooks.json is new (no backup); config.toml existed (one backup).
        assert_eq!(count_backups(&home), 1);
    }

    #[test]
    fn missing_wrapper_is_an_error() {
        let repo = TestRepo::new();
        fs::remove_file(repo.root.join(WRAPPER_RELATIVE)).unwrap();
        let home = repo.root.join("codex-home");

        assert!(matches!(
            init_with_paths(&repo.root, &home, &mut Vec::new()),
            Err(InitError::WrapperMissing { .. })
        ));
    }
}
