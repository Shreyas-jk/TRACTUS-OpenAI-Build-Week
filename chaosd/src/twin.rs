use chaos_core::contract::{DepChange, Effects, OpClass, Reason};
use chaos_core::parse::SimpleCommand;
use std::future::Future;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Output;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const TWIN_TIMEOUT: Duration = Duration::from_secs(3);
const OVERLAY_UNAVAILABLE: &str = "CHAOSTWIN_OVERLAY_UNAVAILABLE";
static NEXT_TWIN: AtomicUsize = AtomicUsize::new(0);

pub enum TwinOutcome {
    Effects(Effects),
    NeedsHuman(Reason),
}

pub trait TwinExecutor: Send + Sync {
    fn speculate<'a>(
        &'a self,
        cmd: &'a SimpleCommand,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = TwinOutcome> + Send + 'a>>;
}

#[derive(Default)]
pub struct NoTwin;

impl TwinExecutor for NoTwin {
    fn speculate<'a>(
        &'a self,
        _cmd: &'a SimpleCommand,
        _cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = TwinOutcome> + Send + 'a>> {
        Box::pin(async { TwinOutcome::NeedsHuman(Reason::Opaque) })
    }
}

pub struct DockerTwin {
    workspace_root: PathBuf,
    image: String,
}

impl DockerTwin {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            image: "alpine:3.20".to_owned(),
        }
    }

    async fn speculate_inner(&self, command: &SimpleCommand, cwd: &Path) -> TwinOutcome {
        let temp = match TemporaryTree::new("chaostwin-twin") {
            Ok(temp) => temp,
            Err(_) => return TwinOutcome::NeedsHuman(Reason::Opaque),
        };
        let command = render_command(command);
        let overlay_name = container_name("overlay");

        match self
            .run_overlay(&temp, &overlay_name, &command, cwd)
            .await
        {
            Ok(output) if overlay_unavailable(&output) => {
                // Docker Desktop's Linux VM owns overlayfs. Some host bind-mount
                // layouts cannot back an overlay upperdir, so fall back to a
                // cp -R snapshot plus recursive diff at a higher latency cost.
                self.run_copy_diff(&temp, &container_name("copy"), &command, cwd)
                    .await
            }
            Ok(_) if !temp.path.join("upper").exists() => TwinOutcome::NeedsHuman(Reason::Opaque),
            Ok(_) => match effects_from_upperdir(temp.path.join("upper"), &self.workspace_root) {
                Ok(effects) => TwinOutcome::Effects(effects),
                Err(_) => TwinOutcome::NeedsHuman(Reason::Opaque),
            },
            Err(TwinError::Timeout) => TwinOutcome::NeedsHuman(Reason::TwinTimeout),
            Err(TwinError::Unavailable) => TwinOutcome::NeedsHuman(Reason::Opaque),
        }
    }

    async fn run_overlay(
        &self,
        temp: &TemporaryTree,
        container_name: &str,
        command: &str,
        cwd: &Path,
    ) -> Result<Output, TwinError> {
        let state = temp.path.to_string_lossy().to_string();
        let lower = self.workspace_root.to_string_lossy().to_string();
        let workdir = self.container_workdir(cwd);
        let script = format!(
            "mkdir -p /state/upper /state/work /work; \
             mount -t overlay overlay -o lowerdir=/lower,upperdir=/state/upper,workdir=/state/work /work \
             || {{ echo {OVERLAY_UNAVAILABLE} >&2; exit 125; }}; \
             cd {workdir}; /bin/sh -c \"$1\"; status=$?; umount /work || true; exit $status"
        );
        self.run_docker(
            container_name,
            [
                "run".to_owned(),
                "--rm".to_owned(),
                "--name".to_owned(),
                container_name.to_owned(),
                "--privileged".to_owned(),
                "--network=none".to_owned(),
                "--pids-limit=256".to_owned(),
                "-v".to_owned(),
                format!("{lower}:/lower:ro"),
                "-v".to_owned(),
                format!("{state}:/state"),
                self.image.clone(),
                "sh".to_owned(),
                "-c".to_owned(),
                script,
                "--".to_owned(),
                command.to_owned(),
            ],
        )
        .await
    }

    async fn run_copy_diff(
        &self,
        temp: &TemporaryTree,
        container_name: &str,
        command: &str,
        cwd: &Path,
    ) -> TwinOutcome {
        let snapshot = temp.path.join("snapshot");
        if fs::create_dir_all(&snapshot).is_err() || copy_workspace(&self.workspace_root, &snapshot).await.is_err() {
            return TwinOutcome::NeedsHuman(Reason::Opaque);
        }

        let snapshot_mount = snapshot.to_string_lossy().to_string();
        let workdir = self.container_workdir(cwd);
        let output = self
            .run_docker(
                container_name,
                [
                    "run".to_owned(),
                    "--rm".to_owned(),
                    "--name".to_owned(),
                    container_name.to_owned(),
                    "--network=none".to_owned(),
                    "--pids-limit=256".to_owned(),
                    "-v".to_owned(),
                    format!("{snapshot_mount}:/work"),
                    "-w".to_owned(),
                    workdir,
                    self.image.clone(),
                    "sh".to_owned(),
                    "-c".to_owned(),
                    command.to_owned(),
                ],
            )
            .await;
        match output {
            Ok(_) => match effects_from_recursive_diff(&self.workspace_root, &snapshot) {
                Ok(effects) => TwinOutcome::Effects(effects),
                Err(_) => TwinOutcome::NeedsHuman(Reason::Opaque),
            },
            Err(TwinError::Timeout) => TwinOutcome::NeedsHuman(Reason::TwinTimeout),
            Err(TwinError::Unavailable) => TwinOutcome::NeedsHuman(Reason::Opaque),
        }
    }

    async fn run_docker<I>(&self, container_name: &str, args: I) -> Result<Output, TwinError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut docker = Command::new("docker");
        docker.args(args);
        docker.stdout(Stdio::piped());
        docker.stderr(Stdio::piped());
        let child = docker.spawn().map_err(|_| TwinError::Unavailable)?;
        match timeout(TWIN_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(_)) => Err(TwinError::Unavailable),
            Err(_) => {
                let _ = Command::new("docker")
                    .arg("kill")
                    .arg(container_name)
                    .output()
                    .await;
                Err(TwinError::Timeout)
            }
        }
    }

    fn container_workdir(&self, cwd: &Path) -> String {
        cwd.strip_prefix(&self.workspace_root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| format!("/work/{}", relative.display()))
            .unwrap_or_else(|| "/work".to_owned())
    }
}

