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
//!   strict / immediate-abort semantics.  Parsing is delegated to
//!   [`crate::bridge::config::task_grace_ms`], which `tracing::warn!`s on
//!   unparseable values and falls back to the default.

use mlua::prelude::*;
use mlua_batteries::task::{Driver, TaskConfig};

use crate::bridge::config;

fn parse_driver_env() -> Driver {
    match std::env::var("AGENT_BLOCK_TASK_DRIVER").ok().as_deref() {
        Some("coroutine") => Driver::Coroutine,
        _ => Driver::AsyncFn,
    }
}

pub fn register(lua: &Lua) -> LuaResult<()> {
    let cfg = TaskConfig {
        default_driver: parse_driver_env(),
        grace_ms: config::task_grace_ms(),
    };
    mlua_batteries::task::register_with(lua, cfg)
}
