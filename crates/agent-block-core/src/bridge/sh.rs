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
use std::path::PathBuf;
use std::time::Duration;

use agent_block_types::creds::OWN_CREDENTIAL_ENV_VARS;

use crate::host::HostContext;

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
        // timeout below would return while the command kept running.
        .kill_on_drop(true);

    for var in OWN_CREDENTIAL_ENV_VARS {
        command.env_remove(var);
    }

    let child = command.spawn().map_err(|e| format!("exec error: {e}"))?;

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            // Timeout expired. `child` was moved into `wait_with_output`, so it
            // cannot be killed by name here; cancelling that future drops the
            // child, and `kill_on_drop(true)` above turns that drop into a
            // SIGKILL. Without that flag the child would survive this return.
            format!("timeout after {}s", timeout.as_secs())
        })?
        .map_err(|e| format!("wait error: {e}"))?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok((code, stdout, stderr))
}
