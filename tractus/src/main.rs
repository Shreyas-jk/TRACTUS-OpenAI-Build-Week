use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process;
use tractus::store::{ContractDocument, ContractStore, StoreError};
use tractus_core::contract::{GitOp, GitOpSet, OpClass, OpSet};

mod init;
mod launcher;

const ARTIFACT_PATHS: &[&str] = &[
    "target/**",
    "node_modules/**",
    "**/__pycache__/**",
    ".venv/**",
];

const DEFAULT_OPERATION_CHOICES: &[usize] = &[1, 2];
const DEFAULT_GIT_CHOICES: &[usize] = &[1, 2];

fn main() {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    match run(env::args().skip(1), &mut input, &mut output) {
        Ok(0) => {}
        Ok(exit_code) => process::exit(exit_code),
        Err(error) => {
            eprintln!("tractus: {error}");
            process::exit(2);
        }
    }
}

fn run<I, R, W>(arguments: I, input: &mut R, output: &mut W) -> Result<i32, CliError>
where
    I: IntoIterator<Item = String>,
    R: BufRead,
    W: Write,
{
    match parse_command(arguments)? {
        Command::Help => {
            write_usage(output)?;
            Ok(0)
        }
        Command::Init => {
            init::run_init(output)?;
            Ok(0)
        }
        Command::New { workspace_root } => {
            let _ = run_new(&workspace_root, input, output)?;
            Ok(0)
        }
        Command::Codex {
            workspace_root,
            codex_args,
        } => launcher::launch_codex(&workspace_root, &codex_args).map_err(CliError::Launcher),
        Command::Launch {
            workspace_root,
            codex_args,
        } => run_launch(&workspace_root, &codex_args, input, output),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Init,
    New {
        workspace_root: PathBuf,
    },
    Codex {
        workspace_root: PathBuf,
        codex_args: Vec<String>,
    },
    Launch {
        workspace_root: PathBuf,
        codex_args: Vec<String>,
    },
    Help,
}

fn parse_command<I>(arguments: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = String>,
{
    let arguments: Vec<String> = arguments.into_iter().collect();
    let Some(command) = arguments.first() else {
        return Ok(Command::Launch {
            workspace_root: env::current_dir()?,
            codex_args: Vec::new(),
        });
    };
    match command.as_str() {
        "--help" | "-h" | "help" => Ok(Command::Help),
        "init" => parse_init(&arguments[1..]),
        "new" => parse_new(&arguments[1..]),
        "codex" => {
            let (workspace_root, codex_args) = parse_launch_args(&arguments[1..])?;
            Ok(Command::Codex {
                workspace_root,
                codex_args,
            })
        }
        // No recognized subcommand means the bare launch flow: pick or create a
        // contract, then start Codex. Any stray tokens pass through to Codex.
        _ => {
            let (workspace_root, codex_args) = parse_launch_args(&arguments)?;
            Ok(Command::Launch {
                workspace_root,
                codex_args,
            })
        }
    }
}

fn parse_init(arguments: &[String]) -> Result<Command, CliError> {
    for argument in arguments {
        match argument.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            _ => return Err(CliError::Usage(format!("unknown argument {argument:?}"))),
        }
    }
    Ok(Command::Init)
}

fn parse_new(arguments: &[String]) -> Result<Command, CliError> {
    let mut workspace_root = env::current_dir()?;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--workspace" => {
                let path = arguments.next().ok_or_else(|| {
                    CliError::Usage("--workspace requires a path argument".to_owned())
                })?;
                workspace_root = PathBuf::from(path);
            }
            _ => return Err(CliError::Usage(format!("unknown argument {argument:?}"))),
        }
    }
    Ok(Command::New { workspace_root })
}

fn parse_launch_args(arguments: &[String]) -> Result<(PathBuf, Vec<String>), CliError> {
    let mut workspace_root = env::current_dir()?;
    let mut codex_args = Vec::new();
    let mut pass_through = false;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        if pass_through {
            codex_args.push(argument.clone());
            continue;
        }
        match argument.as_str() {
            "--workspace" => {
                let path = arguments.next().ok_or_else(|| {
                    CliError::Usage("--workspace requires a path argument".to_owned())
                })?;
                workspace_root = PathBuf::from(path);
            }
            "--" => pass_through = true,
            _ => codex_args.push(argument.clone()),
        }
    }
    Ok((workspace_root, codex_args))
}

