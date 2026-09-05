//! agent-block CLI entry point.
//!
//! Parses command-line arguments, optionally enters the sandbox, and launches
//! the Host. The binary is intentionally thin — all logic lives in Lua scripts.

mod blocks;
mod mcp_serve;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

use agent_block_core::host::{PromptSource, ScriptSource, SecretKeySource};
use agent_block_core::sandbox::{self, SandboxConfig};
use agent_block_core::{run, BlockConfig};
use agent_block_mcp::DEFAULT_RPC_TIMEOUT;

#[derive(Parser, Debug)]
#[command(
    name = "agent-block",
    about = "Single-purpose agent building block with built-in mesh communication"
)]
struct Cli {
    /// Subcommand. Omit to run a single script via `-s` — the original and
    /// still the default form.
    #[command(subcommand)]
    command: Option<Command>,

    /// Lua script path. Required unless `--block` or a subcommand is given.
    #[arg(short = 's', long, conflicts_with = "block")]
    script: Option<PathBuf>,

    /// Run a registered block by name instead of a script by path.
    ///
    /// Resolved against `<project>/blocks/` then `$AGENT_BLOCK_HOME/blocks/`
    /// (`<name>.lua` or `<name>/init.lua`), the same registry `agent-block
    /// mcp` serves — so `--block summarize` here and `run_block` with
    /// `block = "summarize"` there run the same file.
    #[arg(short = 'b', long, value_name = "NAME")]
    block: Option<String>,

    /// Relay URL (optional; mesh features disabled if not set)
    #[arg(short = 'r', long)]
    relay: Option<String>,

    /// Ed25519 secret key (64 hex chars) for mesh identity. If omitted, a
    /// random keypair is generated. Env: `AGENT_BLOCK_MESH_SECRET_KEY`.
    #[arg(long, env = "AGENT_BLOCK_MESH_SECRET_KEY")]
    secret_key: Option<String>,

    /// Project root directory
    ///
    /// `global` so it can be written on either side of a subcommand: it means
    /// the same thing for a single script and for a served block, and having to
    /// remember which side it goes on is a usage error waiting to happen.
    #[arg(short = 'p', long, default_value = ".", global = true)]
    project: PathBuf,

    /// Per-RPC timeout for MCP round-trips (seconds). Must be > 0.
    /// Applied uniformly to connect / list_tools / call_tool.
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..), global = true)]
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

    /// Run inside an OS-level execution boundary (Linux only).
    ///
    /// Filesystem writes are confined to the project root, `AGENT_BLOCK_HOME`,
    /// `/tmp`, a few `/dev` nodes, and `AGENT_BLOCK_SANDBOX_FS_RW`; io_uring is
    /// denied. Reads and executes are unrestricted. The boundary is inherited
    /// by `sh.exec` and `mcp.connect` child processes. Set
    /// `AGENT_BLOCK_SANDBOX_TCP=0` to also deny TCP.
    ///
    /// Env: `AGENT_BLOCK_SANDBOX` — read manually by `SandboxConfig::from_env`
    /// rather than bound here: clap parses before the project `.env` is loaded
    /// and its bool binding accepts only literal `true`/`false`, while the
    /// manual read supports the documented truthy/falsy set (`1`, `yes`, …).
    #[arg(long)]
    sandbox: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Serve the registered blocks to an MCP client over stdio.
    ///
    /// Register it with an MCP client the way any stdio server is registered.
    /// `<project>/blocks/` and `$AGENT_BLOCK_HOME/blocks/` are served when
    /// they exist; `--block-dir` adds more:
    ///
    /// ```json
    /// { "command": "agent-block", "args": ["mcp", "--project", "/path/to/project"] }
    /// ```
    Mcp(mcp_serve::McpArgs),
}

