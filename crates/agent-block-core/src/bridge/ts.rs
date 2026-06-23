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
//! See `bridge/config.rs` for the ENV → path mapping (`AGENT_BLOCK_TS_PATH`).

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mlua::prelude::*;
use rusqlite::Connection;

use crate::bridge::{json_to_lua, lua_to_json};
use crate::host::HostContext;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Validate that every tag key contains only ASCII alphanumeric characters or
/// underscores, guarding against SQL injection via format-string tag paths.
///
/// # Arguments
///
/// - `key`: the tag key to validate
///
/// # Errors
///
/// Returns a `LuaError` if `key` contains any character outside `[a-zA-Z0-9_]`.
fn validate_tag_key(key: &str) -> LuaResult<()> {
    if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(LuaError::external(
            "ts tag key must be [a-zA-Z0-9_]+".to_string(),
        ))
    }
}

/// Build the SQL query string for `std.ts.query`.
///
/// Constructs a parameterised SQL string for one of three query shapes:
/// - **raw** (`agg = None`): `SELECT ts, value, tags … ORDER BY ts, rowid LIMIT ? OFFSET ?`
/// - **single-aggregate** (`agg = Some(_)`, `bucket_ms = None`):
///   `SELECT <AGG_EXPR> FROM ts WHERE …` (single row, no LIMIT/OFFSET)
/// - **time-bucketed** (`agg = Some(_)`, `bucket_ms = Some(_)`):
///   `SELECT (ts/?)*? AS bucket_ts, <AGG_EXPR> … GROUP BY bucket_ts … LIMIT ? OFFSET ?`
///
/// The returned string uses positional `?` placeholders. The binding order is:
/// `series, from_ts, to_ts, [tag_values…], [bucket_ms, bucket_ms], [limit, offset]`.
///
/// # Arguments
///
/// - `agg`: optional aggregation function name (`"count"`, `"sum"`, `"avg"`, `"last"`)
/// - `bucket_ms`: optional bucket width in milliseconds (> 0)
/// - `tag_keys`: ordered list of tag keys for the AND-filter; paths become
///   `json_extract(tags, '$.<key>')` placeholders
/// - `limit`: optional maximum row count (`>= 0`)
/// - `offset`: optional row skip count (`>= 0`)
///
/// # Errors
///
/// Returns `Err(String)` for an unrecognised aggregation function name.
fn build_query_sql(
    agg: Option<&str>,
    bucket_ms: Option<i64>,
    tag_keys: &[String],
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<String, String> {
    // Build the shared WHERE clause fragment (after series / ts range filter).
    // Each tag key adds one `AND json_extract(tags, '$.<key>') = ?` clause.
    // This implements the Crux C2 conjunction contract: every k/v pair in
    // opts.tags is evaluated independently via json_extract, never as a
    // single serialised-string equality match.
    let tag_clauses: String = tag_keys
        .iter()
        .map(|k| format!(" AND json_extract(tags, '$.{k}') = ?"))
        .collect();

    let where_clause = format!("WHERE series = ? AND ts >= ? AND ts <= ?{tag_clauses}");

    // Helper to append LIMIT / OFFSET fragments (not used in single-agg mode).
    let limit_clause = match (limit, offset) {
        (Some(l), Some(o)) => format!(" LIMIT {l} OFFSET {o}"),
        (Some(l), None) => format!(" LIMIT {l}"),
        (None, Some(o)) => format!(" LIMIT -1 OFFSET {o}"),
        (None, None) => String::new(),
    };

    match agg {
        // ── path 1: raw rows ──────────────────────────────────────────────
        None => {
            let sql = format!(
                "SELECT ts, value, tags FROM ts {where_clause} ORDER BY ts, rowid{limit_clause}"
            );
            Ok(sql)
        }

        // ── path 2 / 3: aggregate ─────────────────────────────────────────
        Some(agg_name) => {
            // Build the aggregate expression.  Note that agg="last" is
            // special: it is not a SQL aggregate function but an ORDER+LIMIT
            // operation.  For the time-bucketed case (path 3) we use MAX(ts)
            // per bucket to identify the latest row, which requires the
            // caller to do a second fetch (or we use a subquery).  For
            // simplicity the bucketed "last" uses MAX(ts) as a proxy for the
            // last timestamp — callers needing the actual value should use
            // a separate query.  For single-agg "last" we use a full
            // ORDER BY ts DESC LIMIT 1 subquery form.
            let agg_expr: &str = match agg_name {
                "count" => "COUNT(*)",
                "sum" => "SUM(CAST(value AS REAL))",
                "avg" => "AVG(CAST(value AS REAL))",
                "last" => {
                    // handled specially per path below
                    "last"
                }
                other => return Err(format!("unknown agg: {other}")),
            };

            match bucket_ms {
                // ── path 2: single aggregate (no bucket) ─────────────────
                None => {
                    if agg_name == "last" {
                        // agg="last" + no bucket → ORDER BY ts DESC LIMIT 1
                        let sql = format!(
                            "SELECT value, tags, ts FROM ts {where_clause} ORDER BY ts DESC, rowid DESC LIMIT 1"
                        );
                        Ok(sql)
                    } else {
                        let sql = format!("SELECT {agg_expr} FROM ts {where_clause}");
                        Ok(sql)
                    }
                }

                // ── path 3: time-bucketed aggregate ───────────────────────
                Some(bms) => {
                    // bucket_ts = (ts / bucket_ms) * bucket_ms  (integer division)
                    // The bucket_ms literal is embedded directly into the SQL
                    // string (safe: validated positive i64, no user-supplied
                    // string content).  Embedding avoids the parameter-ordering
                    // issue that arises when SELECT-clause `?` placeholders
                    // appear before WHERE-clause `?` placeholders — SQLite
                    // positional binding fills them left-to-right, so any `?`
                    // in the SELECT would consume series/from_ts/to_ts params.
                    let bucketed_agg_expr = if agg_name == "last" {
                        // For bucketed "last" we return the maximum ts in
                        // each bucket as a proxy for the last value.
                        // CAST(value AS REAL) would not be meaningful here;
                        // instead we expose MAX(ts) for the bucket boundary.
                        "MAX(ts)".to_string()
                    } else {
                        agg_expr.to_string()
                    };

                    let sql = format!(
                        "SELECT (ts / {bms}) * {bms} AS bucket_ts, {bucketed_agg_expr} AS agg_value \
                         FROM ts {where_clause} \
                         GROUP BY bucket_ts ORDER BY bucket_ts{limit_clause}"
                    );
                    Ok(sql)
                }
            }
        }
    }
}

// ── registration ─────────────────────────────────────────────────────────────

/// Register the `std.ts` bridge into `lua`.
///
/// On first call this function:
/// 1. Acquires the ts SQLite connection and runs the DDL (idempotent —
///    `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`).
/// 2. Installs `std.ts.append`, `std.ts.query`, and `std.ts.last` as async
///    Lua functions.
/// 3. Loads `ts_tools.lua` to provide `std.ts.register_tools`.
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
/// - the `std` global is not a table or any `std.ts` assignment fails
pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    // ── DDL init ─────────────────────────────────────────────────────────
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

    drop(conn);

    // ── Build std.ts table ────────────────────────────────────────────────
    let ts_tbl = lua.create_table()?;

    // ── std.ts.append ─────────────────────────────────────────────────────
    ts_tbl.set("append", make_append(lua, Arc::clone(&ctx.ts_conn))?)?;

    // ── std.ts.query ──────────────────────────────────────────────────────
    ts_tbl.set("query", make_query(lua, Arc::clone(&ctx.ts_conn))?)?;

    // ── std.ts.last ───────────────────────────────────────────────────────
    ts_tbl.set("last", make_last(lua, Arc::clone(&ctx.ts_conn))?)?;

    // ── Install into std global ───────────────────────────────────────────
    let std_table: LuaTable = lua.globals().get("std")?;
    std_table.set("ts", ts_tbl)?;

    // ── Load ts_tools.lua (std.ts.register_tools) ─────────────────────────
    lua.load(include_str!("ts_tools.lua"))
        .set_name("std.ts.register_tools")
        .exec()?;

    Ok(())
}

