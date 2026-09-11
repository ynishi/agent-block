//! agent-block CLI entry point.
//!
//! Parses command-line arguments, optionally enters the sandbox, and launches
//! the Host. The binary is intentionally thin — all logic lives in Lua scripts.

mod blocks;
mod knl;
mod mcp_serve;
mod serve;
mod vendor;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
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
    /// Resolved against `<project>/.agent-block/blocks/`, then
    /// `<project>/blocks/`, then `$AGENT_BLOCK_HOME/blocks/` (`<name>.lua` or
    /// `<name>/init.lua`), the same registry `agent-block mcp` serves — so
    /// `--block summarize` here and `run_block` with `block = "summarize"`
    /// there run the same file.
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
    ///
    /// One run's own input, so it has no env binding: see `--config`.
    #[arg(long)]
    prompt: Option<String>,

    /// Context string injected as `_CONTEXT` Lua global.
    /// Typically used as a system prompt: `agent.run({system = _CONTEXT, ...})`.
    ///
    /// One run's own input, so it has no env binding: see `--config`.
    #[arg(short = 'c', long)]
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
    /// One run's own output, so it has no env binding: see `--config`.
    #[arg(long, value_name = "FILE")]
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

    /// A JSON file holding this run's own inputs: `prompt`, `context`,
    /// `result`, `labels`.
    ///
    /// One argument instead of four, for a caller that starts runs — a job
    /// manager writes the file and names it here. Free text goes in it
    /// unescaped, which a command line cannot promise: a prompt with quotes
    /// or newlines in it survives a file and not a shell.
    ///
    /// The lowest layer of the three: a value in the file is used when
    /// neither the flag nor (for the knobs that have one) the environment
    /// gave one. So **file, then environment, then argument** — the later
    /// one wins.
    ///
    /// It carries what belongs to ONE run, and nothing that belongs to the
    /// host. Where the databases live, the sandbox, `AGENT_BLOCK_HOME` —
    /// those stay environment variables read from the project's `.env`,
    /// which is that half's config file already.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
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
    /// Read the kernel's log from outside the process that wrote it.
    ///
    /// A run's facts land in the project's kernel database, and every reader
    /// so far has been Lua running inside the host. This is the door for the
    /// reader that is not: a session id in, JSON Lines out.
    ///
    /// ```text
    /// agent-block knl export --session <ID> --as events|messages
    /// ```
    Knl(knl::KnlArgs),
    /// Write an embedded module into the project, to edit as its own.
    ///
    /// The copy lands in `<project>/.agent-block/lib/`, the first tier the
    /// require path searches, so it is what `require("<name>")` resolves from
    /// then on. The original stays reachable as `require("embedded.<name>")`.
    ///
    /// ```text
    /// agent-block vendor [--path <dir>] [--force] <name>...
    /// agent-block vendor --list
    /// ```
    Vendor(vendor::VendorArgs),
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
    // manager collects, and stderr is where it looks; `knl export` writes JSON
    // Lines there, where a log line would be a malformed record; and `vendor`
    // writes the paths it wrote, which a caller may well read as a list.
    let log_to_stderr = matches!(
        cli.command,
        Some(Command::Mcp(_))
            | Some(Command::Serve(_))
            | Some(Command::Knl(_))
            | Some(Command::Vendor(_))
    );

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
        Some(Command::Knl(args)) => {
            return knl::run(args, &cli.project).await;
        }
        Some(Command::Vendor(args)) => {
            return vendor::run(args, &cli.project);
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
                        "unknown block '{name}'; registered: [{}] (looked in \
                         <project>/.agent-block/blocks/, <project>/blocks/ and \
                         $AGENT_BLOCK_HOME/blocks/)",
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

    // The lowest of the three layers. Read before the argument shapes below
    // so a value it carries stands in exactly where the command line gave
    // none — the file loses to a flag, and never the other way round.
    let file = read_run_config(cli.config.as_deref())?;
    let cli_prompt = cli.prompt.or(file.prompt);
    let cli_context = cli.context.or(file.context);
    let result_path = cli.result.or(file.result);

    // Map the CLI argument shapes to the SDK `Source` enums. File-backed
    // variants are read eagerly here so the error message carries the
    // CLI flag name (`--prompt-file` / `--context-file`); the SDK side
    // sees the contents directly via `PromptSource::Inline`.
    let prompt = match (cli_prompt, cli.prompt_file) {
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
    let context = match (cli_context, cli.context_file) {
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
    for (key, value) in file.labels {
        builder = builder.session_label(key, value);
    }
    for (key, value) in parse_labels(&cli.labels)? {
        builder = builder.session_label(key, value);
    }
    let config = builder.build();

    let value = run_capture(config).await?;
    if let Some(path) = result_path {
        std::fs::write(&path, &value)
            .with_context(|| format!("writing the script's result to '{}'", path.display()))?;
    }
    Ok(())
}

/// One run's own inputs, as a file: what `--config` names.
///
/// The three that used to be environment variables (`AGENT_BLOCK_PROMPT` /
/// `_CONTEXT` / `_RESULT_PATH`) and the labels beside them. They belong
/// together because they are all answers to "which run is this" — and they
/// left the environment for the same reason: an environment variable is
/// inherited, so a block that starts another `agent-block` handed its child
/// its own prompt and its own result file to write over.
///
/// Closed (`deny_unknown_fields`): a misspelled key is a run that would have
/// silently gone without its prompt.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RunConfig {
    /// `_PROMPT`.
    #[serde(default)]
    prompt: Option<String>,
    /// `_CONTEXT`.
    #[serde(default)]
    context: Option<String>,
    /// Where the returned value is written.
    #[serde(default)]
    result: Option<PathBuf>,
    /// What every session this run opens is labelled with.
    #[serde(default)]
    labels: serde_json::Map<String, serde_json::Value>,
}

/// Read the `--config` file, or an empty config when none was named.
fn read_run_config(path: Option<&Path>) -> anyhow::Result<RunConfig> {
    let Some(path) = path else {
        return Ok(RunConfig::default());
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading --config '{}'", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing --config '{}' as JSON", path.display()))
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
