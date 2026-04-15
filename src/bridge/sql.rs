//! `std.sql` — SQLite (rusqlite WAL) bridge for Lua scripts.
//!
//! Provides:
//! - `std.sql.query(sql, params?) -> rows`   rows = array of { col_name = value, ... }
//! - `std.sql.exec(sql, params?)  -> { affected = N, last_id = M }`
//!
//! rusqlite calls are executed inside `tokio::task::spawn_blocking` to avoid
//! blocking the async runtime.  Lock acquisition is also inside spawn_blocking
//! to prevent holding a Mutex guard across `.await` (await-holding-lock).

use std::sync::Arc;

use mlua::prelude::*;
use rusqlite::{
    types::{Value, ValueRef},
    Connection,
};
use serde_json::Map;
use tracing::warn;

use crate::host::HostContext;

// ---------------------------------------------------------------------------
// Param conversion: Lua → rusqlite
// ---------------------------------------------------------------------------

/// Convert a Lua array table to `Vec<rusqlite::types::Value>`.
///
/// Supported Lua types:
/// - nil        → Null
/// - boolean    → Integer(0 or 1)  (SQLite has no native bool)
/// - integer    → Integer(i64)
/// - number     → Real(f64), rejects NaN / ±Inf (SQLite stores but
///   JSON/Lua round-trip and serde_json cannot represent them)
/// - string     → Text(String)
///
/// Unsupported types (table, function, userdata) return an error.
fn lua_params_to_values(tbl: &LuaTable) -> Result<Vec<Value>, String> {
    let len = tbl.raw_len();
    let mut result = Vec::with_capacity(len);
    for i in 1..=len {
        let v: LuaValue = tbl
            .raw_get(i)
            .map_err(|e| format!("params table access error: {e}"))?;
        let sql_val = match v {
            LuaValue::Nil => Value::Null,
            LuaValue::Boolean(b) => Value::Integer(if b { 1 } else { 0 }),
            LuaValue::Integer(n) => Value::Integer(n),
            LuaValue::Number(f) => {
                if !f.is_finite() {
                    return Err(format!(
                        "SQL param #{i} is non-finite ({f}); NaN and ±Inf are not supported"
                    ));
                }
                Value::Real(f)
            }
            LuaValue::String(s) => Value::Text(
                s.to_str()
                    .map_err(|e| format!("param string encoding error: {e}"))?
                    .to_string(),
            ),
            other => return Err(format!("unsupported SQL param type: {}", other.type_name())),
        };
        result.push(sql_val);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Query execution (SELECT-like)
// ---------------------------------------------------------------------------

/// Execute a query and return all rows as a list of column-name→JSON-value maps.
///
/// This function must be called inside `spawn_blocking`.
fn run_query(
    conn: &Connection,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Map<String, serde_json::Value>>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| format!("sql error: {e}"))?;

    let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();

    let mut rows = stmt
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|e| format!("sql error: {e}"))?;

    let mut result = Vec::new();
    while let Some(row) = rows.next().map_err(|e| format!("sql error: {e}"))? {
        let mut map = serde_json::Map::new();
        for (i, name) in col_names.iter().enumerate() {
            let val = match row.get_ref(i).map_err(|e| format!("sql error: {e}"))? {
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Integer(n) => serde_json::Value::Number(n.into()),
                ValueRef::Real(f) => {
                    // SQLite can store non-finite doubles via the C API, but
                    // serde_json refuses NaN / ±Inf (returns None from
                    // `from_f64`). Silently lowering them to NULL would
                    // corrupt the round-trip. Surface the corruption so the
                    // caller can decide rather than pretending it was NULL.
                    serde_json::Number::from_f64(f)
                        .map(serde_json::Value::Number)
                        .ok_or_else(|| {
                            format!(
                                "non-finite REAL in column '{}' ({f}); \
                                 NaN / ±Inf cannot be represented in JSON/Lua",
                                col_names[i]
                            )
                        })?
                }
                ValueRef::Text(b) => {
                    // DB encoding is UTF-8 (SQLite default, PRAGMA encoding
                    // unset). Invalid UTF-8 in a TEXT column means the DB was
                    // corrupted by an external writer that bypassed the
                    // declared encoding. Reject instead of silently replacing
                    // with U+FFFD — matches the write path's strictness and
                    // surfaces corruption early.
                    let s = std::str::from_utf8(b).map_err(|e| {
                        format!("non-UTF-8 TEXT in column '{}': {e}", col_names[i])
                    })?;
                    serde_json::Value::String(s.to_string())
                }
                ValueRef::Blob(_) => return Err("blob columns not supported in POC".to_string()),
            };
            map.insert(name.clone(), val);
        }
        result.push(map);
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Exec execution (INSERT / UPDATE / DELETE / DDL)
// ---------------------------------------------------------------------------

/// Execute a DML/DDL statement and return (affected_rows, last_insert_rowid).
///
/// This function must be called inside `spawn_blocking`.
fn run_exec(conn: &Connection, sql: &str, params: &[Value]) -> Result<(usize, i64), String> {
    let affected = conn
        .execute(sql, rusqlite::params_from_iter(params.iter()))
        .map_err(|e| format!("sql error: {e}"))?;
    let last_id = conn.last_insert_rowid();
    Ok((affected, last_id))
}

// ---------------------------------------------------------------------------
// Row → Lua conversion
// ---------------------------------------------------------------------------

/// Convert a list of column-name→JSON-value maps into a Lua array table.
///
/// Each element is a Lua table `{ col_name = value, ... }`. NULL columns
/// arrive here as `serde_json::Value::Null` and are translated by
/// `json_to_lua` into the `LightUserData(null_ptr)` sentinel (exposed to
/// Lua as `std.sql.null`), which keeps the column present in the row table.
/// This preserves the distinction between "column is NULL" and "column was
/// not in the query".
fn rows_to_lua(lua: &Lua, rows: Vec<Map<String, serde_json::Value>>) -> LuaResult<LuaValue> {
    let arr = lua.create_table()?;
    for (i, row_map) in rows.into_iter().enumerate() {
        let row_tbl = lua.create_table()?;
        for (col, val) in row_map {
            let lua_val = super::json_to_lua(lua, val)?;
            row_tbl.set(col.as_str(), lua_val)?;
        }
        arr.set(i + 1, row_tbl)?;
    }
    Ok(LuaValue::Table(arr))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Lock the shared Connection mutex without panicking.
///
/// On `PoisonError` we log and recover via `into_inner()`. Poison here means a
/// previous blocking thread panicked while holding the guard; for a local
/// agent-runtime SQLite (single-process, embedded) the safest path is to log
/// and keep serving rather than tear the host down.
pub(super) fn lock_conn(
    conn: &std::sync::Mutex<rusqlite::Connection>,
) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
    conn.lock().unwrap_or_else(|poisoned| {
        warn!("sql conn mutex was poisoned; recovering via into_inner");
        poisoned.into_inner()
    })
}

/// Race an `spawn_blocking` SQL operation against the configured query timeout.
///
/// When the timeout fires first we call `sqlite3_interrupt` via the stored
/// handle so the blocking thread returns quickly, releases the Mutex guard,
/// and frees the connection for subsequent calls. Without the interrupt the
/// Mutex would stay locked for however long the runaway query takes to
/// finish naturally, freezing the whole `std.sql` namespace.
pub(super) async fn race_timeout<T, F>(
    fut: F,
    timeout: Option<std::time::Duration>,
    interrupt: &rusqlite::InterruptHandle,
    op: &'static str,
) -> LuaResult<T>
where
    F: std::future::Future<Output = Result<Result<T, String>, tokio::task::JoinError>>,
{
    let joined = match timeout {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(joined) => joined,
            Err(_) => {
                interrupt.interrupt();
                warn!(op, timeout_ms = d.as_millis() as u64, "sql timeout");
                return Err(LuaError::external(format!(
                    "sql timeout ({}ms) in {op}",
                    d.as_millis()
                )));
            }
        },
        None => fut.await,
    };
    joined
        .map_err(|e| {
            warn!(op, error = %e, "spawn_blocking join error");
            LuaError::external(format!("spawn_blocking: {e}"))
        })?
        .map_err(|e| {
            warn!(op, error = %e, "sql execution error");
            LuaError::external(e)
        })
}

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let sql_tbl = lua.create_table()?;

    // ── std.sql.null ──────────────────────────────────────────────────────
    // Sentinel that represents SQL NULL on the Lua side (also used for JSON
    // null in values returned from `sql` / `kv` / `mcp` / `llm` bridges).
    // `mlua::Value::NULL` is `LightUserData(null_ptr)`, and any equivalent
    // LightUserData produced from `std::ptr::null_mut()` compares equal via
    // Lua `==` (lightuserdata equality is pointer equality), so scripts can
    // write `if row.col == std.sql.null then ... end`.
    sql_tbl.set("null", LuaValue::NULL)?;

    // ── std.sql.query ─────────────────────────────────────────────────────
    {
        let ctx_conn = Arc::clone(&ctx.sql_conn);
        let ctx_interrupt = Arc::clone(&ctx.sql_interrupt);
        sql_tbl.set(
            "query",
            lua.create_async_function(move |lua, (sql, params): (String, Option<LuaTable>)| {
                let conn = Arc::clone(&ctx_conn);
                let interrupt = Arc::clone(&ctx_interrupt);
                // Build params before entering async move: Lua ownership is
                // required here, and Vec<Value> is Send so it can be moved
                // into the spawn_blocking closure.
                let params_result = params
                    .map(|t| lua_params_to_values(&t))
                    .transpose()
                    .map_err(LuaError::external);
                async move {
                    let params_vec = params_result?.unwrap_or_default();
                    let fut = tokio::task::spawn_blocking(move || {
                        let guard = lock_conn(&conn);
                        run_query(&guard, &sql, &params_vec)
                    });
                    let timeout = super::config::sql_query_timeout();
                    let rows = race_timeout(fut, timeout, &interrupt, "sql.query").await?;
                    rows_to_lua(&lua, rows)
                }
            })?,
        )?;
    }

    // ── std.sql.exec ──────────────────────────────────────────────────────
    {
        let ctx_conn = Arc::clone(&ctx.sql_conn);
        let ctx_interrupt = Arc::clone(&ctx.sql_interrupt);
        sql_tbl.set(
            "exec",
            lua.create_async_function(move |lua, (sql, params): (String, Option<LuaTable>)| {
                let conn = Arc::clone(&ctx_conn);
                let interrupt = Arc::clone(&ctx_interrupt);
                let params_result = params
                    .map(|t| lua_params_to_values(&t))
                    .transpose()
                    .map_err(LuaError::external);
                async move {
                    let params_vec = params_result?.unwrap_or_default();
                    let fut = tokio::task::spawn_blocking(move || {
                        let guard = lock_conn(&conn);
                        run_exec(&guard, &sql, &params_vec)
                    });
                    let timeout = super::config::sql_query_timeout();
                    let (affected, last_id) =
                        race_timeout(fut, timeout, &interrupt, "sql.exec").await?;

                    let result_tbl = lua.create_table()?;
                    result_tbl.set("affected", affected as i64)?;
                    result_tbl.set("last_id", last_id)?;
                    Ok(LuaValue::Table(result_tbl))
                }
            })?,
        )?;
    }

    let std_ns: LuaTable = lua.globals().get("std")?;
    std_ns.set("sql", sql_tbl)?;

    // Load std.sql.register_tools (LLM-facing helper; requires `tool` global).
    lua.load(include_str!("sql_tools.lua"))
        .set_name("std.sql.register_tools")
        .exec()?;

    Ok(())
}