// Deliberately *not* `#[tokio::main]`: the sandbox has to be installed before
// any worker thread exists (see `startup` below).
fn main() {
    if let Err(err) = startup() {
        // Human-readable one-line summary + cause chain, instead of anyhow's
        // default `{:?}` Debug dump. Keeps the non-zero exit code contract.
        eprintln!("error: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("caused by: {cause}");
        }
        std::process::exit(1);
    }
}

/// Synchronous startup path: parse → sandbox → build runtime → run.
///
/// The ordering here is load-bearing. Landlock's `restrict_self` and the seccomp
/// filter cover the calling thread plus everything spawned afterwards, so the
/// sandbox must be applied before the tokio runtime creates its worker threads
/// — hence a hand-built runtime instead of the `#[tokio::main]` macro.
fn startup() -> anyhow::Result<()> {
    // rustls 0.23+ requires an explicit CryptoProvider install when multiple
    // (or zero) backends are compiled in. tokio-tungstenite + reqwest pull
    // rustls transitively; without this the first WSS connect panics.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Parsed before tracing is installed because the destination depends on the
    // subcommand: `mcp` speaks JSON-RPC on stdout, so a log line written there
    // is a protocol violation, not noise. Nothing logs before this point.
    let cli = Cli::parse();
    let log_to_stderr = matches!(cli.command, Some(Command::Mcp(_)));

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if log_to_stderr {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    // The SDK host loads `{project}/.env` as well, but that happens far too late
    // for the sandbox knobs. dotenvy never overwrites an already-set variable,
    // so loading it twice is equivalent to loading it once. A missing `.env` is
    // not an error (same fail-silent semantics as the host-side load).
    let _ = dotenvy::from_path(cli.project.join(".env"));

    let sandbox_config = SandboxConfig::from_env(cli.sandbox);
    if sandbox_config.enabled {
        sandbox::apply(&sandbox_config, &cli.project)
            .context("failed to enter sandbox mode (--sandbox / AGENT_BLOCK_SANDBOX)")?;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the tokio runtime")?;

    runtime.block_on(run_cli(cli))
}

async fn run_cli(cli: Cli) -> anyhow::Result<()> {
    let mcp_rpc_timeout = cli
        .mcp_timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RPC_TIMEOUT);

    if let Some(Command::Mcp(args)) = cli.command {
        return mcp_serve::serve(args, &cli.project, mcp_rpc_timeout).await;
    }

    // clap cannot mark `script` required now that `--block` or a subcommand
    // can stand in for it, so the check moves here. The message names the
    // spellings because it replaces clap's own missing-argument error, which
    // named `--script`.
    let script = match (cli.script, cli.block) {
        (Some(path), None) => path,
        (None, Some(name)) => {
            let registered = blocks::scan(&blocks::dirs(&cli.project, &[]));
            blocks::find(&registered, &name)
                .map(|b| b.path.clone())
                .with_context(|| {
                    format!(
                        "unknown block '{name}'; registered: [{}] (looked in <project>/blocks/ \
                         and $AGENT_BLOCK_HOME/blocks/)",
                        blocks::names(&registered)
                    )
                })?
        }
        (Some(_), Some(_)) => {
            // clap's `conflicts_with` should make this unreachable.
            anyhow::bail!("--script and --block are mutually exclusive");
        }
        (None, None) => anyhow::bail!(
            "no script given: pass -s/--script <PATH>, -b/--block <NAME>, or use a subcommand (see --help)"
        ),
    };

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

    let mut builder = BlockConfig::builder(ScriptSource::Path(script), cli.project)
        .mcp_rpc_timeout(mcp_rpc_timeout);
    if let Some(relay) = cli.relay {
        builder = builder.relay_url(relay);
    }
    if let Some(secret_key) = cli.secret_key {
        builder = builder.secret_key(SecretKeySource::Inline(secret_key));
    }
    if let Some(prompt) = prompt {
        builder = builder.prompt(prompt);
    }
    if let Some(context) = context {
        builder = builder.context(context);
    }
    let config = builder.build();

    Ok(run(config).await?)
}
