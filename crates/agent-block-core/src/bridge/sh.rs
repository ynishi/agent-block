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

use mlua::prelude::*;
use std::path::PathBuf;
use std::time::Duration;

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
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("exec error: {e}"))?;

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            // Timeout expired — kill the child process.
            // child is moved into wait_with_output, so we can't kill it here.
            // tokio drops the child on timeout which sends SIGKILL.
            format!("timeout after {}s", timeout.as_secs())
        })?
        .map_err(|e| format!("wait error: {e}"))?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok((code, stdout, stderr))
}