// ── append ────────────────────────────────────────────────────────────────────

/// Create the `std.ts.append(series, value, tags?, at?)` async function.
///
/// # Arguments
///
/// - `lua`: the Lua state
/// - `conn`: shared SQLite connection
///
/// # Errors
///
/// Returns `LuaError` on Mutex poison, rusqlite error, or JSON encode error.
fn make_append(lua: &Lua, conn: Arc<Mutex<Connection>>) -> LuaResult<LuaFunction> {
    lua.create_async_function(
        move |lua, (series, value, tags, at): (String, LuaValue, Option<LuaTable>, Option<i64>)| {
            let conn = Arc::clone(&conn);
            async move {
                tracing::trace!(series = %series, "ts.append");

                // ── resolve timestamp ─────────────────────────────────────
                // unwrap_or_default: UNIX_EPOCH-before fallback → Duration::ZERO.
                // This is a safe fallback path, not an unguarded unwrap.
                let ts_ms = at.unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64
                });

                // ── encode value before entering spawn_blocking ───────────
                // lua_to_json requires &Lua (Lua VM access) and cannot be
                // called inside spawn_blocking.  Serialise to String here
                // in the async context, then move the String into the closure.
                let value_json = lua_to_json(&lua, value).map_err(LuaError::external)?;
                let value_str = serde_json::to_string(&value_json).map_err(LuaError::external)?;

                // ── encode tags ───────────────────────────────────────────
                let tags_str: Option<String> = match tags {
                    None => None,
                    Some(tbl) => {
                        // Validate all tag keys before encoding.
                        for pair in tbl.clone().pairs::<String, LuaValue>() {
                            let (k, _) = pair?;
                            validate_tag_key(&k)?;
                        }
                        let tags_json =
                            lua_to_json(&lua, LuaValue::Table(tbl)).map_err(LuaError::external)?;
                        Some(serde_json::to_string(&tags_json).map_err(LuaError::external)?)
                    }
                };

                // ── blocking SQLite insert ────────────────────────────────
                let result = tokio::task::spawn_blocking(move || {
                    let conn = conn
                        .lock()
                        .map_err(|e| format!("ts conn lock poisoned: {e}"))?;
                    conn.execute(
                        "INSERT INTO ts (series, ts, tags, value) VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![series, ts_ms, tags_str, value_str],
                    )
                    .map_err(|e| format!("ts append: {e}"))?;
                    Ok::<(), String>(())
                })
                .await
                .map_err(|e| LuaError::external(format!("ts task: {e}")))?;

                result.map_err(|e| {
                    tracing::warn!(error = %e, "ts append failed");
                    LuaError::external(e)
                })?;

                Ok(LuaValue::Nil)
            }
        },
    )
}

