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

    let socket_path = socket_override.unwrap_or_else(default_socket_path);
    let state = AppState::new(DaemonClient::new(socket_path.clone()));
    let addr: SocketAddr = addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!(
        "tractus-console on http://{addr}  ·  daemon socket {}",
        socket_path.display()
    );
    axum::serve(listener, app(state)).await?;
    Ok(())
}

fn print_usage() {
    println!("usage: tractus-console [--addr <host:port>] [--sock <path>] [--workspace <path>]");
    println!();
    println!("Serves the Tractus firewall dashboard and control-plane API.");
    println!("Defaults: --addr {DEFAULT_ADDR}, socket auto-selected from the workspace.");
}
