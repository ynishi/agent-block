//! `std.sql` — SQLite (rusqlite WAL) bridge for Lua scripts.
//!
//! Provides:
//! - `std.sql.query(sql, params?) -> rows`   rows = array of { col_name = value, ... }
//! - `std.sql.exec(sql, params?)  -> { affected = N, last_id = M }`
//!
//! rusqlite calls are executed inside `tokio::task::spawn_blocking` to avoid
//! blocking the async runtime.  Lock acquisition is also inside spawn_blocking
//! to prevent holding a Mutex guard across `.await` (await-holding-lock).

// TODO: gate DDL at manifest perm check (see issue §Permission 分離)

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
/// - number     → Real(f64)
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
            LuaValue::Number(f) => Value::Real(f),
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
                ValueRef::Real(f) => serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                ValueRef::Text(b) => {
                    serde_json::Value::String(String::from_utf8_lossy(b).to_string())
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
/// Each element is a Lua table `{ col_name = value, ... }`.
/// NULL columns are omitted (Lua tables cannot hold nil values).
fn rows_to_lua(lua: &Lua, rows: Vec<Map<String, serde_json::Value>>) -> LuaResult<LuaValue> {
    let arr = lua.create_table()?;
    for (i, row_map) in rows.into_iter().enumerate() {
        let row_tbl = lua.create_table()?;
        for (col, val) in row_map {
            let lua_val = super::json_to_lua(lua, val)?;
            // Skip NULL: setting nil into a table is a no-op in Lua semantics,
            // and mlua returns an error if you try to set nil explicitly.
            if !matches!(lua_val, LuaValue::Nil) {
                row_tbl.set(col.as_str(), lua_val)?;
            }
        }
        arr.set(i + 1, row_tbl)?;
    }
    Ok(LuaValue::Table(arr))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let sql_tbl = lua.create_table()?;

    // ── std.sql.query ─────────────────────────────────────────────────────
    {
        let ctx_conn = Arc::clone(&ctx.sql_conn);
        sql_tbl.set(
            "query",
            lua.create_async_function(move |lua, (sql, params): (String, Option<LuaTable>)| {
                let conn = Arc::clone(&ctx_conn);
                // Build params before entering async move: Lua ownership is
                // required here, and Vec<Value> is Send so it can be moved
                // into the spawn_blocking closure.
                let params_result = params
                    .map(|t| lua_params_to_values(&t))
                    .transpose()
                    .map_err(LuaError::external);
                async move {
                    let params_vec = params_result?.unwrap_or_default();
                    let rows = tokio::task::spawn_blocking(move || {
                        let guard = conn.lock().expect("sql conn mutex poisoned");
                        run_query(&guard, &sql, &params_vec)
                    })
                    .await
                    .map_err(|e| {
                        warn!(error = %e, "spawn_blocking join error in sql.query");
                        LuaError::external(format!("spawn_blocking: {e}"))
                    })?
                    .map_err(|e| {
                        warn!(error = %e, "sql.query execution error");
                        LuaError::external(e)
                    })?;
                    rows_to_lua(&lua, rows)
                }
            })?,
        )?;
    }

    // ── std.sql.exec ──────────────────────────────────────────────────────
    {
        let ctx_conn = Arc::clone(&ctx.sql_conn);
        sql_tbl.set(
            "exec",
            lua.create_async_function(move |lua, (sql, params): (String, Option<LuaTable>)| {
                let conn = Arc::clone(&ctx_conn);
                let params_result = params
                    .map(|t| lua_params_to_values(&t))
                    .transpose()
                    .map_err(LuaError::external);
                async move {
                    let params_vec = params_result?.unwrap_or_default();
                    let (affected, last_id) = tokio::task::spawn_blocking(move || {
                        let guard = conn.lock().expect("sql conn mutex poisoned");
                        run_exec(&guard, &sql, &params_vec)
                    })
                    .await
                    .map_err(|e| {
                        warn!(error = %e, "spawn_blocking join error in sql.exec");
                        LuaError::external(format!("spawn_blocking: {e}"))
                    })?
                    .map_err(|e| {
                        warn!(error = %e, "sql.exec execution error");
                        LuaError::external(e)
                    })?;

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

    Ok(())
}