impl TwinExecutor for DockerTwin {
    fn speculate<'a>(
        &'a self,
        command: &'a SimpleCommand,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = TwinOutcome> + Send + 'a>> {
        Box::pin(async move { self.speculate_inner(command, cwd).await })
    }
}

enum TwinError {
    Timeout,
    Unavailable,
}

struct TemporaryTree {
    path: PathBuf,
}

impl TemporaryTree {
    fn new(prefix: &str) -> io::Result<Self> {
        let sequence = NEXT_TWIN.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{sequence}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TemporaryTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn render_command(command: &SimpleCommand) -> String {
    let mut parts = command
        .env
        .iter()
        .map(|(key, value)| format!("{key}={}", shell_quote(value)))
        .collect::<Vec<_>>();
    parts.extend(command.argv.iter().map(|argument| shell_quote(argument)));
    for path in &command.redirect_reads {
        parts.push(format!("< {}", shell_quote(&path.to_string_lossy())));
    }
    for path in &command.redirect_writes {
        parts.push(format!("> {}", shell_quote(&path.to_string_lossy())));
    }
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn container_name(kind: &str) -> String {
    let sequence = NEXT_TWIN.fetch_add(1, Ordering::Relaxed);
    format!("chaostwin-{kind}-{}-{sequence}", std::process::id())
}

fn overlay_unavailable(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains(OVERLAY_UNAVAILABLE)
        || stderr.contains("Read-only file system")
        || stderr.contains("Operation not permitted")
        || stderr.contains("Invalid argument")
        || stderr.contains("not supported")
}

fn effects_from_upperdir(upperdir: PathBuf, workspace_root: &Path) -> io::Result<Effects> {
    let mut effects = Effects {
        op: OpClass::Run,
        ..Effects::default()
    };
    collect_upperdir_effects(&upperdir, &upperdir, workspace_root, &mut effects)?;
    detect_dependency_change(&mut effects);
    Ok(effects)
}

fn collect_upperdir_effects(
    root: &Path,
    current: &Path,
    workspace_root: &Path,
    effects: &mut Effects,
) -> io::Result<()> {
    if !current.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let relative_parent = path
            .parent()
            .and_then(|parent| parent.strip_prefix(root).ok())
            .unwrap_or_else(|| Path::new(""));
        if let Some(deleted) = file_name.strip_prefix(".wh.") {
            if deleted != ".wh..opq" {
                effects.deletes.push(workspace_root.join(relative_parent).join(deleted));
            }
            continue;
        }
        let file_type = entry.file_type()?;
        #[cfg(unix)]
        if file_type.is_char_device() && fs::metadata(&path)?.rdev() == 0 {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            effects.deletes.push(workspace_root.join(relative));
            continue;
        }
        if file_type.is_dir() {
            collect_upperdir_effects(root, &path, workspace_root, effects)?;
        } else {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            effects.writes.push(workspace_root.join(relative));
        }
    }
    Ok(())
}

async fn copy_workspace(workspace_root: &Path, snapshot: &Path) -> io::Result<()> {
    let status = Command::new("cp")
        .arg("-R")
        .arg(workspace_root.join("."))
        .arg(snapshot)
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("cp -R snapshot failed"))
    }
}

