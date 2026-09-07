//! `agent-block serve` — the thin job manager, as a process.
//!
//! What runs here is one embedded block (`serve.lua`) on the ordinary host,
//! with two things the CLI adds around it: the declarations it found beside
//! the registered blocks, handed in as `_JOBS`, and an HTTP listener on the
//! bus (`agent_block_core::bus::http_source`) that the block answers with
//! `bus.on("http", ...)`. The manager itself — what is due, one live run per
//! job, the record — is the `job` Lua module; this file is the entry point
//! and nothing decides here.
//!
//! # A job is a file beside its block
//!
//! `<root>/blocks/<name>/job.toml` for a `<name>/init.lua` block, and
//! `<root>/blocks/<name>.job.toml` for a `<name>.lua` one:
//!
//! ```toml
//! every   = "2m"      # since the previous run ended; omit to run only on request
//! timeout = "10m"     # the run's group is killed at this; default 10m
//! prompt  = "..."     # what `_PROMPT` is in the block; optional
//! context = "..."     # what `_CONTEXT` is; optional
//! block   = "other"   # run a different block under this job name; optional
//! ```
//!
//! Adding the file adds the job on the next start; removing the file removes
//! it. The run is started in `<root>` — the block's project root, where
//! `agent-block` loads `.env` from — so a lane's credentials are the lane's
//! and never the manager's. Declarations are read once at start: a changed
//! set is a restart, which is what a service manager does anyway.
//!
//! # The listener
//!
//! Bound to `127.0.0.1:7788` unless `--bind` says otherwise. A bearer token
//! is minted into `$AGENT_BLOCK_HOME/serve.token` (mode 0600) on first start
//! and required on every request, loopback included; the `Host` / `Origin`
//! checks are the listener's (see the source module). Reaching it from
//! another machine is a tunnel to the loopback port by default; a wider
//! `--bind` is the opt-in, and the token is what stands between it and the
//! network. The routes are the block's:
//!
//! ```text
//! GET    /jobs                 the declarations, with each job's last end and live run
//! GET    /runs?job=&limit=     runs, newest first
//! GET    /runs/<id>            one run, with the tail of its stderr
//! POST   /jobs/<name>/runs     ask for a run now (202; the next tick starts it)
//! DELETE /runs/<id>            ask a live run to stop (202; the next tick kills it)
//! ```
//!
//! # What the manager keeps
//!
//! Its own session log, `$AGENT_BLOCK_HOME/serve.sqlite`: every run's start
//! and end is a record on it, and every decision is read back off it. The
//! host closes a session when its process ends and a closed session cannot
//! be resumed, so each start opens a session of its own and reads across all
//! of them — their ids are the lines of `serve.session`, newest last. Each
//! run writes its own log under `$AGENT_BLOCK_HOME/runs/<job>/<run_id>.sqlite`.
//! Nothing else is state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use agent_block_core::bus::http_source::HttpSourceConfig;
use agent_block_core::host::{run, BlockConfig, ScriptSource};

use crate::blocks::{self, Block};

const SERVE_LUA: &str = include_str!("serve.lua");

/// Where the listener binds when `--bind` is not given: loopback, one port.
pub const DEFAULT_BIND: &str = "127.0.0.1:7788";

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Extra block directories, beyond `<project>/blocks/` and
    /// `$AGENT_BLOCK_HOME/blocks/`.
    #[arg(long = "block-dir", value_name = "DIR")]
    pub block_dirs: Vec<PathBuf>,
    /// The address the HTTP listener binds. Loopback by default; binding
    /// wider exposes the listener to whoever can reach the port, behind the
    /// token.
    #[arg(long, default_value = DEFAULT_BIND, value_name = "ADDR")]
    pub bind: String,
    /// How often the manager looks for work, in seconds.
    #[arg(long, default_value_t = 5, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    pub tick_secs: u64,
    /// How many runs may be live at once, across all jobs.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..))]
    pub max_runs: u64,
}

/// A duration as `job.toml` may spell it: `"2m"`, or a bare number of
/// seconds. Passed through to Lua as written; `job.decl` is what reads it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Spelled {
    Seconds(f64),
    Text(String),
}