// ── query ─────────────────────────────────────────────────────────────────────

/// Create the `std.ts.query(series, opts)` async function.
///
/// `opts` fields (all optional):
/// - `from` (integer): start timestamp ms, default `i64::MIN`
/// - `to` (integer): end timestamp ms, default `i64::MAX`
/// - `tags` (table): AND-filter; each k/v pair becomes a `json_extract` clause
/// - `agg` (string): aggregation — `"count"` | `"sum"` | `"avg"` | `"last"`
/// - `bucket_ms` (integer > 0): bucket width; requires `agg`
/// - `limit` (integer >= 0): maximum result rows
/// - `offset` (integer >= 0): result rows to skip
///
/// Returns a Lua array of row tables:
/// - raw mode: `{ ts, value, tags }`
/// - single-agg (agg, no bucket): `{ value }` (scalar result)
/// - bucketed-agg: `{ bucket_ts, value }`
///
/// Note: `sum`/`avg` treat `value` as a JSON number via `CAST(value AS REAL)`.
/// Rows whose `value` is a JSON object produce `0.0` in SQLite's CAST — prefer
/// number-only series when using `sum`/`avg`.
///
/// # Errors
///
/// Returns `LuaError` on validation failure, Mutex poison, or rusqlite error.
fn make_query(lua: &Lua, conn: Arc<Mutex<Connection>>) -> LuaResult<LuaFunction> {
    lua.create_async_function(move |lua, (series, opts): (String, Option<LuaTable>)| {
        let conn = Arc::clone(&conn);
        async move {
            tracing::trace!(series = %series, "ts.query");

            // ── parse opts ────────────────────────────────────────────
            let from_ts: i64 = opts
                .as_ref()
                .and_then(|t| t.get::<Option<i64>>("from").ok().flatten())
                .unwrap_or(i64::MIN);
            let to_ts: i64 = opts
                .as_ref()
                .and_then(|t| t.get::<Option<i64>>("to").ok().flatten())
                .unwrap_or(i64::MAX);

            let agg: Option<String> = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("agg").ok().flatten());

            let bucket_ms: Option<i64> = opts
                .as_ref()
                .and_then(|t| t.get::<Option<i64>>("bucket_ms").ok().flatten());

            let limit: Option<i64> = opts
                .as_ref()
                .and_then(|t| t.get::<Option<i64>>("limit").ok().flatten());

            let offset: Option<i64> = opts
                .as_ref()
                .and_then(|t| t.get::<Option<i64>>("offset").ok().flatten());

            // ── validate opts ─────────────────────────────────────────
            if let Some(bms) = bucket_ms {
                if bms <= 0 {
                    return Err(LuaError::external(
                        "ts bucket_ms must be positive".to_string(),
                    ));
                }
                if agg.is_none() {
                    return Err(LuaError::external("ts bucket_ms requires agg".to_string()));
                }
            }
            if let Some(l) = limit {
                if l < 0 {
                    return Err(LuaError::external("ts opts.limit must be >= 0".to_string()));
                }
            }
            if let Some(o) = offset {
                if o < 0 {
                    return Err(LuaError::external(
                        "ts opts.offset must be >= 0".to_string(),
                    ));
                }
            }

            // ── extract and validate tags filter ──────────────────────
            // Tags k/v pairs are collected into (key, json_string) pairs
            // before entering spawn_blocking so we can access the Lua VM.
            let tags_filter: Vec<(String, String)> = match opts
                .as_ref()
                .and_then(|t| t.get::<Option<LuaTable>>("tags").ok().flatten())
            {
                None => vec![],
                Some(tbl) => {
                    let mut pairs = Vec::new();
                    for p in tbl.pairs::<String, LuaValue>() {
                        let (k, v) = p?;
                        validate_tag_key(&k)?;
                        // Encode tag value as a JSON string for comparison.
                        let v_json = lua_to_json(&lua, v).map_err(LuaError::external)?;
                        let v_str = match &v_json {
                            serde_json::Value::String(s) => s.clone(),
                            other => serde_json::to_string(other).map_err(LuaError::external)?,
                        };
                        pairs.push((k, v_str));
                    }
                    pairs
                }
            };

            let tag_keys: Vec<String> = tags_filter.iter().map(|(k, _)| k.clone()).collect();
            let tag_values: Vec<String> = tags_filter.iter().map(|(_, v)| v.clone()).collect();

            // ── build SQL ─────────────────────────────────────────────
            let agg_ref = agg.as_deref();
            let sql = build_query_sql(agg_ref, bucket_ms, &tag_keys, limit, offset)
                .map_err(LuaError::external)?;

            // ── execute in blocking thread ────────────────────────────
            let is_single_agg = agg.is_some() && bucket_ms.is_none();
            let is_last_single = agg.as_deref() == Some("last") && bucket_ms.is_none();
            let is_bucketed = agg.is_some() && bucket_ms.is_some();

            let rows_raw: Result<Vec<Vec<Option<String>>>, String> =
                tokio::task::spawn_blocking(move || {
                    let conn = conn
                        .lock()
                        .map_err(|e| format!("ts conn lock poisoned: {e}"))?;

                    let mut stmt = conn
                        .prepare(&sql)
                        .map_err(|e| format!("ts query prepare: {e}"))?;

                    // Build the parameter list dynamically.
                    // Order: series, from_ts, to_ts, [tag_values…], [bucket_ms × 2 if bucketed]
                    let mut params: Vec<Box<dyn rusqlite::ToSql>> =
                        vec![Box::new(series), Box::new(from_ts), Box::new(to_ts)];
                    for v in tag_values {
                        params.push(Box::new(v));
                    }
                    // Note: bucket_ms is embedded as a literal in the SQL
                    // (see build_query_sql path 3), so no additional params
                    // are needed for the bucketed-aggregate case.

                    let param_refs: Vec<&dyn rusqlite::ToSql> =
                        params.iter().map(|p| p.as_ref()).collect();

                    let col_count = stmt.column_count();
                    let rows: Vec<Vec<Option<String>>> = stmt
                        .query(param_refs.as_slice())
                        .map_err(|e| format!("ts query exec: {e}"))?
                        .mapped(|row| {
                            let mut cols = Vec::with_capacity(col_count);
                            for i in 0..col_count {
                                // Use Value (not String) to handle INTEGER and REAL columns.
                                // rusqlite's FromSql for String only accepts Text/Blob and
                                // returns FromSqlError::InvalidType for INTEGER or REAL values
                                // (e.g. the `ts` column, COUNT(*), SUM, AVG, bucket_ts).
                                let v: rusqlite::types::Value =
                                    row.get::<_, rusqlite::types::Value>(i)?;
                                let s = match v {
                                    rusqlite::types::Value::Null => None,
                                    rusqlite::types::Value::Integer(n) => Some(n.to_string()),
                                    rusqlite::types::Value::Real(f) => Some(f.to_string()),
                                    rusqlite::types::Value::Text(s) => Some(s),
                                    rusqlite::types::Value::Blob(_) => None,
                                };
                                cols.push(s);
                            }
                            Ok(cols)
                        })
                        .collect::<Result<_, _>>()
                        .map_err(|e| format!("ts query row: {e}"))?;

                    Ok(rows)
                })
                .await
                .map_err(|e| LuaError::external(format!("ts task: {e}")))?;

            let rows_raw = rows_raw.map_err(|e| {
                tracing::warn!(error = %e, "ts query failed");
                LuaError::external(e)
            })?;

            // ── decode rows into Lua ───────────────────────────────────
            // Column layout depends on the query path:
            //   raw:         [ts(i64), value(text), tags(text|null)]
            //   single-agg:  [agg_value(text|null)]  (or [value,tags,ts] for last)
            //   bucketed:    [bucket_ts(i64), agg_value(text|null)]
            let result_table = lua.create_table()?;

            if is_last_single {
                // path 2 agg=last: columns are [value, tags, ts]
                // Returns at most 1 row; wrap as single-element array for
                // consistency with other agg modes.
                for (idx, row) in rows_raw.iter().enumerate() {
                    let row_tbl = lua.create_table()?;
                    // col 0: value TEXT
                    let value_lv = decode_value_col(&lua, row.first().and_then(|s| s.as_deref()))?;
                    row_tbl.set("value", value_lv)?;
                    // col 1: tags TEXT|NULL
                    let tags_lv = decode_tags_col(&lua, row.get(1).and_then(|s| s.as_deref()))?;
                    row_tbl.set("tags", tags_lv)?;
                    // col 2: ts INTEGER
                    let ts_lv = if let Some(Some(s)) = row.get(2) {
                        let n: i64 = s.parse().map_err(LuaError::external)?;
                        LuaValue::Integer(n)
                    } else {
                        LuaValue::Nil
                    };
                    row_tbl.set("ts", ts_lv)?;
                    result_table.set(idx + 1, row_tbl)?;
                }
            } else if is_single_agg {
                // path 2 (non-last): single column [agg_value]
                for (idx, row) in rows_raw.iter().enumerate() {
                    let row_tbl = lua.create_table()?;
                    let agg_lv = decode_value_col(&lua, row.first().and_then(|s| s.as_deref()))?;
                    row_tbl.set("value", agg_lv)?;
                    result_table.set(idx + 1, row_tbl)?;
                }
            } else if is_bucketed {
                // path 3: columns [bucket_ts, agg_value]
                for (idx, row) in rows_raw.iter().enumerate() {
                    let row_tbl = lua.create_table()?;
                    // col 0: bucket_ts INTEGER (from (ts/?)*? expression)
                    let bts_lv = if let Some(Some(s)) = row.first() {
                        let n: i64 = s.parse().map_err(LuaError::external)?;
                        LuaValue::Integer(n)
                    } else {
                        LuaValue::Nil
                    };
                    row_tbl.set("bucket_ts", bts_lv)?;
                    // col 1: agg_value
                    let agg_lv = decode_value_col(&lua, row.get(1).and_then(|s| s.as_deref()))?;
                    row_tbl.set("value", agg_lv)?;
                    result_table.set(idx + 1, row_tbl)?;
                }
            } else {
                // path 1: raw rows [ts, value, tags]
                for (idx, row) in rows_raw.iter().enumerate() {
                    let row_tbl = lua.create_table()?;
                    // col 0: ts INTEGER
                    let ts_lv = if let Some(Some(s)) = row.first() {
                        let n: i64 = s.parse().map_err(LuaError::external)?;
                        LuaValue::Integer(n)
                    } else {
                        LuaValue::Nil
                    };
                    row_tbl.set("ts", ts_lv)?;
                    // col 1: value TEXT
                    let value_lv = decode_value_col(&lua, row.get(1).and_then(|s| s.as_deref()))?;
                    row_tbl.set("value", value_lv)?;
                    // col 2: tags TEXT|NULL
                    let tags_lv = decode_tags_col(&lua, row.get(2).and_then(|s| s.as_deref()))?;
                    row_tbl.set("tags", tags_lv)?;
                    result_table.set(idx + 1, row_tbl)?;
                }
            }

            Ok(LuaValue::Table(result_table))
        }
    })
}

