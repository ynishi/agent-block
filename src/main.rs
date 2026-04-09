//! agent-block CLI entry point.
//!
//! Parses command-line arguments and launches the Host.
//! The binary is intentionally thin — all logic lives in Lua scripts.

use clap::Parser;
use std::path::PathBuf;

use crate::host::{run, BlockConfig};

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

    let config = BlockConfig {
        script_path: cli.script,
        project_root: cli.project,
        relay_url: cli.relay,
    };

    Ok(run(config).await?)
}