fn effects_from_recursive_diff(workspace_root: &Path, snapshot: &Path) -> io::Result<Effects> {
    let original = walk_files(workspace_root)?;
    let copied = walk_files(snapshot)?;
    let mut effects = Effects {
        op: OpClass::Run,
        ..Effects::default()
    };

    for (relative, copied_path) in &copied {
        match original.get(relative) {
            Some(original_path) if same_file(original_path, copied_path)? => {}
            _ => effects.writes.push(workspace_root.join(relative)),
        }
    }
    for relative in original.keys() {
        if !copied.contains_key(relative) {
            effects.deletes.push(workspace_root.join(relative));
        }
    }
    detect_dependency_change(&mut effects);
    Ok(effects)
}

fn walk_files(root: &Path) -> io::Result<std::collections::HashMap<PathBuf, PathBuf>> {
    let mut files = std::collections::HashMap::new();
    walk_files_inner(root, root, &mut files)?;
    Ok(files)
}

fn walk_files_inner(
    root: &Path,
    current: &Path,
    files: &mut std::collections::HashMap<PathBuf, PathBuf>,
) -> io::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_files_inner(root, &path, files)?;
        } else {
            files.insert(path.strip_prefix(root).unwrap_or(&path).to_path_buf(), path);
        }
    }
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> io::Result<bool> {
    let left_metadata = fs::metadata(left)?;
    let right_metadata = fs::metadata(right)?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    Ok(fs::read(left)? == fs::read(right)?)
}

fn detect_dependency_change(effects: &mut Effects) {
    const DEPENDENCY_FILES: &[&str] = &[
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "package-lock.json",
        "pyproject.toml",
        "requirements.txt",
        "uv.lock",
        "pnpm-lock.yaml",
    ];
    let changed = effects
        .writes
        .iter()
        .chain(&effects.deletes)
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| DEPENDENCY_FILES.contains(&name))
        })
        .cloned();
    if let Some(manifest) = changed {
        effects.dep_change = Some(DepChange {
            manifest,
            summary: "twin observed manifest or lockfile change".to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires Docker Desktop and an alpine image"]
    async fn docker_twin_observes_created_file() {
        let root = std::env::temp_dir().join(format!("chaostwin-twin-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let twin = DockerTwin::new(root.clone());
        let command = SimpleCommand {
            argv: vec!["touch".to_owned(), "twin-created.txt".to_owned()],
            redirect_writes: Vec::new(),
            redirect_reads: Vec::new(),
            env: Default::default(),
            operator_after: None,
        };

        let outcome = twin.speculate(&command, &root).await;
        match outcome {
            TwinOutcome::Effects(effects) => {
                assert!(effects.writes.contains(&root.join("twin-created.txt")));
            }
            TwinOutcome::NeedsHuman(reason) => panic!("twin did not run: {reason:?}"),
        }
        let _ = fs::remove_dir_all(root);
    }
}