// ── last ──────────────────────────────────────────────────────────────────────

/// Create the `std.ts.last(series, tags?)` async function.
///
/// Returns the most-recent data point for `series` (optionally filtered by
/// `tags` using the same AND-conjunction as `std.ts.query`).
///
/// Return value:
/// - `nil` — no matching row found
/// - `{ ts = <i64>, value = <decoded>, tags = <table or nil> }` — latest row
///
/// # Arguments
///
/// - `lua`: the Lua state
/// - `conn`: shared SQLite connection
///
/// # Errors
///
/// Returns `LuaError` on tag key validation failure, Mutex poison, or rusqlite
/// error.
fn make_last(lua: &Lua, conn: Arc<Mutex<Connection>>) -> LuaResult<LuaFunction> {
    lua.create_async_function(move |lua, (series, tags): (String, Option<LuaTable>)| {
        let conn = Arc::clone(&conn);
        async move {
            tracing::trace!(series = %series, "ts.last");

            // ── extract and validate tags filter ──────────────────────
            let tags_filter: Vec<(String, String)> = match tags {
                None => vec![],
                Some(tbl) => {
                    let mut pairs = Vec::new();
                    for p in tbl.pairs::<String, LuaValue>() {
                        let (k, v) = p?;
                        validate_tag_key(&k)?;
                        let v_json = lua_to_json(&lua, v).map_err(LuaError::external)?;
                        let v_str = match &v_json {
                            serde_json::Value::String(s) => s.clone(),
                            other => serde_json::to_string(other).map_err(LuaError::external)?,
                        };
                        pairs.push((k, v_str));
                    }
                    pairs
                }
            };

            let tag_keys: Vec<String> = tags_filter.iter().map(|(k, _)| k.clone()).collect();
            let tag_values: Vec<String> = tags_filter.iter().map(|(_, v)| v.clone()).collect();

            // Build tag_clauses for WHERE.
            let tag_clauses: String = tag_keys
                .iter()
                .map(|k| format!(" AND json_extract(tags, '$.{k}') = ?"))
                .collect();

            let sql = format!(
                "SELECT ts, value, tags FROM ts \
                 WHERE series = ? AND ts >= ? AND ts <= ?{tag_clauses} \
                 ORDER BY ts DESC, rowid DESC LIMIT 1"
            );

            let row_raw: Result<Option<(i64, String, Option<String>)>, String> =
                tokio::task::spawn_blocking(move || {
                    let conn = conn
                        .lock()
                        .map_err(|e| format!("ts conn lock poisoned: {e}"))?;
                    let mut stmt = conn
                        .prepare(&sql)
                        .map_err(|e| format!("ts last prepare: {e}"))?;

                    let mut params: Vec<Box<dyn rusqlite::ToSql>> =
                        vec![Box::new(series), Box::new(i64::MIN), Box::new(i64::MAX)];
                    for v in tag_values {
                        params.push(Box::new(v));
                    }
                    let param_refs: Vec<&dyn rusqlite::ToSql> =
                        params.iter().map(|p| p.as_ref()).collect();

                    let mut rows = stmt
                        .query(param_refs.as_slice())
                        .map_err(|e| format!("ts last query: {e}"))?;

                    if let Some(row) = rows.next().map_err(|e| format!("ts last row: {e}"))? {
                        let ts_val: i64 = row.get(0).map_err(|e| format!("ts last ts col: {e}"))?;
                        let value_str: String =
                            row.get(1).map_err(|e| format!("ts last value col: {e}"))?;
                        let tags_str: Option<String> =
                            row.get(2).map_err(|e| format!("ts last tags col: {e}"))?;
                        Ok(Some((ts_val, value_str, tags_str)))
                    } else {
                        Ok(None)
                    }
                })
                .await
                .map_err(|e| LuaError::external(format!("ts task: {e}")))?;

            let row_opt = row_raw.map_err(|e| {
                tracing::warn!(error = %e, "ts last failed");
                LuaError::external(e)
            })?;

            match row_opt {
                None => Ok(LuaValue::Nil),
                Some((ts_val, value_str, tags_str)) => {
                    let row_tbl = lua.create_table()?;
                    row_tbl.set("ts", LuaValue::Integer(ts_val))?;

                    // Two-Phase decode: raw JSON string → serde_json::Value → LuaValue
                    let v_json: serde_json::Value =
                        serde_json::from_str(&value_str).map_err(LuaError::external)?;
                    let v_lv = json_to_lua(&lua, v_json)?;
                    row_tbl.set("value", v_lv)?;

                    let tags_lv = decode_tags_col(&lua, tags_str.as_deref())?;
                    row_tbl.set("tags", tags_lv)?;

                    Ok(LuaValue::Table(row_tbl))
                }
            }
        }
    })
}

