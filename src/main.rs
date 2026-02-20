use std::sync::Arc;
use tokio::sync::RwLock;

use clap::Parser;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use frama_c_mcp_server::frama_c::client::FramaCClient;
use frama_c_mcp_server::mcp::server::FramaCMcpServer;
use frama_c_mcp_server::state::SessionState;

#[derive(clap::Parser)]
#[command(name = "frama-c-mcp-server")]
#[command(about = "MCP server for Frama-C formal verification")]
struct Cli {
    /// Unix socket path of a running Frama-C server
    #[arg(long)]
    socket: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let state = Arc::new(RwLock::new(SessionState::default()));

    tracing::info!("connecting to Frama-C server at {}", cli.socket);
    let client = FramaCClient::connect(&cli.socket, state.clone()).await?;
    tracing::info!("connected, project loaded");

    let server = FramaCMcpServer::new(client, state);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    tracing::info!("MCP server running on stdio");
    service.waiting().await?;

    Ok(())
}
