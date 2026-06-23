//! `std.kv` thin adapter.
//!
//! Bridge implementation moved to the `mlua-batteries` crate
//! (`mlua_batteries::kv`).  This module only resolves the host's
//! environment-driven SQL configuration into a
//! [`mlua_batteries::sql::SqlConfig`] (shared with `std.sql`) before
//! delegating to [`mlua_batteries::kv::register_with`], then layers the
//! agent-block Lua tool helpers (`kv_tools.lua`) on top.
//!
//! See `bridge/config.rs` for the ENV → config mapping.

use std::sync::Arc;

use mlua::prelude::*;
use mlua_batteries::sql::SqlConfig;

use crate::host::HostContext;

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let cfg = SqlConfig {
        query_timeout: super::config::sql_query_timeout(),
    };
    mlua_batteries::kv::register_with(
        lua,
        Arc::clone(&ctx.kv_conn),
        Arc::clone(&ctx.kv_interrupt),
        cfg,
    )?;

    // Load std.kv.register_tools (LLM-facing helper; requires `tool` global).
    lua.load(include_str!("kv_tools.lua"))
        .set_name("std.kv.register_tools")
        .exec()?;

    Ok(())
}
