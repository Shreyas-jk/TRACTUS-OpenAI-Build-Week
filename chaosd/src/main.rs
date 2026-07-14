use chaosd::server::{bind_default_listener, serve, ServerConfig};
use chaosd::state::shared_state;
use chaosd::twin::DockerTwin;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let listener = bind_default_listener().expect("bind Chaos Twin Unix socket");
    let workspace_root = std::env::current_dir().expect("read workspace root");
    let config = Arc::new(ServerConfig::new(
        shared_state(),
        workspace_root.clone(),
        Arc::new(DockerTwin::new(workspace_root)),
    ));

    if let Err(error) = serve(listener, config).await {
        tracing::error!(%error, "chaosd stopped");
    }
}