fn write_usage<W: Write>(output: &mut W) -> Result<(), CliError> {
    writeln!(
        output,
        "usage: tractus [--workspace <path>] [-- <codex args>]"
    )?;
    writeln!(output, "       tractus init")?;
    writeln!(output, "       tractus new [--workspace <path>]")?;
    writeln!(
        output,
        "       tractus codex [--workspace <path>] [-- <codex args>]"
    )?;
    writeln!(output, "")?;
    writeln!(
        output,
        "Run with no command to pick or create a contract and launch Codex."
    )?;
    writeln!(
        output,
        "`init` performs one-time setup: installs the Codex hook and enables it."
    )?;
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum NewOutcome {
    Created(ContractDocument),
    Cancelled,
}

fn run_new<R: BufRead, W: Write>(
    workspace_root: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<NewOutcome, CliError> {
    let workspace_root = fs::canonicalize(workspace_root)?;
    writeln!(output, "TRACTUS ▸ new intent contract")?;
    writeln!(output, "Workspace: {}", workspace_root.display())?;
    writeln!(output, "")?;
    writeln!(
        output,
        "This contract stays available for future Tractus sessions until you switch it."
    )?;
    writeln!(output, "Build-artifact paths are included automatically.")?;
    writeln!(output, "")?;

    let task = prompt_nonempty(input, output, "Project goal: ")?;
    let allowed_paths = prompt_paths(input, output)?;
    let selected_operations = prompt_operations(input, output)?;
    let deps_may_change = prompt_yes_no(input, output, "May change dependencies? [y/N]: ", false)?;
    let selected_git_ops = prompt_git_ops(input, output)?;
    let network = prompt_yes_no(input, output, "May access network? [y/N]: ", false)?;

    let contract = build_contract(
        task,
        allowed_paths,
        selected_operations,
        deps_may_change,
        selected_git_ops,
        network,
    );
    render_contract_preview(output, &contract)?;
    if !prompt_yes_no(
        input,
        output,
        "Save and activate this contract? [Y/n]: ",
        true,
    )? {
        writeln!(output, "Contract creation cancelled; nothing was saved.")?;
        return Ok(NewOutcome::Cancelled);
    }

    let store = ContractStore::open(&workspace_root)?;
    let document = store.create(contract)?;
    writeln!(output, "")?;
    writeln!(output, "Saved and activated contract {}.", document.id)?;
    writeln!(
        output,
        "Stored at {}/.tractus/contracts/{}.json",
        workspace_root.display(),
        document.id
    )?;
    Ok(NewOutcome::Created(document))
}

/// The bare `tractus` flow: pick an existing contract or create one, activate it,
/// then launch Codex with it enforced.
fn run_launch<R: BufRead, W: Write>(
    workspace_root: &Path,
    codex_args: &[String],
    input: &mut R,
    output: &mut W,
) -> Result<i32, CliError> {
    let canonical = fs::canonicalize(workspace_root)?;
    let store = ContractStore::open(&canonical)?;
    let documents = store.list()?;

    let selected_id = if documents.is_empty() {
        writeln!(
            output,
            "No contracts yet for this workspace — let's create one."
        )?;
        writeln!(output, "")?;
        match run_new(workspace_root, input, output)? {
            NewOutcome::Created(document) => document.id,
            NewOutcome::Cancelled => return Ok(0),
        }
    } else {
        match select_contract(&store, &documents, workspace_root, input, output)? {
            Some(id) => id,
            None => return Ok(0),
        }
    };

    // `launch_codex` re-reads the active contract, so make the selection active
    // before handing off.
    store.activate(&selected_id)?;
    launcher::launch_codex(workspace_root, codex_args).map_err(CliError::Launcher)
}

fn select_contract<R: BufRead, W: Write>(
    store: &ContractStore,
    documents: &[ContractDocument],
    workspace_root: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<Option<String>, CliError> {
    writeln!(
        output,
        "Recent contracts for {}:",
        store.workspace_root().display()
    )?;
    for (index, document) in documents.iter().enumerate() {
        writeln!(output, "  [{}] {}", index + 1, document.contract.task)?;
    }
    writeln!(output, "")?;

    loop {
        let raw = prompt_line(input, output, "Select a contract [1], (n)ew, or (q)uit: ")?;
        if raw.is_empty() {
            return Ok(Some(documents[0].id.clone()));
        }
        match raw.to_ascii_lowercase().as_str() {
            "q" | "quit" => {
                writeln!(output, "No contract selected.")?;
                return Ok(None);
            }
            "n" | "new" => {
                return match run_new(workspace_root, input, output)? {
                    NewOutcome::Created(document) => Ok(Some(document.id)),
                    NewOutcome::Cancelled => Ok(None),
                };
            }
            _ => match raw.parse::<usize>() {
                Ok(choice) if (1..=documents.len()).contains(&choice) => {
                    return Ok(Some(documents[choice - 1].id.clone()));
                }
                _ => writeln!(
                    output,
                    "Enter a number from 1 through {}, n, or q.",
                    documents.len()
                )?,
            },
        }
    }
}

fn prompt_nonempty<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    prompt: &str,
) -> Result<String, CliError> {
    loop {
        let value = prompt_line(input, output, prompt)?;
        if !value.is_empty() {
            return Ok(value);
        }
        writeln!(output, "A project goal is required.")?;
    }
}

fn prompt_paths<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<Vec<String>, CliError> {
    writeln!(
        output,
        "Allowed paths are workspace-relative. Separate paths with commas (for example: src/**, tests/**)."
    )?;
    loop {
        let raw = prompt_line(input, output, "Paths in scope: ")?;
        match normalize_paths(&raw) {
            Ok(paths) if paths.len() > ARTIFACT_PATHS.len() => return Ok(paths),
            Ok(_) => writeln!(
                output,
                "Choose at least one project path in addition to build artifacts."
            )?,
            Err(error) => writeln!(output, "{error}")?,
        }
    }
}

fn prompt_operations<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<Vec<OpClass>, CliError> {
    writeln!(output, "Allowed operations:")?;
    for (choice, name) in operation_choices() {
        writeln!(output, "  [{choice}] {name}")?;
    }
    writeln!(
        output,
        "Editing, creating, or deleting code automatically grants test and build."
    )?;
    let choices = prompt_choices(
        input,
        output,
        "Operations [default 1,2]: ",
        operation_choices().len(),
        DEFAULT_OPERATION_CHOICES,
        false,
    )?;
    Ok(implied_operations(
        choices
            .into_iter()
            .filter_map(operation_from_choice)
            .collect(),
    ))
}

fn prompt_git_ops<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<Vec<GitOp>, CliError> {
    writeln!(output, "Git permissions (enter `none` for no Git access):")?;
    for (choice, name) in git_choices() {
        writeln!(output, "  [{choice}] {name}")?;
    }
    let choices = prompt_choices(
        input,
        output,
        "Git permissions [default 1,2]: ",
        git_choices().len(),
        DEFAULT_GIT_CHOICES,
        true,
    )?;
    Ok(choices.into_iter().filter_map(git_from_choice).collect())
}

fn prompt_choices<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    prompt: &str,
    maximum: usize,
    default: &[usize],
    allow_none: bool,
) -> Result<Vec<usize>, CliError> {
    loop {
        let raw = prompt_line(input, output, prompt)?;
        if raw.is_empty() {
            return Ok(default.to_vec());
        }
        if allow_none && raw.eq_ignore_ascii_case("none") {
            return Ok(Vec::new());
        }
        match parse_choices(&raw, maximum) {
            Ok(choices) if !choices.is_empty() => return Ok(choices),
            Ok(_) => writeln!(output, "Choose at least one numbered option.")?,
            Err(error) => writeln!(output, "{error}")?,
        }
    }
}

fn prompt_yes_no<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    prompt: &str,
    default: bool,
) -> Result<bool, CliError> {
    loop {
        let raw = prompt_line(input, output, prompt)?;
        if raw.is_empty() {
            return Ok(default);
        }
        match raw.to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(output, "Enter y or n.")?,
        }
    }
}

