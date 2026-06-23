//! log.* — Logging via tracing.
//!
//! All Lua log calls are emitted under a `lua` tracing target with a
//! `script` field so output can be filtered and attributed per script.
//!
//! Environment access (`env.*`) is provided by mlua-batteries (`std.env`).
//! Agent-specific functions (`env.agent_id`, `env.project_root`) are injected
//! into the batteries-provided `std.env` table here.

use mlua::prelude::*;

use crate::host::HostContext;

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let log_tbl = lua.create_table()?;

    // Extract script name once, clone into each closure.
    let script_name: String = lua
        .globals()
        .get::<Option<String>>("_SCRIPT_NAME")?
        .unwrap_or_else(|| "unknown".to_string());

    {
        let s = script_name.clone();
        log_tbl.set(
            "info",
            lua.create_function(move |_, msg: String| {
                tracing::info!(target: "lua", script = %s, "{msg}");
                Ok(())
            })?,
        )?;
    }
    {
        let s = script_name.clone();
        log_tbl.set(
            "warn",
            lua.create_function(move |_, msg: String| {
                tracing::warn!(target: "lua", script = %s, "{msg}");
                Ok(())
            })?,
        )?;
    }
    {
        let s = script_name.clone();
        log_tbl.set(
            "error",
            lua.create_function(move |_, msg: String| {
                tracing::error!(target: "lua", script = %s, "{msg}");
                Ok(())
            })?,
        )?;
    }
    {
        let s = script_name;
        log_tbl.set(
            "debug",
            lua.create_function(move |_, msg: String| {
                tracing::debug!(target: "lua", script = %s, "{msg}");
                Ok(())
            })?,
        )?;
    }

    lua.globals().set("log", log_tbl)?;

    // Inject agent-specific functions into std.env (provided by mlua-batteries)
    let std_ns: LuaTable = lua.globals().get("std")?;
    let env_tbl: LuaTable = std_ns.get("env")?;

    let agent_id_str = ctx
        .mesh_agent
        .as_ref()
        .map(|a| a.agent_id().to_string())
        .unwrap_or_default();
    env_tbl.set(
        "agent_id",
        lua.create_function(move |_, ()| Ok(agent_id_str.clone()))?,
    )?;

    let project_root = ctx.project_root.to_string_lossy().to_string();
    env_tbl.set(
        "project_root",
        lua.create_function(move |_, ()| Ok(project_root.clone()))?,
    )?;

    Ok(())
}
