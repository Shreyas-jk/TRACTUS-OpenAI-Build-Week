use chaosd::server::{bind_default_listener, serve, ServerConfig};
use chaosd::state::shared_state;
use chaosd::twin::PooledTwin;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let listener = bind_default_listener().expect("bind Tractus Unix socket");
    let workspace_root = std::env::current_dir().expect("read workspace root");
    let twin = PooledTwin::new(workspace_root.clone());
    twin.start();
    let config = Arc::new(ServerConfig::new(
        shared_state(),
        workspace_root,
        Arc::new(twin),
    ));

    if let Err(error) = serve(listener, config).await {
        tracing::error!(%error, "chaosd stopped");
    }
}
