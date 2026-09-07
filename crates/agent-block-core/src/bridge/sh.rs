//! sh.* — Shell command execution.
//!
//! # Security
//!
//! By default there are no restrictions on command execution — Lua scripts can
//! run arbitrary shell commands via `sh -c`.  This is intentional; the trust
//! boundary is the Lua script author.
//!
//! Sandbox mode (`--sandbox` / `AGENT_BLOCK_SANDBOX`, Linux only) narrows what
//! those commands can *do* rather than what they may be: the Landlock ruleset
//! and seccomp filter installed at startup are inherited by every child spawned
//! here, so filesystem writes outside the allowlist and io_uring are denied for
//! `sh -c` payloads too, without this bridge knowing about it. See
//! [`crate::sandbox`] for the enforced semantics and its limitations.
//!
//! Sandbox mode is off by default, so the default trust boundary is unchanged.
//! It is a coarse execution boundary, not a command allowlist or a
//! capability-based policy — those remain unimplemented.
//!
//! Independently of sandbox mode, the block host's *own* credential environment
//! variables ([`agent_block_types::creds::OWN_CREDENTIAL_ENV_VARS`]) are removed
//! from every child spawned here, so commands the host runs cannot read the keys
//! the host itself uses. The same set is stripped from MCP server subprocesses
//! (see `agent_block_mcp::McpManager::connect`).
//! Custom per-LLM-conf key env names (`api_key_env`) are not covered by that
//! removal, and it is not an env allowlist — everything else is still inherited.

use mlua::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use agent_block_types::creds::OWN_CREDENTIAL_ENV_VARS;

use crate::host::HostContext;

/// Process groups this bridge started and has not yet seen finish.
///
/// A timeout kills its own group directly, so this exists for the other way a
/// run ends: a signal to the host. `kill_on_drop` cannot cover that — the
/// default disposition of SIGINT ends the process without running a
/// destructor, so nothing would fire. The set is walked by
/// [`install_signal_cleanup`] instead.
///
/// Process-global rather than per-host because a signal is delivered to the
/// process, not to a host.
static LIVE_GROUPS: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

fn remember_group(pgid: i32) {
    if let Ok(mut g) = LIVE_GROUPS.lock() {
        g.insert(pgid);
    }
}

fn forget_group(pgid: i32) {
    if let Ok(mut g) = LIVE_GROUPS.lock() {
        g.remove(&pgid);
    }
}

/// SIGKILL every process group still running under this bridge.
///
/// The pid of a group leader is its group id, and killing the group is what
/// reaches the descendants: `sh -c "cargo test"` is the command, but the test
/// binary it built is a grandchild, and killing the command alone leaves that
/// binary running. On a shared machine it stays there.
#[cfg(unix)]
pub fn kill_live_groups() {
    let groups: Vec<i32> = match LIVE_GROUPS.lock() {
        Ok(g) => g.iter().copied().collect(),
        Err(_) => return,
    };
    for pgid in groups {
        // SAFETY: `killpg` is a libc call with no invariants for the caller to
        // uphold. A group that has already gone answers ESRCH, which is the
        // answer this wants anyway.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
        forget_group(pgid);
    }
}

#[cfg(not(unix))]
pub fn kill_live_groups() {}