/// The file beside a block, as written. Unknown keys are refused: a
/// misspelt `evry` that silently meant "never" is the mistake a declaration
/// can least afford.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobFile {
    block: Option<String>,
    every: Option<Spelled>,
    timeout: Option<Spelled>,
    prompt: Option<String>,
    context: Option<String>,
}

/// A declaration, resolved: what `_JOBS` carries to the block, one per file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobDecl {
    pub name: String,
    pub block: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub every: Option<Spelled>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Spelled>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Where a block's declaration would be: `job.toml` beside `init.lua`, or
/// `<name>.job.toml` beside `<name>.lua`.
pub fn declaration_path(block: &Block) -> PathBuf {
    if block.path.file_name().and_then(|n| n.to_str()) == Some("init.lua") {
        block.path.with_file_name("job.toml")
    } else {
        block.path.with_extension("job.toml")
    }
}

/// The project root a block was registered under: the parent of the block
/// directory it was found in. `<root>/blocks/<name>/init.lua` and
/// `<root>/blocks/<name>.lua` both answer `<root>`.
pub fn project_root_of(block: &Block) -> PathBuf {
    let blocks_dir = if block.path.file_name().and_then(|n| n.to_str()) == Some("init.lua") {
        block.path.parent().and_then(Path::parent)
    } else {
        block.path.parent()
    };
    blocks_dir
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The declarations among `blocks`: one per block that has a file beside
/// it. A file that does not parse is an error naming it, not a job that
/// silently never runs.
pub fn scan_jobs(blocks: &[Block]) -> Result<Vec<JobDecl>> {
    let mut out = Vec::new();
    for block in blocks {
        let path = declaration_path(block);
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading job declaration {}", path.display()))?;
        let file: JobFile = toml::from_str(&text)
            .with_context(|| format!("parsing job declaration {}", path.display()))?;
        let block_name = file.block.clone().unwrap_or_else(|| block.name.clone());
        let script = if block_name == block.name {
            block.path.clone()
        } else {
            blocks::find(blocks, &block_name)
                .map(|b| b.path.clone())
                .with_context(|| {
                    format!(
                        "job '{}' names block '{}', which is not registered ({})",
                        block.name,
                        block_name,
                        path.display()
                    )
                })?
        };
        out.push(JobDecl {
            name: block.name.clone(),
            block: block_name,
            path: script,
            cwd: project_root_of(block),
            every: file.every,
            timeout: file.timeout,
            prompt: file.prompt,
            context: file.context,
        });
    }
    Ok(out)
}

/// The bearer token: read from `<home>/serve.token`, or minted there (64
/// hex characters, mode 0600) when there is none yet.
fn token_at(home: &Path) -> Result<(String, PathBuf)> {
    let path = home.join("serve.token");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok((trimmed.to_string(), path));
        }
    }
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    std::fs::write(&path, format!("{token}\n"))
        .with_context(|| format!("writing token {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting mode on {}", path.display()))?;
    }
    Ok((token, path))
}

