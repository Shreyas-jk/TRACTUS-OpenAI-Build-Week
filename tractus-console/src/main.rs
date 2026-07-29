//! `tractus-console` — serve the firewall dashboard and control-plane API.

use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process;
use tractus_console::daemon::{default_socket_path, DaemonClient};
use tractus_console::server::{app, AppState};

const DEFAULT_ADDR: &str = "127.0.0.1:8787";

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("tractus-console: {error}");
        process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let mut addr = DEFAULT_ADDR.to_owned();
    let mut socket_override: Option<PathBuf> = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--addr" => addr = arguments.next().ok_or("--addr requires an address")?,
            "--sock" => {
                socket_override = Some(PathBuf::from(
                    arguments.next().ok_or("--sock requires a path")?,
                ))
            }
            "--workspace" => {
                let workspace = arguments.next().ok_or("--workspace requires a path")?;
                std::env::set_var("TRACTUS_WORKSPACE_ROOT", workspace);
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }

    // Pick up OPENAI_API_KEY (and model overrides) from a local .env so the
    // operator never has to export them by hand. Real environment wins.
    let loaded = load_dotenv();

    let socket_path = socket_override.unwrap_or_else(default_socket_path);
    let state = AppState::new(DaemonClient::new(socket_path.clone()));
    let addr: SocketAddr = addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    println!("tractus-console on http://{addr}");
    println!("  daemon socket: {}", socket_path.display());
    if !loaded.is_empty() {
        println!("  loaded from .env: {}", loaded.join(", "));
    }
    if std::env::var("OPENAI_API_KEY").is_ok_and(|key| !key.is_empty()) {
        println!("  intent extraction: enabled (OPENAI_API_KEY set)");
    } else {
        println!("  intent extraction: disabled — set OPENAI_API_KEY or add it to .env");
    }

    axum::serve(listener, app(state)).await?;
    Ok(())
}

/// Load `KEY=VALUE` pairs from `./.env` into the environment without overriding
/// anything already set. Returns the names loaded (never the values). No-ops if
/// there is no `.env`. Supports `#` comments, optional `export ` prefixes, and
/// single/double-quoted values.
fn load_dotenv() -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(".env") else {
        return Vec::new();
    };
    let mut loaded = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().strip_prefix("export ").unwrap_or(key).trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        std::env::set_var(key, value);
        loaded.push(key.to_owned());
    }
    loaded
}

fn print_usage() {
    println!("usage: tractus-console [--addr <host:port>] [--sock <path>] [--workspace <path>]");
    println!();
    println!("Serves the Tractus firewall dashboard and control-plane API.");
    println!("Defaults: --addr {DEFAULT_ADDR}, socket auto-selected from the workspace.");
}
