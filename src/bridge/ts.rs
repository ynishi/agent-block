//! `std.ts.*` — SQLite-backed time-series primitive.
//!
//! This is the only storage bridge whose implementation is in-tree
//! (mlua_batteries provides no TSDB module). DDL / append / query /
//! last all live in this file.
//!
//! Backend: single `ts` table in `ts.sqlite` (or `:memory:`).
//!
//! Schema:
//! ```sql
//! CREATE TABLE IF NOT EXISTS ts (
//!     series TEXT NOT NULL,
//!     ts     INTEGER NOT NULL,
//!     tags   TEXT,
//!     value  TEXT NOT NULL
//! );
//! CREATE INDEX IF NOT EXISTS idx_ts_series_ts ON ts(series, ts);
//! ```
//!
//! Column notes:
//! - `series`: logical stream name (e.g. `"cpu_load"`, `"agent_events"`)
//! - `ts`: Unix timestamp in milliseconds (i64)
//! - `tags`: JSON object (`{"task": "X", "phase": "Y"}`) or NULL; filtered
//!   via `json_extract` in queries — never compared as a serialised string
//! - `value`: JSON-encoded payload; accepts both JSON numbers and JSON objects
//!   so that callers can append plain numeric metrics or structured MCP
//!   envelope payloads without loss (dual-type contract, Crux §3.8 C1)
//!
//! Append / query / last are implemented in Subtask 2.  This module skeleton
//! initialises the DDL and installs an empty `std.ts` table into the Lua VM.
//!
//! See `bridge/config.rs` for the ENV → path mapping (`AGENT_BLOCK_TS_PATH`).

use mlua::prelude::*;

use crate::host::HostContext;

/// Register the `std.ts` bridge into `lua`.
///
/// On first call this function:
/// 1. Acquires the ts SQLite connection and runs the DDL (idempotent —
///    `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`).
/// 2. Installs an empty `std.ts` Lua table as a placeholder; the actual
///    `append` / `query` / `last` closures are added in Subtask 2.
///
/// # Arguments
///
/// - `lua`: the Lua state to register into (main Isle or handler Isle)
/// - `ctx`: host context providing `ts_conn` (Arc<Mutex<Connection>>)
///
/// # Errors
///
/// Returns a `LuaError` if:
/// - the Mutex is poisoned (`ts conn lock poisoned`)
/// - the DDL `execute_batch` fails (`ts DDL: <rusqlite error>`)
/// - the `std` global is not a table or `std.ts` assignment fails
pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    // ── DDL init ─────────────────────────────────────────────────────────
    // Acquire the connection and run both DDL statements in one batch call.
    // execute_batch supports semicolon-separated statements, so no second
    // prepare/execute is needed. The IF NOT EXISTS guards make this idempotent
    // across restarts and `:memory:` re-initializations.
    let conn = ctx.ts_conn.lock().map_err(|e| {
        tracing::warn!(error = %e, "ts conn lock poisoned during DDL");
        LuaError::external(format!("ts conn lock poisoned: {e}"))
    })?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ts \
         (series TEXT NOT NULL, ts INTEGER NOT NULL, \
          tags TEXT, value TEXT NOT NULL); \
         CREATE INDEX IF NOT EXISTS idx_ts_series_ts ON ts(series, ts);",
    )
    .map_err(|e| {
        tracing::warn!(error = %e, "ts ddl failed");
        LuaError::external(format!("ts DDL: {e}"))
    })?;

    // Release the lock before touching the Lua state to avoid holding a
    // MutexGuard across any potential Lua GC or re-entrant lock attempt.
    drop(conn);

    // ── Install empty std.ts table ────────────────────────────────────────
    // mlua_batteries::register_all(lua, "std") has already installed the
    // `std` global before bridges are registered (see host.rs build_isle_init).
    // We fetch it and set the `ts` field to an empty table; Subtask 2 will
    // populate it with `append` / `query` / `last` closures.
    let std_table: LuaTable = lua.globals().get("std")?;
    std_table.set("ts", lua.create_table()?)?;

    Ok(())
}
