//! `std.sql` thin adapter.
//!
//! Bridge implementation lives in the `mlua-batteries-sqlite` crate
//! (`mlua_batteries_sqlite::sql`), which since 0.5 is where `std.sql` and
//! `std.kv` moved out of `mlua-batteries` itself.  This module only resolves
//! the host's environment-driven SQL configuration into a
//! [`mlua_batteries_sqlite::sql::SqlConfig`] before delegating to
//! [`mlua_batteries_sqlite::sql::register_with`], then layers the agent-block
//! Lua tool helpers (`sql_tools.lua`) on top.
//!
//! The connection is not passed in any more: what the batteries take is a
//! handle to the thread that owns it ([`crate::host::HostContext::sql_isle`]).
//! Opening the database, the busy timeout and `journal_mode` are the host's,
//! applied where the isle is spawned — see `host.rs` — and the ENV → config
//! mapping is in `bridge/config.rs`.

use mlua::prelude::*;
use mlua_batteries_sqlite::sql::SqlConfig;

use crate::host::HostContext;

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let cfg = SqlConfig {
        query_timeout: super::config::sql_query_timeout(),
    };
    mlua_batteries_sqlite::sql::register_with(lua, ctx.sql_isle.clone(), cfg)?;

    // Load std.sql.register_tools (LLM-facing helper; requires `tool` global).
    lua.load(include_str!("sql_tools.lua"))
        .set_name("std.sql.register_tools")
        .exec()?;

    Ok(())
}
