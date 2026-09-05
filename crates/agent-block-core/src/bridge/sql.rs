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
//! What the batteries take is the connection the host opened
//! ([`crate::host::HostContext::sql_conn`]) — shared as `Arc<Mutex<_>>` with
//! its `InterruptHandle` beside it.  Opening the database, the busy timeout
//! and `journal_mode` are still the host's, applied before the VM exists (see
//! `host.rs`), and the ENV → config mapping is in `bridge/config.rs`.
//!
//! The VM thread does not wait on SQLite here either: a statement goes to
//! `tokio::task::spawn_blocking` and the mutex is taken *inside* that closure,
//! so the Lua VM yields for the whole round trip and no guard is held across
//! an `.await`.  A cancelled `std.task` scope or an expired query timeout
//! interrupts the statement through the handle.  (`std.ts` and the kernel
//! store reach the same end by the other route — a connection thread of their
//! own; see `bridge/ts.rs`.)

use mlua::prelude::*;
use mlua_batteries_sqlite::sql::SqlConfig;

use crate::host::HostContext;

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let cfg = SqlConfig {
        query_timeout: super::config::sql_query_timeout(),
    };
    mlua_batteries_sqlite::sql::register_with(
        lua,
        ctx.sql_conn.conn.clone(),
        ctx.sql_conn.interrupt.clone(),
        cfg,
    )?;

    // Load std.sql.register_tools (LLM-facing helper; requires `tool` global).
    lua.load(include_str!("sql_tools.lua"))
        .set_name("std.sql.register_tools")
        .exec()?;

    Ok(())
}
