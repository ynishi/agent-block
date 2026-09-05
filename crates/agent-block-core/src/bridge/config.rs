//! Config resolution for `std.kv` / `std.sql` / `std.ts` storage backends.
//!
//! All knobs are ENV-driven (no CLI flags) so `.env` can drive them uniformly.
//!
//! | ENV var                            | Default                  | Used by  |
//! |------------------------------------|--------------------------|----------|
//! | `AGENT_BLOCK_HOME`                 | `$HOME/.agent-block`     | all      |
//! | `AGENT_BLOCK_KV_PATH`              | `{HOME}/kv.sqlite`       | std.kv   |
//! | `AGENT_BLOCK_SQL_PATH`             | `{HOME}/db.sqlite`       | std.sql  |
//! | `AGENT_BLOCK_TS_PATH`              | `{HOME}/ts.sqlite`       | std.ts   |
//! | `AGENT_BLOCK_KNL_PATH`             | `{HOME}/projects/<slug>/knl.sqlite` | knl |
//! | `AGENT_BLOCK_SQL_BUSY_TIMEOUT_MS`  | `5000`                   | all      |
//! | `AGENT_BLOCK_SQL_QUERY_TIMEOUT_MS` | `5000`                   | all      |
//! | `AGENT_BLOCK_SQL_JOURNAL_MODE`     | `WAL`                    | all      |
//! | `AGENT_BLOCK_BUS_CAPACITY`         | `64`                     | EventBus |
//! | `AGENT_BLOCK_TASK_GRACE_MS`        | `1000`                   | task/bus |
//! | `AGENT_BLOCK_UNSEAL`               | unset                    | blocks   |
//!
//! `AGENT_BLOCK_UNSEAL=1` is the one knob here that is not a path or a bound:
//! it downgrades the sealed-module refusal (a project `blocks/knl/`,
//! `knl_adapter`, `knl_types` or `lshape` shadowing the embedded kernel —
//! `host::SEALED`) from an error that ends the run to a `warn!`. It exists for
//! development **on the kernel itself**, where the Lua half is the thing being
//! edited; a project that sets it is running against a kernel the Rust side
//! was not declared against. Any other value, including unset, leaves the
//! refusal in place.
//!
//! `std.kv`, `std.sql`, and `std.ts` are backed by separate SQLite database
//! files so that agent-internal KV state, explicit user SQL data, and
//! time-series rows don't share WAL, page cache, or backup lifecycle.
//! Pragma/timeout knobs apply to all three.
//!
//! The kernel's log is the fourth, and the one that is *per project* rather
//! than per host: a session belongs to the tree of work the script it ran
//! under is part of, so the default lands under `{base_dir}/projects/<slug>/`
//! where `<slug>` names the project root ([`project_slug`]). The other three
//! are unaffected and stay where they are.
//!
//! Special: `=:memory:` selects an in-memory database (works for
//! `AGENT_BLOCK_KV_PATH`, `AGENT_BLOCK_SQL_PATH`, and `AGENT_BLOCK_TS_PATH`).
//! Journal mode is ignored for `:memory:` (SQLite forces MEMORY).
//! `AGENT_BLOCK_SQL_QUERY_TIMEOUT_MS=0` disables the query timeout.

// Not gated: the kernel's database is resolved in every build, because the
// kernel is (see the `sqlite` feature note in this crate's manifest — that
// feature is the `sql.*` / `kv.*` / `ts.*` batteries, not SQLite itself).
use std::path::{Path, PathBuf};
#[cfg(feature = "sqlite")]
use std::time::Duration;

