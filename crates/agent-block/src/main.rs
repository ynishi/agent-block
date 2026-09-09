//! agent-block CLI entry point.
//!
//! Parses command-line arguments, optionally enters the sandbox, and launches
//! the Host. The binary is intentionally thin — all logic lives in Lua scripts.

mod blocks;
mod mcp_serve;
mod serve;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

use agent_block_core::host::{PromptSource, ScriptSource, SecretKeySource};
use agent_block_core::sandbox::{self, SandboxConfig};
use agent_block_core::{run_capture, BlockConfig, BlockError};
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

    /// Write the value the script returned to this file (the block contract's
    /// JSON string, verbatim). stdout stays what it is — the logs — so a
    /// caller that runs a block in a process of its own, as `agent-block
    /// serve` does, has somewhere to read the answer back from. Written only
    /// when the script returns; a script that raises writes nothing.
    ///
    /// Env: `AGENT_BLOCK_RESULT_PATH`.
    #[arg(long, value_name = "FILE", env = "AGENT_BLOCK_RESULT_PATH")]
    result: Option<PathBuf>,

    /// Label every `knl` session this run opens: `--label run=r-7`, repeated
    /// for more than one.
    ///
    /// What this run is, for a caller that runs the same block over and over
    /// and has to tell the runs apart afterwards — a job manager, most of
    /// all. The labels land on each session's opening (`meta`), so one
    /// project database holds every run and a reader selects the one it
    /// wants (`knl.views.sessions`). The alternative — a database per run —
    /// answers the same question by breaking the one above it: the project's
    /// log stops being one stream to read.
    ///
    /// Values are read as JSON when they parse as a scalar (`n=2`,
    /// `retried=true`) and as text otherwise, which is the same vocabulary
    /// `meta` takes everywhere else.
    ///
    /// **No env binding, deliberately.** This names one run, and an
    /// environment variable would be inherited by every process the block
    /// starts — each of them then claiming to be the run its parent is.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    labels: Vec<String>,
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
    /// Run the job manager: start the blocks that declare a `job.toml` on
    /// their interval, each in a process of its own, and answer
    /// submit / list / stop / read over a loopback HTTP listener.
    ///
    /// One long-lived process per machine is the shape: the unit a service
    /// manager holds is this one, and a lane adds or removes a job by adding
    /// or removing a file beside its block.
    Serve(serve::ServeArgs),
}

// Deliberately *not* `#[tokio::main]`: the sandbox has to be installed before
// any worker thread exists (see `startup` below).
fn main() {
    if let Err(err) = startup() {
        // A block that looked at what it needs and did not start
        // (`job.defer(reason)`): one line with the reason, and the exit
        // code the job manager reads as `deferred` — `EX_TEMPFAIL` from
        // sysexits(3), "temporary failure; the user is invited to retry".
        // Not 1, which the manager reads as `failed`, and which this is not.
        if let Some(BlockError::Deferred(reason)) = err.downcast_ref::<BlockError>() {
            eprintln!("deferred: {reason}");
            std::process::exit(EX_TEMPFAIL);
        }
        // Human-readable one-line summary + cause chain, instead of anyhow's
        // default `{:?}` Debug dump. Keeps the non-zero exit code contract.
        eprintln!("error: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("caused by: {cause}");
        }
        std::process::exit(1);
    }
}

/// sysexits(3) `EX_TEMPFAIL`. The same number `blocks/lib/job/init.lua`
/// publishes as `job.DEFERRED_EXIT` and reads back as `outcome = "deferred"`.
const EX_TEMPFAIL: i32 = 75;

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
    // stdio MCP owns stdout; a long-lived manager's logs are what a service
    // manager collects, and stderr is where it looks.
    let log_to_stderr = matches!(cli.command, Some(Command::Mcp(_)) | Some(Command::Serve(_)));

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

    match cli.command {
        Some(Command::Mcp(args)) => {
            return mcp_serve::serve(args, &cli.project, mcp_rpc_timeout).await;
        }
        Some(Command::Serve(args)) => {
            return serve::serve(args, &cli.project, mcp_rpc_timeout).await;
        }
        None => {}
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
    for (key, value) in parse_labels(&cli.labels)? {
        builder = builder.session_label(key, value);
    }
    let config = builder.build();

    let value = run_capture(config).await?;
    if let Some(path) = cli.result {
        std::fs::write(&path, &value)
            .with_context(|| format!("writing the script's result to '{}'", path.display()))?;
    }
    Ok(())
}

/// Read `--label key=value` pairs into the labels a session opens with.
///
/// The value is JSON when it parses as a scalar and text otherwise, so
/// `n=2` and `retried=true` are a number and a flag while `run=r-7` and
/// `note=2 items` are text. That is `meta`'s own vocabulary — a string, a
/// number or a flag — and nothing here can produce anything deeper.
///
/// A pair with no `=` is a usage error rather than a label with an empty
/// value: naming a key and forgetting the value is the likely mistake, and
/// recording it as `""` would hide it in the log.
fn parse_labels(pairs: &[String]) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
    pairs
        .iter()
        .map(|pair| {
            let (key, value) = pair.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("--label takes key=value, got '{pair}' (no '=' in it)")
            })?;
            if key.is_empty() {
                anyhow::bail!("--label takes key=value, got '{pair}' (the key is empty)");
            }
            let value = match serde_json::from_str::<serde_json::Value>(value) {
                Ok(v @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_))) => v,
                _ => serde_json::Value::from(value),
            };
            Ok((key.to_string(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_labels;
    use serde_json::Value;

    /// A label is a string, a number or a flag — `meta`'s own vocabulary —
    /// and which one it is comes from the text, not from a second flag the
    /// caller has to remember.
    #[test]
    fn a_label_value_is_read_as_the_scalar_it_looks_like() {
        let labels =
            parse_labels(&["run=r-7".into(), "n=2".into(), "retried=true".into()]).expect("labels");
        assert_eq!(
            labels,
            vec![
                ("run".to_string(), Value::from("r-7")),
                ("n".to_string(), Value::from(2)),
                ("retried".to_string(), Value::from(true)),
            ]
        );
    }

    /// Anything that is not a scalar stays the text it was: a value that
    /// happens to look like JSON structure is a label, not a shape, because
    /// `meta` is shallow by rule.
    #[test]
    fn a_label_value_that_is_not_a_scalar_stays_text() {
        let labels = parse_labels(&[
            "note=2 items".into(),
            "shape={\"a\":1}".into(),
            "path=/tmp/x".into(),
            "empty=".into(),
        ])
        .expect("labels");
        assert_eq!(
            labels,
            vec![
                ("note".to_string(), Value::from("2 items")),
                ("shape".to_string(), Value::from("{\"a\":1}")),
                ("path".to_string(), Value::from("/tmp/x")),
                ("empty".to_string(), Value::from("")),
            ]
        );
    }

    /// A key with no value is the caller's mistake, said out loud: recording
    /// it as an empty label would bury it in the log instead.
    #[test]
    fn a_pair_without_a_value_is_refused() {
        let err = parse_labels(&["run".into()]).expect_err("no '=' in it");
        assert!(err.to_string().contains("key=value"), "{err}");
        let err = parse_labels(&["=r-7".into()]).expect_err("the key is empty");
        assert!(err.to_string().contains("the key is empty"), "{err}");
    }
}
