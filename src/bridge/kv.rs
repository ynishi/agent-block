//! `std.kv` — SQLite-backed key-value store for Lua scripts.
//!
//! Storage lives in the same SQLite database as `std.sql` (one shared
//! connection), in a dedicated `__kv` table:
//!
//! ```sql
//! CREATE TABLE __kv (
//!     ns    TEXT NOT NULL,
//!     key   TEXT NOT NULL,
//!     value TEXT NOT NULL,   -- JSON-serialized Lua value
//!     PRIMARY KEY (ns, key)
//! ) WITHOUT ROWID;
//! ```
//!
//! Trade-offs vs. the previous JSON-file-per-namespace implementation:
//! - Per-key updates (no whole-namespace rewrite on every set).
//! - Durability + atomicity delegated to SQLite's WAL journal.
//! - Cross-process writes arbitrated by `busy_timeout`.

use std::sync::Arc;

use mlua::prelude::*;
use rusqlite::OptionalExtension;

use crate::host::HostContext;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate a namespace string.
///
/// Namespaces were originally used as filenames, so `/`, `\`, `..`, `\0` were
/// rejected for path-traversal safety. Even though storage is now a SQL table
/// (and namespaces are just column values), we keep the same validation so
/// that existing Lua scripts and tests see identical semantics.
fn validate_ns(ns: &str) -> Result<(), String> {
    if ns.is_empty() {
        return Err(format!("Invalid namespace: '{ns}'"));
    }
    if ns.contains('/') || ns.contains('\\') || ns.contains('\0') || ns.contains("..") {
        return Err(format!("Invalid namespace: '{ns}'"));
    }
    Ok(())
}