#[cfg(feature = "sqlite")]
const DEFAULT_SQL_BUSY_TIMEOUT_MS: u64 = 5000;
#[cfg(feature = "sqlite")]
const DEFAULT_SQL_QUERY_TIMEOUT_MS: u64 = 5000;
#[cfg(feature = "sqlite")]
const DEFAULT_SQL_JOURNAL_MODE: &str = "WAL";
const DEFAULT_BUS_CAPACITY: usize = 64;
const DEFAULT_TASK_GRACE_MS: u64 = 1000;

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
#[cfg(feature = "sqlite")]
pub fn kv_path() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("AGENT_BLOCK_KV_PATH") {
        return Ok(PathBuf::from(v));
    }
    Ok(base_dir()?.join("kv.sqlite"))
}

/// Path to the std.sql SQLite database file (or `:memory:`).
/// `AGENT_BLOCK_SQL_PATH` → `{base_dir}/db.sqlite`.
#[cfg(feature = "sqlite")]
pub fn sql_path() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("AGENT_BLOCK_SQL_PATH") {
        return Ok(PathBuf::from(v));
    }
    Ok(base_dir()?.join("db.sqlite"))
}

/// Path to the std.ts SQLite database file (or `:memory:`).
///
/// `AGENT_BLOCK_TS_PATH` → `{base_dir}/ts.sqlite`.
/// Separate from kv and sql so the TSDB WAL does not share page cache or
/// backup lifecycle with agent-internal KV or user SQL data.
#[cfg(feature = "sqlite")]
pub fn ts_path() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("AGENT_BLOCK_TS_PATH") {
        return Ok(PathBuf::from(v));
    }
    Ok(base_dir()?.join("ts.sqlite"))
}

