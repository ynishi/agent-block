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
use std::collections::BTreeMap;
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
///
/// Each group carries the `label` its `sh.exec` was given, if any, which is
/// what `sh.kill(label)` looks up: a caller that wants to end a command it
/// started has no pid (`sh.exec` answers only when the command has ended)
/// and no future to drop (an awaiting coroutine cannot be reached from
/// another), so the name it chose at start is the one handle it holds.
static LIVE_GROUPS: Mutex<BTreeMap<i32, Group>> = Mutex::new(BTreeMap::new());

/// One live group: the name it was started under, and whether `sh.kill`
/// has ended it — the latter is what the awaiting `sh.exec` reports as
/// `killed`, so a caller tells "stopped by us" from "ended on its own"
/// without reading exit codes, which differ by what the command was (a
/// signalled process has none; a nested `agent-block` host that forwarded
/// the signal exits 130).
struct Group {
    label: Option<String>,
    killed: bool,
}

fn remember_group(pgid: i32, label: Option<String>) {
    if let Ok(mut g) = LIVE_GROUPS.lock() {
        g.insert(
            pgid,
            Group {
                label,
                killed: false,
            },
        );
    }
}

/// Drop the group from the registry and answer whether it had been killed.
fn forget_group(pgid: i32) -> bool {
    match LIVE_GROUPS.lock() {
        Ok(mut g) => g.remove(&pgid).map(|group| group.killed).unwrap_or(false),
        Err(_) => false,
    }
}

/// The group started under `label`, marked as killed, if it is still live.
fn take_labelled(label: &str) -> Option<i32> {
    let mut g = LIVE_GROUPS.lock().ok()?;
    let (pgid, group) = g
        .iter_mut()
        .find(|(_, group)| group.label.as_deref() == Some(label))?;
    group.killed = true;
    Some(*pgid)
}

/// End the process group started with `label`, and answer whether there was
/// one. Nothing to end — the label was never given, or the command has
/// already ended — answers `false`.
///
/// SIGTERM first, SIGKILL after a grace: the command may be a host of its
/// own (`agent-block -s <block>` under `agent-block serve`) whose commands
/// are in groups of *their* own, and only a signal it can catch lets it
/// forward the end to them — SIGKILL would leave a grandchild running under
/// nobody, which is the leak the groups exist to close. The grace is the
/// task grace window twice over: once for the child host to forward, once
/// for it to leave.
#[cfg(unix)]
pub async fn kill_labelled(label: &str) -> bool {
    let Some(pgid) = take_labelled(label) else {
        return false;
    };
    // SAFETY: see `kill_live_groups`.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    let grace = Duration::from_millis(super::config::task_grace_ms());
    let deadline = tokio::time::Instant::now() + grace * 2;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Signal 0 asks whether the group still has a member.
        // SAFETY: as above; a probe sends nothing.
        let alive = unsafe { libc::killpg(pgid, 0) } == 0;
        if !alive {
            return true;
        }
    }
    // SAFETY: as above.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    true
}

#[cfg(not(unix))]
pub async fn kill_labelled(_label: &str) -> bool {
    false
}

/// SIGKILL every process group still running under this bridge.
///
/// The pid of a group leader is its group id, and killing the group is what
/// reaches the descendants: `sh -c "cargo test"` is the command, but the test
/// binary it built is a grandchild, and killing the command alone leaves that
/// binary running. On a shared machine it stays there.
#[cfg(unix)]
pub fn kill_live_groups() {
    signal_live_groups(libc::SIGKILL, true);
}

/// SIGTERM every process group still running under this bridge, and keep
/// them registered: the signal is forwarded, not the end. A command that is
/// a host of its own catches it and forwards it on to its own groups, which
/// a SIGKILL would have left running under nobody; [`kill_live_groups`]
/// after a grace is the backstop for what did not leave.
#[cfg(unix)]
pub fn term_live_groups() {
    signal_live_groups(libc::SIGTERM, false);
}

#[cfg(unix)]
fn signal_live_groups(signal: i32, forget: bool) {
    let groups: Vec<i32> = match LIVE_GROUPS.lock() {
        Ok(g) => g.keys().copied().collect(),
        Err(_) => return,
    };
    for pgid in groups {
        // SAFETY: `killpg` is a libc call with no invariants for the caller to
        // uphold. A group that has already gone answers ESRCH, which is the
        // answer this wants anyway.
        unsafe {
            libc::killpg(pgid, signal);
        }
        if forget {
            forget_group(pgid);
        }
    }
}

