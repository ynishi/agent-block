//! `std.task` thin adapter.
//!
//! Bridge implementation moved to the `mlua-batteries` crate
//! (`mlua_batteries::task`).  This module only resolves the host's
//! environment variables into a [`mlua_batteries::task::TaskConfig`]
//! before delegating to [`mlua_batteries::task::register_with`].
//!
//! # Environment variables
//!
//! - `AGENT_BLOCK_TASK_DRIVER` — `async_fn` (default), `async`, or
//!   `coroutine`.  Selects the default driver used by `std.task.spawn`
//!   when the caller does not pass `opts.driver`.  Unparseable values
//!   silently fall back to `async_fn` (mirrors `AGENT_BLOCK_TASK_GRACE_MS`).
//! - `AGENT_BLOCK_TASK_GRACE_MS` — default grace window (cooperative
//!   cancel → hard abort) used by `std.task.with_timeout` when the caller
//!   does not pass `opts.grace_ms`.  Default: 1000 ms.  Set to 0 for
//!   strict / immediate-abort semantics.

use mlua::prelude::*;
use mlua_batteries::task::{Driver, TaskConfig};

/// Default grace period (ms) when neither the env var nor `opts.grace_ms`
/// is set.  Mirrors the historical agent-block default before extraction.
const DEFAULT_GRACE_MS: u64 = 1000;

fn parse_driver_env() -> Driver {
    match std::env::var("AGENT_BLOCK_TASK_DRIVER").ok().as_deref() {
        Some("coroutine") => Driver::Coroutine,
        _ => Driver::AsyncFn,
    }
}

fn parse_grace_ms_env() -> u64 {
    std::env::var("AGENT_BLOCK_TASK_GRACE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_GRACE_MS)
}

pub fn register(lua: &Lua) -> LuaResult<()> {
    let cfg = TaskConfig {
        default_driver: parse_driver_env(),
        grace_ms: parse_grace_ms_env(),
    };
    mlua_batteries::task::register_with(lua, cfg)
}