/// The directory one project's kernel database lives in, as a single name.
///
/// The project root's absolute path with every separator replaced by `-`, so
/// `/home/u/projects/x` becomes `-home-u-projects-x` — the leading separator
/// keeps its dash, and nothing is hashed. A slug is therefore *readable*: the
/// project a database belongs to can be told from the directory listing, which
/// is the whole reason for the form (it is the one `.claude/projects/<slug>`
/// already uses, so one habit reads both).
///
/// The caller passes the root it resolved — [`crate::host::HostContext`]'s,
/// which is canonicalized — because a relative path would slug two different
/// projects to the same name.
pub fn project_slug(project_root: &Path) -> String {
    project_root
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c == '/' || c == std::path::MAIN_SEPARATOR {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// Path to the kernel's SQLite database for the project rooted at
/// `project_root`.
///
/// `AGENT_BLOCK_KNL_PATH` → `{base_dir}/projects/<slug>/knl.sqlite`.
///
/// Per project rather than per host, unlike the three above: every session a
/// script opens without naming a `store` is a stream in this one file, so the
/// sessions of one project — a tree opened from a default parent included —
/// share a database and can be read back with one statement.
pub fn knl_path(project_root: &Path) -> Result<PathBuf, String> {
    resolve_knl_path(
        std::env::var_os("AGENT_BLOCK_KNL_PATH").map(PathBuf::from),
        project_root,
    )
}

/// [`knl_path`] with the override handed in rather than read.
///
/// The env read is the one thing a test cannot do twice at once, so it stays
/// in the caller above and the rule — an override wins whole, otherwise the
/// per-project default — is a function that can be asked directly.
fn resolve_knl_path(
    override_path: Option<PathBuf>,
    project_root: &Path,
) -> Result<PathBuf, String> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    Ok(base_dir()?
        .join("projects")
        .join(project_slug(project_root))
        .join("knl.sqlite"))
}

/// True when the resolved path is SQLite's in-memory sentinel.
#[cfg(feature = "sqlite")]
pub fn is_memory_sql(path: &std::path::Path) -> bool {
    path.as_os_str() == ":memory:"
}

/// SQLite busy_timeout.
/// `AGENT_BLOCK_SQL_BUSY_TIMEOUT_MS` → 5000ms.
#[cfg(feature = "sqlite")]
pub fn sql_busy_timeout() -> Duration {
    let ms = std::env::var("AGENT_BLOCK_SQL_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SQL_BUSY_TIMEOUT_MS);
    Duration::from_millis(ms)
}

/// SQLite journal_mode pragma value.
/// `AGENT_BLOCK_SQL_JOURNAL_MODE` → `WAL`.
#[cfg(feature = "sqlite")]
pub fn sql_journal_mode() -> String {
    std::env::var("AGENT_BLOCK_SQL_JOURNAL_MODE")
        .unwrap_or_else(|_| DEFAULT_SQL_JOURNAL_MODE.to_string())
}

/// Per-query timeout. `0` disables the timeout.
/// `AGENT_BLOCK_SQL_QUERY_TIMEOUT_MS` → 5000ms.
#[cfg(feature = "sqlite")]
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

/// EventBus bounded mpsc capacity.
/// `AGENT_BLOCK_BUS_CAPACITY` → 64. Parse failures warn and fall back.
pub fn bus_capacity() -> usize {
    match std::env::var("AGENT_BLOCK_BUS_CAPACITY") {
        Ok(v) => v.parse::<usize>().unwrap_or_else(|e| {
            tracing::warn!(
                value = %v,
                error = %e,
                default = DEFAULT_BUS_CAPACITY,
                "AGENT_BLOCK_BUS_CAPACITY parse failed, using default"
            );
            DEFAULT_BUS_CAPACITY
        }),
        Err(_) => DEFAULT_BUS_CAPACITY,
    }
}

/// SIGTERM/SIGINT grace window (ms) shared by `std.task.with_timeout` and the
/// EventBus shutdown path.
/// `AGENT_BLOCK_TASK_GRACE_MS` → 1000. Parse failures warn and fall back.
pub fn task_grace_ms() -> u64 {
    match std::env::var("AGENT_BLOCK_TASK_GRACE_MS") {
        Ok(v) => v.parse::<u64>().unwrap_or_else(|e| {
            tracing::warn!(
                value = %v,
                error = %e,
                default = DEFAULT_TASK_GRACE_MS,
                "AGENT_BLOCK_TASK_GRACE_MS parse failed, using default"
            );
            DEFAULT_TASK_GRACE_MS
        }),
        Err(_) => DEFAULT_TASK_GRACE_MS,
    }
}

/// The kernel database's resolution, held to the two rules it has.
#[cfg(test)]
mod knl_path_tests {
    use super::*;

    /// The slug is the path, readable, with the separators turned into
    /// dashes — the leading one included.
    #[test]
    fn a_slug_is_the_project_path_with_dashes() {
        assert_eq!(
            project_slug(Path::new("/home/u/projects/x")),
            "-home-u-projects-x"
        );
        assert_eq!(project_slug(Path::new("/")), "-");
    }

    /// Two projects are two directories, so two databases: the slug is what
    /// keeps one project's sessions out of another's file.
    #[test]
    fn two_projects_slug_apart() {
        assert_ne!(
            project_slug(Path::new("/home/u/a")),
            project_slug(Path::new("/home/u/b"))
        );
    }

    /// The default lands under `projects/<slug>/knl.sqlite`. Asserted on the
    /// tail rather than the whole path, because the head is `base_dir`'s and
    /// that is the env's to say.
    #[test]
    fn the_default_is_per_project() {
        let path = resolve_knl_path(None, Path::new("/home/u/projects/x")).expect("resolved");
        assert!(
            path.ends_with("projects/-home-u-projects-x/knl.sqlite"),
            "{}",
            path.display()
        );
    }

    /// An override is taken whole: neither the base dir nor the slug is
    /// appended to it, so a caller that names a file gets that file.
    #[test]
    fn an_override_wins() {
        let path = resolve_knl_path(
            Some(PathBuf::from("/tmp/elsewhere/knl.sqlite")),
            Path::new("/home/u/projects/x"),
        )
        .expect("resolved");
        assert_eq!(path, PathBuf::from("/tmp/elsewhere/knl.sqlite"));
    }
}
