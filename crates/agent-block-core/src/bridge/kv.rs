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
//! What the batteries take is the connection the host opened
//! ([`crate::host::HostContext::kv_conn`]) — a database of its own, shared as
//! `Arc<Mutex<_>>` with its `InterruptHandle` beside it.  Opening it, the busy
//! timeout and `journal_mode` are the host's (see `host.rs`), and the ENV →
//! config mapping is in `bridge/config.rs`.
//!
//! # Registration is synchronous, and so is the schema
//!
//! `register_with` creates the `__kv` table on the supplied connection if it
//! is not there, which is a statement run right here, on the Isle's own thread
//! during init — no future to drive, which is what the `futures-executor`
//! `block_on` this module used to hold was for.  The host has already run
//! [`mlua_batteries_sqlite::kv::init_schema`] against the same connection when
//! it opened it, so in practice the DDL here finds the table present and does
//! nothing; `CREATE TABLE IF NOT EXISTS` makes that harmless either way.
//!
//! Once registered, a `std.kv` call behaves like a `std.sql` one: the
//! statement goes to `tokio::task::spawn_blocking`, the mutex is taken inside
//! that closure, and the Lua VM yields rather than waiting.

use mlua::prelude::*;
use mlua_batteries_sqlite::sql::SqlConfig;

use crate::host::HostContext;

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let cfg = SqlConfig {
        query_timeout: super::config::sql_query_timeout(),
    };
    mlua_batteries_sqlite::kv::register_with(
        lua,
        ctx.kv_conn.conn.clone(),
        ctx.kv_conn.interrupt.clone(),
        cfg,
    )?;

    // Load std.kv.register_tools (LLM-facing helper; requires `tool` global).
    lua.load(include_str!("kv_tools.lua"))
        .set_name("std.kv.register_tools")
        .exec()?;

    Ok(())
}