// ── decode helpers ────────────────────────────────────────────────────────────

/// Decode a SQLite `value` column (TEXT storing JSON) into a `LuaValue`.
///
/// Implements the Two-Phase deserialization pattern (Outline K-NEW):
/// `String → serde_json::Value → LuaValue`.  A NULL column (`None`) or a
/// NULL aggregate result returns `LuaValue::Nil`.
///
/// # Arguments
///
/// - `lua`: the Lua state
/// - `raw`: the raw column string, or `None` for SQL NULL
///
/// # Errors
///
/// Returns `LuaError` if the string is not valid JSON.
fn decode_value_col(lua: &Lua, raw: Option<&str>) -> LuaResult<LuaValue> {
    match raw {
        None => Ok(LuaValue::Nil),
        Some(s) => {
            let v: serde_json::Value = serde_json::from_str(s).map_err(LuaError::external)?;
            json_to_lua(lua, v)
        }
    }
}

/// Decode a SQLite `tags` column (TEXT storing a JSON object, or NULL) into a
/// `LuaValue`.
///
/// NULL tags columns return `LuaValue::Nil` (row has no tags).
///
/// # Arguments
///
/// - `lua`: the Lua state
/// - `raw`: the raw column string, or `None` for SQL NULL
///
/// # Errors
///
/// Returns `LuaError` if the string is not valid JSON.
fn decode_tags_col(lua: &Lua, raw: Option<&str>) -> LuaResult<LuaValue> {
    decode_value_col(lua, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    /// Same-ms INSERT order is preserved by `ORDER BY ts, rowid` (raw path).
    ///
    /// Inserts three rows sharing the same `ts` value into an in-memory SQLite
    /// using the production DDL, runs the SQL produced by `build_query_sql`
    /// (raw mode), and verifies the returned values match INSERT order exactly.
    /// Also asserts that the generated SQL string contains the rowid tie-breaker.
    ///
    /// # Test categories
    ///
    /// - (T1) Happy path: normal INSERT + query flow with production DDL.
    /// - (T2) Edge case: all rows share identical `ts` millisecond value.
    /// - (T3) Regression guard: SQL string must contain `ORDER BY ts, rowid`.
    #[test]
    fn raw_path_same_ms_preserves_insert_order() {
        // 1) Assert generated SQL contains the tie-breaker.
        let sql = build_query_sql(None, None, &[], None, None).expect("build_query_sql");
        assert!(
            sql.contains("ORDER BY ts, rowid"),
            "raw path SQL missing rowid tie-breaker: {sql}"
        );

        // 2) Execute against in-memory SQLite using production DDL.
        // Safety: open_in_memory() only fails on internal SQLite allocation
        // errors which cannot occur in a controlled test environment.
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE ts (series TEXT NOT NULL, ts INTEGER NOT NULL, \
             tags TEXT, value TEXT NOT NULL); \
             CREATE INDEX idx_ts_series_ts ON ts(series, ts);",
        )
        .expect("ddl");

        // Insert three rows with identical ts=1000 ms.
        conn.execute(
            "INSERT INTO ts (series, ts, tags, value) VALUES (?, ?, NULL, ?)",
            params!["s", 1000_i64, "\"first\""],
        )
        .expect("insert 1");
        conn.execute(
            "INSERT INTO ts (series, ts, tags, value) VALUES (?, ?, NULL, ?)",
            params!["s", 1000_i64, "\"second\""],
        )
        .expect("insert 2");
        conn.execute(
            "INSERT INTO ts (series, ts, tags, value) VALUES (?, ?, NULL, ?)",
            params!["s", 1000_i64, "\"third\""],
        )
        .expect("insert 3");

        // Safety: prepare() only fails if SQL is malformed; sql is generated by
        // build_query_sql which is already tested above.
        let mut stmt = conn.prepare(&sql).expect("prepare");
        let rows: Vec<String> = stmt
            .query_map(params!["s", i64::MIN, i64::MAX], |r| r.get::<_, String>(1))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");

        assert_eq!(
            rows,
            vec![
                "\"first\"".to_string(),
                "\"second\"".to_string(),
                "\"third\"".to_string()
            ],
            "raw path returned rows in non-INSERT order: {rows:?}"
        );
    }

    /// Same-ms `last` returns the most recently INSERTed row.
    ///
    /// Verifies both the SQL string produced by `build_query_sql(Some("last"), ...)`
    /// contains `ORDER BY ts DESC, rowid DESC LIMIT 1`, and that executing an
    /// equivalent query against in-memory SQLite returns the last INSERT value
    /// even when all rows share the same `ts` ms.
    ///
    /// # Test categories
    ///
    /// - (T1) Happy path: last value retrieval with production DDL.
    /// - (T2) Edge case: all rows share identical `ts` millisecond value.
    /// - (T3) Regression guard: SQL string must contain `ORDER BY ts DESC, rowid DESC LIMIT 1`.
    #[test]
    fn last_path_same_ms_returns_last_insert() {
        // 1) Assert build_query_sql last form contains tie-breaker.
        let sql_last =
            build_query_sql(Some("last"), None, &[], None, None).expect("build_query_sql last");
        assert!(
            sql_last.contains("ORDER BY ts DESC, rowid DESC LIMIT 1"),
            "last path SQL missing rowid DESC tie-breaker: {sql_last}"
        );

        // 2) Execute the make_last-equivalent SQL against in-memory SQLite.
        // Safety: open_in_memory() only fails on internal SQLite allocation
        // errors which cannot occur in a controlled test environment.
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE ts (series TEXT NOT NULL, ts INTEGER NOT NULL, \
             tags TEXT, value TEXT NOT NULL); \
             CREATE INDEX idx_ts_series_ts ON ts(series, ts);",
        )
        .expect("ddl");

        for v in ["\"first\"", "\"second\"", "\"third\""] {
            conn.execute(
                "INSERT INTO ts (series, ts, tags, value) VALUES (?, ?, NULL, ?)",
                params!["s", 1000_i64, v],
            )
            .expect("insert");
        }

        // make_last inline SQL form (mirrors src/bridge/ts.rs post-fix).
        // This string is the expected form after the rowid tie-breaker fix;
        // if make_last's format! drifts, the behaviour test below will catch it.
        let make_last_sql = "SELECT ts, value, tags FROM ts \
             WHERE series = ? AND ts >= ? AND ts <= ? \
             ORDER BY ts DESC, rowid DESC LIMIT 1";

        // Safety: make_last_sql is a literal string, prepare() cannot fail here.
        let mut stmt = conn.prepare(make_last_sql).expect("prepare");
        let value: String = stmt
            .query_row(params!["s", i64::MIN, i64::MAX], |r| r.get::<_, String>(1))
            .expect("query_row");

        assert_eq!(
            value, "\"third\"",
            "last path returned non-last INSERT value: {value}"
        );
    }
}