pub async fn serve(args: ServeArgs, project: &Path, mcp_rpc_timeout: Duration) -> Result<()> {
    let project = project
        .canonicalize()
        .with_context(|| format!("project root {}", project.display()))?;
    let registered = blocks::scan(&blocks::dirs(&project, &args.block_dirs));
    let jobs = scan_jobs(&registered)?;

    let home = agent_block_core::bridge::config::base_dir()
        .map_err(anyhow::Error::msg)
        .context("resolving AGENT_BLOCK_HOME")?;
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    let runs_dir = home.join("runs");
    for job in &jobs {
        let dir = runs_dir.join(&job.name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let (token, token_path) = token_at(&home)?;

    let bin = std::env::current_exe().context("locating this executable")?;
    let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
    eprintln!(
        "serve: {} job(s) [{}]; listening on http://{}; token in {}",
        jobs.len(),
        names.join(", "),
        args.bind,
        token_path.display()
    );

    let mut globals: HashMap<String, Value> = HashMap::new();
    globals.insert("_JOBS".to_string(), serde_json::to_value(&jobs)?);
    globals.insert(
        "_SERVE".to_string(),
        json!({
            "store": home.join("serve.sqlite"),
            "session_file": home.join("serve.session"),
            "runs_dir": runs_dir,
            "tick_s": args.tick_secs,
            "max_runs": args.max_runs,
            "bin": bin,
        }),
    );

    let config = BlockConfig::builder(
        ScriptSource::Inline {
            source: SERVE_LUA.to_string(),
            name: "serve.lua".to_string(),
        },
        project,
    )
    .mcp_rpc_timeout(mcp_rpc_timeout)
    .extra_globals(globals)
    .http_source(HttpSourceConfig::new(args.bind).token(token))
    .build();

    run(config).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(path: &Path, name: &str) -> Block {
        Block {
            name: name.to_string(),
            path: path.to_path_buf(),
            doc: String::new(),
        }
    }

    #[test]
    fn a_declaration_sits_beside_its_block_either_way() {
        let dir = block(Path::new("/r/blocks/drain/init.lua"), "drain");
        assert_eq!(
            declaration_path(&dir),
            PathBuf::from("/r/blocks/drain/job.toml")
        );
        assert_eq!(project_root_of(&dir), PathBuf::from("/r"));
        let file = block(Path::new("/r/blocks/sweep.lua"), "sweep");
        assert_eq!(
            declaration_path(&file),
            PathBuf::from("/r/blocks/sweep.job.toml")
        );
        assert_eq!(project_root_of(&file), PathBuf::from("/r"));
    }

    #[test]
    fn scan_reads_the_file_and_refuses_what_it_does_not_know() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let drain = root.join("blocks/drain");
        std::fs::create_dir_all(&drain).unwrap();
        std::fs::write(drain.join("init.lua"), "return '{}'").unwrap();
        std::fs::write(
            drain.join("job.toml"),
            "every = \"2m\"\ntimeout = 30\nprompt = \"go\"\n",
        )
        .unwrap();
        std::fs::write(root.join("blocks/quiet.lua"), "return '{}'").unwrap();

        let registered = blocks::scan(&[root.join("blocks")]);
        let jobs = scan_jobs(&registered).expect("scan");
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.name, "drain");
        assert_eq!(job.block, "drain");
        assert_eq!(job.path, drain.join("init.lua"));
        assert_eq!(job.cwd, root);
        assert_eq!(job.every, Some(Spelled::Text("2m".into())));
        assert_eq!(job.timeout, Some(Spelled::Seconds(30.0)));
        assert_eq!(job.prompt.as_deref(), Some("go"));

        std::fs::write(drain.join("job.toml"), "evry = \"2m\"\n").unwrap();
        let err = scan_jobs(&registered).expect_err("unknown key");
        assert!(format!("{err:#}").contains("job.toml"), "{err:#}");
    }

    #[test]
    fn a_job_may_name_another_registered_block_and_not_an_unregistered_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("blocks/nightly")).unwrap();
        std::fs::write(root.join("blocks/nightly/init.lua"), "").unwrap();
        std::fs::write(root.join("blocks/nightly/job.toml"), "block = \"worker\"\n").unwrap();
        std::fs::write(root.join("blocks/worker.lua"), "").unwrap();
        let registered = blocks::scan(&[root.join("blocks")]);
        let jobs = scan_jobs(&registered).expect("scan");
        assert_eq!(jobs[0].name, "nightly");
        assert_eq!(jobs[0].block, "worker");
        assert_eq!(jobs[0].path, root.join("blocks/worker.lua"));

        std::fs::write(root.join("blocks/nightly/job.toml"), "block = \"ghost\"\n").unwrap();
        assert!(scan_jobs(&registered).is_err());
    }

    #[test]
    fn the_token_is_minted_once_and_read_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (first, path) = token_at(tmp.path()).expect("mint");
        assert_eq!(first.len(), 64);
        assert_eq!(path, tmp.path().join("serve.token"));
        let (second, _) = token_at(tmp.path()).expect("read");
        assert_eq!(first, second);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