fn prompt_line<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    prompt: &str,
) -> Result<String, CliError> {
    write!(output, "{prompt}")?;
    output.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Err(CliError::InputClosed);
    }
    Ok(line.trim().to_owned())
}

fn normalize_paths(raw: &str) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    for path in raw
        .split(',')
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        let path = normalize_path(path)?;
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    for artifact in ARTIFACT_PATHS {
        let artifact = (*artifact).to_owned();
        if !paths.contains(&artifact) {
            paths.push(artifact);
        }
    }
    Ok(paths)
}

fn normalize_path(raw: &str) -> Result<String, String> {
    if raw.starts_with('/') {
        return Err(format!(
            "{raw:?} is absolute; paths must be workspace-relative."
        ));
    }
    let path = raw.trim_end_matches('/');
    if path.is_empty() {
        return Err("A path cannot be empty.".to_owned());
    }
    if path.split('/').any(|component| component == "..") {
        return Err(format!(
            "{raw:?} contains `..`; paths must stay within the workspace."
        ));
    }
    if path == "." {
        return Ok("**".to_owned());
    }
    if path.ends_with("/**")
        || path
            .chars()
            .any(|character| matches!(character, '*' | '?' | '['))
    {
        Ok(path.to_owned())
    } else {
        Ok(format!("{path}/**"))
    }
}

