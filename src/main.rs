//! agent-block CLI entry point.
//!
//! Parses command-line arguments and launches the Host.
//! The binary is intentionally thin — all logic lives in Lua scripts.

use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

use crate::host::{run, BlockConfig};
use crate::mcp_client::DEFAULT_RPC_TIMEOUT;

mod bridge;
mod error;
mod host;
mod mcp_client;

#[derive(Parser, Debug)]
#[command(
    name = "agent-block",
    about = "Single-purpose agent building block with built-in mesh communication"
)]
struct Cli {
    /// Lua script path
    #[arg(short = 's', long)]
    script: PathBuf,

    /// Relay URL (optional; mesh features disabled if not set)
    #[arg(short = 'r', long)]
    relay: Option<String>,

    /// Project root directory
    #[arg(short = 'p', long, default_value = ".")]
    project: PathBuf,

    /// Per-RPC timeout for MCP round-trips (seconds). Must be > 0.
    /// Applied uniformly to connect / list_tools / call_tool.
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    mcp_timeout_secs: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let mcp_rpc_timeout = cli
        .mcp_timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RPC_TIMEOUT);

    let config = BlockConfig {
        script_path: cli.script,
        project_root: cli.project,
        relay_url: cli.relay,
        mcp_rpc_timeout,
    };

    Ok(run(config).await?)
}
