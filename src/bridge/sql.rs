//! `std.sql` thin adapter.
//!
//! Bridge implementation moved to the `mlua-batteries` crate
//! (`mlua_batteries::sql`).  This module only resolves the host's
//! environment-driven SQL configuration into a
//! [`mlua_batteries::sql::SqlConfig`] before delegating to
//! [`mlua_batteries::sql::register_with`], then layers the agent-block
//! Lua tool helpers (`sql_tools.lua`) on top.
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
    mlua_batteries::sql::register_with(
        lua,
        Arc::clone(&ctx.sql_conn),
        Arc::clone(&ctx.sql_interrupt),
        cfg,
    )?;

    // Load std.sql.register_tools (LLM-facing helper; requires `tool` global).
    lua.load(include_str!("sql_tools.lua"))
        .set_name("std.sql.register_tools")
        .exec()?;

    Ok(())
}