fn parse_choices(raw: &str, maximum: usize) -> Result<Vec<usize>, String> {
    let mut choices = Vec::new();
    for raw_choice in raw.split(',').map(str::trim) {
        let choice = raw_choice
            .parse::<usize>()
            .map_err(|_| format!("{raw_choice:?} is not a valid choice."))?;
        if choice == 0 || choice > maximum {
            return Err(format!("Choose a number from 1 through {maximum}."));
        }
        if !choices.contains(&choice) {
            choices.push(choice);
        }
    }
    Ok(choices)
}

fn implied_operations(mut operations: Vec<OpClass>) -> Vec<OpClass> {
    if operations
        .iter()
        .any(|operation| matches!(operation, OpClass::Edit | OpClass::Create | OpClass::Delete))
    {
        push_unique(&mut operations, OpClass::Test);
        push_unique(&mut operations, OpClass::Build);
    }
    operations.sort_by_key(|operation| *operation as u8);
    operations
}

fn push_unique<T: Eq + Copy>(values: &mut Vec<T>, value: T) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn build_contract(
    task: String,
    allowed_paths: Vec<String>,
    allowed_operations: Vec<OpClass>,
    deps_may_change: bool,
    allowed_git_ops: Vec<GitOp>,
    network: bool,
) -> tractus_core::contract::ContractSpec {
    let mut allowed_ops = OpSet::empty();
    for operation in allowed_operations {
        allowed_ops.insert(operation);
    }
    let mut git_ops = GitOpSet::empty();
    for operation in allowed_git_ops {
        git_ops.insert(operation);
    }
    tractus_core::contract::ContractSpec {
        task,
        allowed_paths,
        allowed_ops,
        deps_may_change,
        git_ops,
        network,
    }
}

fn render_contract_preview<W: Write>(
    output: &mut W,
    contract: &tractus_core::contract::ContractSpec,
) -> Result<(), CliError> {
    writeln!(output, "")?;
    writeln!(output, "INTENT CONTRACT")?;
    writeln!(output, "  task: {}", contract.task)?;
    writeln!(output, "  paths: {}", contract.allowed_paths.join(", "))?;
    writeln!(
        output,
        "  operations: {}",
        operation_choices()
            .iter()
            .filter_map(|(choice, name)| {
                operation_from_choice(*choice)
                    .filter(|operation| contract.allowed_ops.contains(*operation))
                    .map(|_| *name)
            })
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        output,
        "  dependency changes: {}",
        if contract.deps_may_change {
            "ON"
        } else {
            "OFF"
        }
    )?;
    let git = git_choices()
        .iter()
        .filter_map(|(choice, name)| {
            git_from_choice(*choice)
                .filter(|operation| contract.git_ops.contains(*operation))
                .map(|_| *name)
        })
        .collect::<Vec<_>>();
    writeln!(
        output,
        "  git: {}",
        if git.is_empty() {
            "none".to_owned()
        } else {
            git.join(", ")
        }
    )?;
    writeln!(
        output,
        "  network: {}",
        if contract.network { "ON" } else { "OFF" }
    )?;
    Ok(())
}

fn operation_choices() -> &'static [(usize, &'static str)] {
    &[
        (1, "read"),
        (2, "edit"),
        (3, "create"),
        (4, "delete"),
        (5, "test"),
        (6, "build"),
        (7, "run"),
    ]
}

fn operation_from_choice(choice: usize) -> Option<OpClass> {
    match choice {
        1 => Some(OpClass::Read),
        2 => Some(OpClass::Edit),
        3 => Some(OpClass::Create),
        4 => Some(OpClass::Delete),
        5 => Some(OpClass::Test),
        6 => Some(OpClass::Build),
        7 => Some(OpClass::Run),
        _ => None,
    }
}

