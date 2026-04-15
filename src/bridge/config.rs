//! Config resolution for `std.kv` / `std.sql` storage backends.
//!
//! All knobs are ENV-driven (no CLI flags) so `.env` can drive them uniformly.
//!
//! | ENV var                            | Default                  | Used by  |
//! |------------------------------------|--------------------------|----------|
//! | `AGENT_BLOCK_HOME`                 | `$HOME/.agent-block`     | both     |
//! | `AGENT_BLOCK_KV_PATH`              | `{HOME}/kv.sqlite`       | std.kv   |
//! | `AGENT_BLOCK_SQL_PATH`             | `{HOME}/db.sqlite`       | std.sql  |
//! | `AGENT_BLOCK_SQL_BUSY_TIMEOUT_MS`  | `5000`                   | both     |
//! | `AGENT_BLOCK_SQL_QUERY_TIMEOUT_MS` | `5000`                   | both     |
//! | `AGENT_BLOCK_SQL_JOURNAL_MODE`     | `WAL`                    | both     |
//!
//! `std.kv` and `std.sql` are backed by separate SQLite database files so
//! that agent-internal KV state and explicit user SQL data don't share WAL,
//! page cache, or backup lifecycle. Pragma/timeout knobs apply to both.
//!
//! Special: `=:memory:` selects an in-memory database (works for both
//! `AGENT_BLOCK_KV_PATH` and `AGENT_BLOCK_SQL_PATH`).
//! Journal mode is ignored for `:memory:` (SQLite forces MEMORY).
//! `AGENT_BLOCK_SQL_QUERY_TIMEOUT_MS=0` disables the query timeout.

use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_SQL_BUSY_TIMEOUT_MS: u64 = 5000;
const DEFAULT_SQL_QUERY_TIMEOUT_MS: u64 = 5000;
const DEFAULT_SQL_JOURNAL_MODE: &str = "WAL";

/// Base dir for agent-block local state.
/// `AGENT_BLOCK_HOME` → `$HOME/.agent-block`.
pub fn base_dir() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("AGENT_BLOCK_HOME") {
        return Ok(PathBuf::from(v));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME env var not set".to_string())?;
    Ok(PathBuf::from(home).join(".agent-block"))
}

/// Path to the std.kv SQLite database file (or `:memory:`).
/// `AGENT_BLOCK_KV_PATH` → `{base_dir}/kv.sqlite`.
pub fn kv_path() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("AGENT_BLOCK_KV_PATH") {
        return Ok(PathBuf::from(v));
    }
    Ok(base_dir()?.join("kv.sqlite"))
}

/// Path to the std.sql SQLite database file (or `:memory:`).
/// `AGENT_BLOCK_SQL_PATH` → `{base_dir}/db.sqlite`.
pub fn sql_path() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("AGENT_BLOCK_SQL_PATH") {
        return Ok(PathBuf::from(v));
    }
    Ok(base_dir()?.join("db.sqlite"))
}

/// True when the resolved path is SQLite's in-memory sentinel.
pub fn is_memory_sql(path: &std::path::Path) -> bool {
    path.as_os_str() == ":memory:"
}

/// SQLite busy_timeout.
/// `AGENT_BLOCK_SQL_BUSY_TIMEOUT_MS` → 5000ms.
pub fn sql_busy_timeout() -> Duration {
    let ms = std::env::var("AGENT_BLOCK_SQL_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SQL_BUSY_TIMEOUT_MS);
    Duration::from_millis(ms)
}

/// SQLite journal_mode pragma value.
/// `AGENT_BLOCK_SQL_JOURNAL_MODE` → `WAL`.
pub fn sql_journal_mode() -> String {
    std::env::var("AGENT_BLOCK_SQL_JOURNAL_MODE")
        .unwrap_or_else(|_| DEFAULT_SQL_JOURNAL_MODE.to_string())
}

/// Per-query timeout. `0` disables the timeout.
/// `AGENT_BLOCK_SQL_QUERY_TIMEOUT_MS` → 5000ms.
pub fn sql_query_timeout() -> Option<Duration> {
    let ms = std::env::var("AGENT_BLOCK_SQL_QUERY_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SQL_QUERY_TIMEOUT_MS);
    if ms == 0 {
        None
    } else {
        Some(Duration::from_millis(ms))
    }
}
