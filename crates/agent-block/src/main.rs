//! agent-block CLI entry point.
//!
//! Parses command-line arguments and launches the Host.
//! The binary is intentionally thin — all logic lives in Lua scripts.

use anyhow::Context as _;
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

use agent_block_core::host::{PromptSource, ScriptSource, SecretKeySource};
use agent_block_core::{run, BlockConfig};
use agent_block_mcp::DEFAULT_RPC_TIMEOUT;

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

    /// Path to a file whose contents are injected as `_PROMPT` Lua global.
    /// Mutually exclusive with `--prompt`.
    #[arg(long, value_name = "FILE", conflicts_with = "prompt")]
    prompt_file: Option<PathBuf>,

    /// Path to a file whose contents are injected as `_CONTEXT` Lua global.
    /// Mutually exclusive with `--context`.
    #[arg(long, value_name = "FILE", conflicts_with = "context")]
    context_file: Option<PathBuf>,
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

    // Map the CLI argument shapes to the SDK `Source` enums. File-backed
    // variants are read eagerly here so the error message carries the
    // CLI flag name (`--prompt-file` / `--context-file`); the SDK side
    // sees the contents directly via `PromptSource::Inline`.
    let prompt = match (cli.prompt, cli.prompt_file) {
        (None, None) => None,
        (Some(s), None) => Some(PromptSource::Inline(s)),
        (None, Some(p)) => {
            let content = std::fs::read_to_string(&p)
                .with_context(|| format!("failed to read --prompt-file '{}'", p.display()))?;
            Some(PromptSource::Inline(content))
        }
        (Some(_), Some(_)) => {
            // clap's `conflicts_with` should make this unreachable.
            anyhow::bail!("--prompt and --prompt-file are mutually exclusive");
        }
    };
    let context = match (cli.context, cli.context_file) {
        (None, None) => None,
        (Some(s), None) => Some(PromptSource::Inline(s)),
        (None, Some(p)) => {
            let content = std::fs::read_to_string(&p)
                .with_context(|| format!("failed to read --context-file '{}'", p.display()))?;
            Some(PromptSource::Inline(content))
        }
        (Some(_), Some(_)) => {
            anyhow::bail!("--context and --context-file are mutually exclusive");
        }
    };

    let config = BlockConfig {
        script: ScriptSource::Path(cli.script),
        project_root: cli.project,
        relay_url: cli.relay,
        secret_key: cli.secret_key.map(SecretKeySource::Inline),
        mcp_rpc_timeout,
        prompt,
        context,
        host_handlers: std::collections::HashMap::new(),
        host_handler: None,
        auto_serve_bus: false,
        shutdown_token: None,
    };

    Ok(run(config).await?)
}
