use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Select the daemon socket consistently for every local Chaos Twin client.
pub fn default_socket_path() -> PathBuf {
    env::var_os("CHAOSTWIN_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(fallback_socket_path)
}

/// Use the XDG runtime directory when present, otherwise use the real UID.
fn fallback_socket_path() -> PathBuf {
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("chaostwin.sock");
    }
    PathBuf::from(format!("/tmp/chaostwin-{}.sock", current_uid()))
}

fn current_uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|uid| uid.trim().to_owned())
        .filter(|uid| !uid.is_empty())
        .unwrap_or_else(|| "0".to_owned())
}