fn init_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS __kv (\n                ns    TEXT NOT NULL,\n                key   TEXT NOT NULL,\n                value TEXT NOT NULL,\n                PRIMARY KEY (ns, key)\n            ) WITHOUT ROWID;",
    )
    .map_err(|e| format!("kv schema init: {e}"))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    // One-time schema init on the shared connection.
    {
        let guard = super::sql::lock_conn(&ctx.kv_conn);
        init_schema(&guard).map_err(LuaError::external)?;
    }

    let kv_tbl = lua.create_table()?;

    // ── std.kv.get ────────────────────────────────────────────────────────
    {
        let ctx_conn = Arc::clone(&ctx.kv_conn);
        let ctx_interrupt = Arc::clone(&ctx.kv_interrupt);
        kv_tbl.set(
            "get",
            lua.create_async_function(move |lua, (ns, key): (String, String)| {
                let conn = Arc::clone(&ctx_conn);
                let interrupt = Arc::clone(&ctx_interrupt);
                let ns_check = validate_ns(&ns).map_err(LuaError::external);
                async move {
                    ns_check?;
                    let fut = tokio::task::spawn_blocking(move || {
                        let guard = super::sql::lock_conn(&conn);
                        guard
                            .query_row(
                                "SELECT value FROM __kv WHERE ns = ?1 AND key = ?2",
                                rusqlite::params![ns, key],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .map_err(|e| format!("kv.get sql error: {e}"))
                    });
                    let timeout = super::config::sql_query_timeout();
                    let row =
                        super::sql::race_timeout(fut, timeout, &interrupt, "kv.get").await?;
                    match row {
                        None => Ok(LuaValue::Nil),
                        Some(s) => {
                            let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
                                LuaError::external(format!("kv.get json parse: {e}"))
                            })?;
                            super::json_to_lua(&lua, v)
                        }
                    }
                }
            })?,
        )?;
    }

    // ── std.kv.set ────────────────────────────────────────────────────────
    {
        let ctx_conn = Arc::clone(&ctx.kv_conn);
        let ctx_interrupt = Arc::clone(&ctx.kv_interrupt);
        kv_tbl.set(
            "set",
            lua.create_async_function(
                move |lua, (ns, key, value): (String, String, LuaValue)| {
                    let conn = Arc::clone(&ctx_conn);
                    let interrupt = Arc::clone(&ctx_interrupt);
                    // Serialize synchronously on the Lua thread (LuaValue is
                    // !Send, so it can't cross the spawn_blocking boundary).
                    let ns_check = validate_ns(&ns).map_err(LuaError::external);
                    let json_result = super::lua_to_json(&lua, value).and_then(|v| {
                        serde_json::to_string(&v)
                            .map_err(|e| LuaError::external(format!("kv.set serialize: {e}")))
                    });
                    async move {
                        ns_check?;
                        let json_str = json_result?;
                        let fut = tokio::task::spawn_blocking(move || {
                            let guard = super::sql::lock_conn(&conn);
                            guard
                                .execute(
                                    "INSERT INTO __kv (ns, key, value) VALUES (?1, ?2, ?3) \
                                     ON CONFLICT(ns, key) DO UPDATE SET value = excluded.value",
                                    rusqlite::params![ns, key, json_str],
                                )
                                .map(|_| ())
                                .map_err(|e| format!("kv.set sql error: {e}"))
                        });
                        let timeout = super::config::sql_query_timeout();
                        super::sql::race_timeout(fut, timeout, &interrupt, "kv.set").await
                    }
                },
            )?,
        )?;
    }

    // ── std.kv.delete ─────────────────────────────────────────────────────
    {
        let ctx_conn = Arc::clone(&ctx.kv_conn);
        let ctx_interrupt = Arc::clone(&ctx.kv_interrupt);
        kv_tbl.set(
            "delete",
            lua.create_async_function(move |_, (ns, key): (String, String)| {
                let conn = Arc::clone(&ctx_conn);
                let interrupt = Arc::clone(&ctx_interrupt);
                let ns_check = validate_ns(&ns).map_err(LuaError::external);
                async move {
                    ns_check?;
                    let fut = tokio::task::spawn_blocking(move || {
                        let guard = super::sql::lock_conn(&conn);
                        guard
                            .execute(
                                "DELETE FROM __kv WHERE ns = ?1 AND key = ?2",
                                rusqlite::params![ns, key],
                            )
                            .map(|n| n > 0)
                            .map_err(|e| format!("kv.delete sql error: {e}"))
                    });
                    let timeout = super::config::sql_query_timeout();
                    super::sql::race_timeout(fut, timeout, &interrupt, "kv.delete").await
                }
            })?,
        )?;
    }

    // ── std.kv.list ───────────────────────────────────────────────────────
    {
        let ctx_conn = Arc::clone(&ctx.kv_conn);
        let ctx_interrupt = Arc::clone(&ctx.kv_interrupt);
        kv_tbl.set(
            "list",
            lua.create_async_function(move |lua, (ns, prefix): (String, Option<String>)| {
                let conn = Arc::clone(&ctx_conn);
                let interrupt = Arc::clone(&ctx_interrupt);
                let ns_check = validate_ns(&ns).map_err(LuaError::external);
                async move {
                    ns_check?;
                    let fut = tokio::task::spawn_blocking(move || {
                        let guard = super::sql::lock_conn(&conn);
                        let mut stmt = guard
                            .prepare("SELECT key FROM __kv WHERE ns = ?1 ORDER BY key")
                            .map_err(|e| format!("kv.list prepare: {e}"))?;
                        let keys: Vec<String> = stmt
                            .query_map(rusqlite::params![ns], |row| row.get::<_, String>(0))
                            .map_err(|e| format!("kv.list query: {e}"))?
                            .collect::<Result<_, _>>()
                            .map_err(|e| format!("kv.list row: {e}"))?;
                        Ok::<_, String>(keys)
                    });
                    let timeout = super::config::sql_query_timeout();
                    let keys =
                        super::sql::race_timeout(fut, timeout, &interrupt, "kv.list").await?;

                    let tbl = lua.create_table()?;
                    let mut idx = 1usize;
                    for k in keys {
                        let include = prefix.as_deref().map_or(true, |p| k.starts_with(p));
                        if include {
                            tbl.set(idx, k.as_str())?;
                            idx += 1;
                        }
                    }
                    Ok(LuaValue::Table(tbl))
                }
            })?,
        )?;
    }

    let std_ns: LuaTable = lua.globals().get("std")?;
    std_ns.set("kv", kv_tbl)?;

    // Load std.kv.register_tools (LLM-facing helper; requires `tool` global).
    lua.load(include_str!("kv_tools.lua"))
        .set_name("std.kv.register_tools")
        .exec()?;

    Ok(())
}