fn git_choices() -> &'static [(usize, &'static str)] {
    &[
        (1, "status"),
        (2, "diff"),
        (3, "log"),
        (4, "add"),
        (5, "commit"),
        (6, "checkout"),
        (7, "push"),
        (8, "force-push"),
        (9, "reset-hard"),
        (10, "clean"),
    ]
}

fn git_from_choice(choice: usize) -> Option<GitOp> {
    match choice {
        1 => Some(GitOp::Status),
        2 => Some(GitOp::Diff),
        3 => Some(GitOp::Log),
        4 => Some(GitOp::Add),
        5 => Some(GitOp::Commit),
        6 => Some(GitOp::Checkout),
        7 => Some(GitOp::Push),
        8 => Some(GitOp::ForcePush),
        9 => Some(GitOp::ResetHard),
        10 => Some(GitOp::Clean),
        _ => None,
    }
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    InputClosed,
    Io(io::Error),
    Store(StoreError),
    Launcher(launcher::LauncherError),
    Init(init::InitError),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(
                formatter,
                "{message}\nusage: tractus new [--workspace <path>]"
            ),
            Self::InputClosed => write!(
                formatter,
                "interactive input closed before the contract was complete"
            ),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Store(error) => write!(formatter, "contract store error: {error}"),
            Self::Launcher(error) => write!(formatter, "launcher error: {error}"),
            Self::Init(error) => write!(formatter, "init error: {error}"),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Launcher(error) => Some(error),
            Self::Init(error) => Some(error),
            _ => None,
        }
    }
}