#[cfg(not(unix))]
pub fn kill_live_groups() {}

#[cfg(not(unix))]
pub fn term_live_groups() {}

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
            // Forwarded as received, then ended: a command that is a host
            // of its own needs the signal it can catch to reach what it
            // started; the kill after the grace is for what did not leave.
            term_live_groups();
            tokio::time::sleep(Duration::from_millis(super::config::task_grace_ms())).await;
            kill_live_groups();
            // `bus.serve` has a handler for the same signal and ends the
            // run through the script — it returns, what follows it runs,
            // the host shuts down, the process exits 0. That path is not
            // bounded by this grace (a manager records what it stopped
            // before leaving), so while it is serving the exit is its.
            if super::bus::is_serving() {
                tracing::info!("sh: signal forwarded; bus.serve owns the exit");
                return;
            }
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

                // The name `sh.kill` may later use for this command's group.
                let label: Option<String> = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("label").ok().flatten());

                let result = run_async(&cmd, &cwd, Duration::from_secs(timeout_secs), label).await;

                match result {
                    Ok((code, stdout, stderr, killed)) => {
                        let t = lua.create_table()?;
                        t.set("ok", true)?;
                        t.set("code", code)?;
                        t.set("stdout", stdout)?;
                        t.set("stderr", stderr)?;
                        // Ended by `sh.kill`, whatever exit code that left.
                        t.set("killed", killed)?;
                        Ok(t)
                    }
                    Err(failure) => {
                        let t = lua.create_table()?;
                        t.set("ok", false)?;
                        t.set("error", failure.message)?;
                        // Said as a field and not only in the message: a
                        // caller that tells "did not answer in time" from
                        // "could not start" should not have to parse prose.
                        t.set("timed_out", failure.timed_out)?;
                        Ok(t)
                    }
                }
            }
        })?,
    )?;

    // ── sh.kill ───────────────────────────────────────────────────────
    // End a command another coroutine is awaiting, by the `label` its
    // `sh.exec` was given. Reaches the whole group, as the timeout does,
    // and gives a child host the chance to reach its own first.
    sh_tbl.set(
        "kill",
        lua.create_async_function(
            |_, label: String| async move { Ok(kill_labelled(&label).await) },
        )?,
    )?;

    lua.globals().set("sh", sh_tbl)?;
    Ok(())
}

/// Why a command did not answer, and whether that was the timeout. The
/// message is what `sh.exec` puts in `error`; `timed_out` is the one
/// distinction a caller acts on differently (a run that did not answer is
/// not a run that answered no).
struct ExecFailure {
    message: String,
    timed_out: bool,
}

impl ExecFailure {
    fn timed_out(message: String) -> Self {
        Self {
            message,
            timed_out: true,
        }
    }

    fn other(message: String) -> Self {
        Self {
            message,
            timed_out: false,
        }
    }
}

async fn run_async(
    cmd: &str,
    cwd: &PathBuf,
    timeout: Duration,
    label: Option<String>,
) -> Result<(i32, String, String, bool), ExecFailure> {
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

    let child = command
        .spawn()
        .map_err(|e| ExecFailure::other(format!("exec error: {e}")))?;

    // Read before the child moves into `wait_with_output`. A group leader's
    // pid is its group id, which is the handle the timeout needs.
    let pgid = if grouped {
        child.id().map(|id| id as i32)
    } else {
        None
    };
    if let Some(p) = pgid {
        remember_group(p, label.clone());
    }

    let waited = tokio::time::timeout(timeout, child.wait_with_output()).await;

    let mut killed = false;
    if let Some(p) = pgid {
        #[cfg(unix)]
        if waited.is_err() {
            // SAFETY: see `kill_live_groups`.
            unsafe {
                libc::killpg(p, libc::SIGKILL);
            }
        }
        killed = forget_group(p);
    }

    let output = waited
        .map_err(|_| {
            // Timeout expired. Cancelling the wait drops the child, and
            // `kill_on_drop(true)` turns that into a SIGKILL for the command
            // itself; the `killpg` above is what reaches what the command
            // started. Without a group of its own only the first happens.
            ExecFailure::timed_out(format!("timeout after {}s", timeout.as_secs()))
        })?
        .map_err(|e| ExecFailure::other(format!("wait error: {e}")))?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok((code, stdout, stderr, killed))
}
