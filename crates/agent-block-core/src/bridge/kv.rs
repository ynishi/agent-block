//! `std.kv` thin adapter.
//!
//! Bridge implementation lives in the `mlua-batteries-sqlite` crate
//! (`mlua_batteries_sqlite::kv`), which since 0.5 is where `std.sql` and
//! `std.kv` moved out of `mlua-batteries` itself.  This module only resolves
//! the host's environment-driven SQL configuration into a
//! [`mlua_batteries_sqlite::sql::SqlConfig`] (shared with `std.sql`) before
//! delegating to [`mlua_batteries_sqlite::kv::register_with`], then layers the
//! agent-block Lua tool helpers (`kv_tools.lua`) on top.
//!
//! The connection is not passed in any more: what the batteries take is a
//! handle to the thread that owns it ([`crate::host::HostContext::kv_isle`]).
//! Opening the database, the busy timeout and `journal_mode` are the host's,
//! applied where the isle is spawned — see `host.rs` — and the ENV → config
//! mapping is in `bridge/config.rs`.
//!
//! # Why this one call is driven to completion here
//!
//! `kv::register_with` is `async`, because creating the `__kv` table is one
//! round trip to the connection thread.  Bridge registration is not: it runs
//! inside a closure the Lua Isle executes on its own thread, and there is no
//! async form of that closure to hand the future to.  So the future is driven
//! here, and this is safe rather than merely convenient: the only thing it
//! waits on is the *SQLite* isle's thread, which is already running and is not
//! the thread being blocked.  It happens once, during host init, before any
//! script has run.

use mlua::prelude::*;
use mlua_batteries_sqlite::sql::SqlConfig;

use crate::host::HostContext;

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let cfg = SqlConfig {
        query_timeout: super::config::sql_query_timeout(),
    };
    futures_executor::block_on(mlua_batteries_sqlite::kv::register_with(
        lua,
        ctx.kv_isle.clone(),
        cfg,
    ))?;

    // Load std.kv.register_tools (LLM-facing helper; requires `tool` global).
    lua.load(include_str!("kv_tools.lua"))
        .set_name("std.kv.register_tools")
        .exec()?;

    Ok(())
}
