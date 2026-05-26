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
mod bus;
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

    /// Ed25519 secret key (64 hex chars) for mesh identity. If omitted, a
    /// random keypair is generated. Env: `AGENT_BLOCK_MESH_SECRET_KEY`.
    #[arg(long, env = "AGENT_BLOCK_MESH_SECRET_KEY")]
    secret_key: Option<String>,

    /// Project root directory
    #[arg(short = 'p', long, default_value = ".")]
    project: PathBuf,

    /// Per-RPC timeout for MCP round-trips (seconds). Must be > 0.
    /// Applied uniformly to connect / list_tools / call_tool.
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    mcp_timeout_secs: Option<u64>,

    /// Prompt string injected as `_PROMPT` Lua global.
    /// Scripts can use it as `agent.run({prompt = _PROMPT, ...})`.
    /// Env: `AGENT_BLOCK_PROMPT`.
    #[arg(long, env = "AGENT_BLOCK_PROMPT")]
    prompt: Option<String>,

    /// Context string injected as `_CONTEXT` Lua global.
    /// Typically used as a system prompt: `agent.run({system = _CONTEXT, ...})`.
    /// Env: `AGENT_BLOCK_CONTEXT`.
    #[arg(short = 'c', long, env = "AGENT_BLOCK_CONTEXT")]
    context: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls 0.23+ requires an explicit CryptoProvider install when multiple
    // (or zero) backends are compiled in. tokio-tungstenite + reqwest pull
    // rustls transitively; without this the first WSS connect panics.
    let _ = rustls::crypto::ring::default_provider().install_default();

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
        secret_key: cli.secret_key,
        mcp_rpc_timeout,
        prompt: cli.prompt,
        context: cli.context,
    };

    Ok(run(config).await?)
}
