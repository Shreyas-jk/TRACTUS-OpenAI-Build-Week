mod handoff;
mod server;
mod state;
mod twin;

use crate::server::{bind_default_listener, serve, ServerConfig};
use crate::state::shared_state;
use crate::twin::NoTwin;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let listener = bind_default_listener().expect("bind Chaos Twin Unix socket");
    let workspace_root = std::env::current_dir().expect("read workspace root");
    let config = Arc::new(ServerConfig::new(
        shared_state(),
        workspace_root,
        Arc::new(NoTwin),
    ));

    if let Err(error) = serve(listener, config).await {
        tracing::error!(%error, "chaosd stopped");
    }
}
