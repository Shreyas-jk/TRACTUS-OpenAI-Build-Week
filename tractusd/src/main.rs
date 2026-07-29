use std::sync::Arc;
use tractusd::server::{bind_default_listener, serve, ServerConfig};
use tractusd::state::shared_state;
use tractusd::twin::PooledTwin;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let listener = bind_default_listener().expect("bind Tractus Unix socket");
    let workspace_root = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonicalize workspace root");
    let twin = PooledTwin::new(workspace_root.clone());
    twin.start();
    let config = Arc::new(ServerConfig::new(
        shared_state(),
        workspace_root,
        Arc::new(twin),
    ));

    if let Err(error) = serve(listener, config).await {
        tracing::error!(%error, "tractusd stopped");
    }
}