impl From<init::InitError> for CliError {
    fn from(error: init::InitError) -> Self {
        Self::Init(error)
    }
}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<StoreError> for CliError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<launcher::LauncherError> for CliError {
    fn from(error: launcher::LauncherError) -> Self {
        Self::Launcher(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestWorkspace {
        root: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("tractus-new-test-{}-{sequence}", process::id()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn wizard_input(confirm: &str) -> Cursor<Vec<u8>> {
        Cursor::new(
            format!("Fix the flaky API test\nsrc, tests/api\n1,2\n\n\nn\n{confirm}\n").into_bytes(),
        )
    }

    #[test]
    fn new_wizard_creates_an_active_least_privilege_contract() {
        let workspace = TestWorkspace::new();
        let mut input = wizard_input("y");
        let mut output = Vec::new();

        let outcome = run_new(&workspace.root, &mut input, &mut output).unwrap();
        let NewOutcome::Created(document) = outcome else {
            panic!("expected a saved contract");
        };

        assert_eq!(document.contract.task, "Fix the flaky API test");
        assert_eq!(
            document.contract.allowed_paths,
            vec![
                "src/**",
                "tests/api/**",
                "target/**",
                "node_modules/**",
                "**/__pycache__/**",
                ".venv/**",
            ]
        );
        assert!(document.contract.allowed_ops.contains(OpClass::Read));
        assert!(document.contract.allowed_ops.contains(OpClass::Edit));
        assert!(document.contract.allowed_ops.contains(OpClass::Test));
        assert!(document.contract.allowed_ops.contains(OpClass::Build));
        assert!(!document.contract.allowed_ops.contains(OpClass::Run));
        assert!(!document.contract.deps_may_change);
        assert!(!document.contract.network);
        assert!(document.contract.git_ops.contains(GitOp::Status));
        assert!(document.contract.git_ops.contains(GitOp::Diff));

        let store = ContractStore::open(&workspace.root).unwrap();
        assert_eq!(store.load_active().unwrap().unwrap().id, document.id);
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("Saved and activated contract"));
    }

    #[test]
    fn declining_confirmation_writes_no_contract() {
        let workspace = TestWorkspace::new();
        let mut input = wizard_input("n");
        let mut output = Vec::new();

        assert_eq!(
            run_new(&workspace.root, &mut input, &mut output).unwrap(),
            NewOutcome::Cancelled
        );
        assert!(!workspace.root.join(".tractus").exists());
    }

    #[test]
    fn choices_reject_invalid_values_and_deduplicate() {
        assert_eq!(parse_choices("1, 2, 2", 3).unwrap(), vec![1, 2]);
        assert!(parse_choices("0", 3).is_err());
        assert!(parse_choices("4", 3).is_err());
        assert!(parse_choices("read", 3).is_err());
    }

    #[test]
    fn paths_are_workspace_relative_and_artifacts_are_added_once() {
        assert_eq!(
            normalize_paths("src, target/**, src/**").unwrap(),
            vec![
                "src/**",
                "target/**",
                "node_modules/**",
                "**/__pycache__/**",
                ".venv/**",
            ]
        );
        assert!(normalize_paths("/etc").is_err());
        assert!(normalize_paths("src/../secrets").is_err());
    }

    #[test]
    fn parser_accepts_the_new_command_with_a_workspace() {
        assert_eq!(
            parse_command([
                "new".to_owned(),
                "--workspace".to_owned(),
                "/tmp/project".to_owned(),
            ])
            .unwrap(),
            Command::New {
                workspace_root: PathBuf::from("/tmp/project"),
            }
        );
    }

    #[test]
    fn parser_passes_codex_arguments_and_keeps_its_workspace_local() {
        assert_eq!(
            parse_command([
                "codex".to_owned(),
                "--workspace".to_owned(),
                "/tmp/project".to_owned(),
                "--".to_owned(),
                "--model".to_owned(),
                "gpt-5.6-terra".to_owned(),
            ])
            .unwrap(),
            Command::Codex {
                workspace_root: PathBuf::from("/tmp/project"),
                codex_args: vec!["--model".to_owned(), "gpt-5.6-terra".to_owned()],
            }
        );
    }

    #[test]
    fn no_arguments_selects_the_bare_launch_flow() {
        assert!(matches!(
            parse_command(Vec::<String>::new()).unwrap(),
            Command::Launch { codex_args, .. } if codex_args.is_empty()
        ));
    }

    #[test]
    fn init_command_parses() {
        assert_eq!(parse_command(["init".to_owned()]).unwrap(), Command::Init);
    }

    #[test]
    fn bare_launch_forwards_workspace_and_codex_arguments() {
        assert_eq!(
            parse_command([
                "--workspace".to_owned(),
                "/tmp/project".to_owned(),
                "--".to_owned(),
                "--model".to_owned(),
                "gpt-5.6-terra".to_owned(),
            ])
            .unwrap(),
            Command::Launch {
                workspace_root: PathBuf::from("/tmp/project"),
                codex_args: vec!["--model".to_owned(), "gpt-5.6-terra".to_owned()],
            }
        );
    }

    #[test]
    fn select_contract_defaults_to_most_recent_and_quits() {
        let workspace = TestWorkspace::new();
        let store = ContractStore::open(&workspace.root).unwrap();
        store
            .create(tractus_core::contract::ContractSpec {
                task: "older task".to_owned(),
                allowed_paths: vec!["src/**".to_owned(), "target/**".to_owned()],
                allowed_ops: OpSet::empty(),
                deps_may_change: false,
                git_ops: GitOpSet::empty(),
                network: false,
            })
            .unwrap();
        let newest = store
            .create(tractus_core::contract::ContractSpec {
                task: "newest task".to_owned(),
                allowed_paths: vec!["src/**".to_owned(), "target/**".to_owned()],
                allowed_ops: OpSet::empty(),
                deps_may_change: false,
                git_ops: GitOpSet::empty(),
                network: false,
            })
            .unwrap();
        let documents = store.list().unwrap();

        // Empty input picks the most recently used contract (list is MRU-first).
        let mut enter = Cursor::new(b"\n".to_vec());
        let mut output = Vec::new();
        let chosen =
            select_contract(&store, &documents, &workspace.root, &mut enter, &mut output).unwrap();
        assert_eq!(chosen, Some(newest.id));

        // `q` selects nothing.
        let mut quit = Cursor::new(b"q\n".to_vec());
        let mut output = Vec::new();
        assert_eq!(
            select_contract(&store, &documents, &workspace.root, &mut quit, &mut output).unwrap(),
            None
        );
    }

    #[test]
    fn launching_with_no_contract_and_declining_creation_does_not_start_codex() {
        let workspace = TestWorkspace::new();
        // Wizard input that declines the final save.
        let mut input = wizard_input("n");
        let mut output = Vec::new();

        assert_eq!(
            run_launch(&workspace.root, &[], &mut input, &mut output).unwrap(),
            0
        );
        assert!(ContractStore::open(&workspace.root)
            .unwrap()
            .load_active()
            .unwrap()
            .is_none());
    }
}