/// Forward a terminating signal to the process groups `sh.exec` started.
///
/// Called once by the host. With each command in a group of its own the
/// terminal's Ctrl-C no longer reaches it — the signal goes to the host's
/// group, and the command is no longer in it — so the host has to pass it on,
/// or a command would outlive the interrupt that was meant to stop it.
///
/// After forwarding, this waits `AGENT_BLOCK_TASK_GRACE_MS` and then ends the
/// process. The wait is what lets a shutdown path that is already listening
/// finish first: `bus.serve` has its own handler for the same signals and
/// cancels its token there, and when it ends the run this never reaches its
/// exit. When nothing else is listening — a plain script — the exit is what
/// keeps Ctrl-C meaning what it did before, since installing a listener at all
/// takes the default disposition away.
pub fn install_signal_cleanup() {
    #[cfg(unix)]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::signal::unix::{signal, SignalKind};

        // The host calls this per run, and a server host runs many. One
        // listener is what is wanted; more would race each other to the exit.
        static INSTALLED: AtomicBool = AtomicBool::new(false);
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }

        tokio::spawn(async move {
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "sh: SIGTERM handler not installed");
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            kill_live_groups();
            tokio::time::sleep(Duration::from_millis(super::config::task_grace_ms())).await;
            // 128 + SIGINT, the shell's convention for a signalled exit.
            std::process::exit(130);
        });
    }
}

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let sh_tbl = lua.create_table()?;
    let default_cwd = ctx.project_root.clone();

    sh_tbl.set(
        "exec",
        lua.create_async_function(move |lua, (cmd, opts): (String, Option<LuaTable>)| {
            let default_cwd = default_cwd.clone();
            async move {
                let timeout_secs: u64 = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("timeout").ok().flatten())
                    .unwrap_or(30);

                let cwd: PathBuf = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("cwd").ok().flatten())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| default_cwd.clone());

                let result = run_async(&cmd, &cwd, Duration::from_secs(timeout_secs)).await;

                match result {
                    Ok((code, stdout, stderr)) => {
                        let t = lua.create_table()?;
                        t.set("ok", true)?;
                        t.set("code", code)?;
                        t.set("stdout", stdout)?;
                        t.set("stderr", stderr)?;
                        Ok(t)
                    }
                    Err(e) => {
                        let t = lua.create_table()?;
                        t.set("ok", false)?;
                        t.set("error", e)?;
                        Ok(t)
                    }
                }
            }
        })?,
    )?;

    lua.globals().set("sh", sh_tbl)?;
    Ok(())
}

async fn run_async(
    cmd: &str,
    cwd: &PathBuf,
    timeout: Duration,
) -> Result<(i32, String, String), String> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Tokio does NOT kill children on drop by default; without this the
        // timeout below would return while the command kept running. It is the
        // backstop for every way this future can be cancelled; the timeout
        // path below does not rely on it, because it reaches further.
        .kill_on_drop(true);

    // A group of its own, so a timeout can reach the whole tree rather than
    // the command alone. `sh -c "cargo test"` is one process and the test
    // binary is two below it; SIGKILL to the command leaves that binary
    // running, and on a shared machine it stays. Off by configuration, the
    // command stays in the host's group and the old reach applies.
    #[cfg(unix)]
    let grouped = super::config::sh_process_group();
    #[cfg(unix)]
    if grouped {
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let grouped = false;

    for var in OWN_CREDENTIAL_ENV_VARS {
        command.env_remove(var);
    }

    let child = command.spawn().map_err(|e| format!("exec error: {e}"))?;

    // Read before the child moves into `wait_with_output`. A group leader's
    // pid is its group id, which is the handle the timeout needs.
    let pgid = if grouped {
        child.id().map(|id| id as i32)
    } else {
        None
    };
    if let Some(p) = pgid {
        remember_group(p);
    }

    let waited = tokio::time::timeout(timeout, child.wait_with_output()).await;

    if let Some(p) = pgid {
        #[cfg(unix)]
        if waited.is_err() {
            // SAFETY: see `kill_live_groups`.
            unsafe {
                libc::killpg(p, libc::SIGKILL);
            }
        }
        forget_group(p);
    }

    let output = waited
        .map_err(|_| {
            // Timeout expired. Cancelling the wait drops the child, and
            // `kill_on_drop(true)` turns that into a SIGKILL for the command
            // itself; the `killpg` above is what reaches what the command
            // started. Without a group of its own only the first happens.
            format!("timeout after {}s", timeout.as_secs())
        })?
        .map_err(|e| format!("wait error: {e}"))?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok((code, stdout, stderr))
}
