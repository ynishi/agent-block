//! Lua-facing entry points (`std.task.*` callables).
//!
//! Every item registered into the `std.task` table by `super::register`
//! lives here.  Lower-level plumbing — cancellation, scope, spawn — lives
//! in sibling modules and is re-exported as `pub(super)` to keep the
//! surface narrow.
//!
//! The `with_timeout` teardown uses [`parse_timeout_opts`] to resolve the
//! grace window in the documented precedence order:
//! `opts.grace_ms` > `AGENT_BLOCK_TASK_GRACE_MS` > [`DEFAULT_GRACE_MS`].

use std::time::{Duration, Instant};

use mlua::prelude::*;
use mlua::{Function, MultiValue, Value};
use tracing::{debug, warn};

use super::cancel::{effective_token, race_sleep, race_yield, CancelToken};
use super::driver::{spawn_into, Handle};
use super::scope::{abort_all, current_scope, drain_scope, Scope, ScopeGuard, ScopeHandle};
use super::{ms_to_duration, LOCAL_SCOPE, TASK_INFO, TASK_TOKEN};

pub(super) fn spawn(lua: &Lua, (func, opts): (Function, Option<LuaTable>)) -> LuaResult<Handle> {
    let scope = current_scope(lua)?;
    spawn_into(lua, &scope, func, opts)
}

pub(super) async fn sleep(_: Lua, ms: f64) -> LuaResult<()> {
    let dur = ms_to_duration(ms, "std.task.sleep")?;
    race_sleep(dur).await
}

pub(super) async fn yield_now(_: Lua, _: ()) -> LuaResult<()> {
    race_yield().await
}

pub(super) async fn checkpoint(_lua: Lua, _: ()) -> LuaResult<()> {
    // Consult the effective cancel token (TASK_TOKEN in a spawned task,
    // LOCAL_SCOPE.token in a scope body) so checkpoint called from either
    // context observes cancellation.  Raise immediately if set, otherwise
    // yield and re-observe — mirrors Trio's level-triggered semantics.
    if let Some(t) = effective_token() {
        if t.is_cancelled() {
            return Err(LuaError::external("task cancelled"));
        }
    }
    race_yield().await
}

pub(super) fn cancel_token(_: &Lua, _: ()) -> LuaResult<CancelToken> {
    Ok(CancelToken::new())
}

/// `std.task.current()` — returns a table `{id, name, cancelled}` describing
/// the currently-executing spawned task, or `nil` if called from outside a
/// spawned task (e.g. at module top level or inside a `task.scope` body).
pub(super) fn current(lua: &Lua, _: ()) -> LuaResult<Value> {
    let info = TASK_INFO.try_with(|i| i.clone()).ok();
    match info {
        None => Ok(Value::Nil),
        Some(i) => {
            let t = lua.create_table()?;
            t.set("id", i.id)?;
            t.set("name", i.name)?;
            let cancelled = TASK_TOKEN.try_with(|t| t.is_cancelled()).unwrap_or(false);
            t.set("cancelled", cancelled)?;
            Ok(Value::Table(t))
        }
    }
}

/// `task.scope(fn)` or `task.scope(name, fn)` — structured nursery.
///
/// Cooperative-only contract: on error, the scope cancels its token and
/// drains all children; children that never reach a cancel checkpoint
/// (no `checkpoint`, no cancel-aware `sleep` / `yield`) block the drain
/// indefinitely.  This matches Trio / Swift TaskGroup / Kotlin
/// coroutineScope / tokio-util `TaskTracker`: `scope` has no wall-time
/// guarantee, so hard abort is out of scope — wrap with
/// `task.with_timeout` to bound teardown.
pub(super) async fn task_scope(_lua: Lua, args: MultiValue) -> LuaResult<Value> {
    let (name, func) = parse_scope_args(&args)?;
    let scope = Scope::new(name);
    let scope_for_body = scope.clone();
    let handle = ScopeHandle(scope.clone());

    LOCAL_SCOPE
        .scope(scope, async move {
            // Guard aborts children if the async body is dropped mid-await
            // (e.g. an outer timeout fires).  Normal completion paths drain
            // children explicitly and disarm the guard before it drops.
            let guard = ScopeGuard::new(scope_for_body.clone());

            let user_result: LuaResult<Value> = func.call_async::<Value>(handle).await;

            if user_result.is_err() {
                scope_for_body.borrow().token.cancel();
            }
            drain_scope(&scope_for_body).await;
            guard.disarm();

            user_result
        })
        .await
}

fn parse_scope_args(args: &MultiValue) -> LuaResult<(Option<String>, Function)> {
    let mut iter = args.iter();
    let first = iter
        .next()
        .ok_or_else(|| LuaError::external("task.scope requires at least a function"))?;
    match first {
        Value::Function(f) => Ok((None, f.clone())),
        Value::String(s) => {
            let n = s.to_str()?.to_string();
            let second = iter
                .next()
                .ok_or_else(|| LuaError::external("task.scope(name, fn) requires a function"))?;
            match second {
                Value::Function(f) => Ok((Some(n), f.clone())),
                _ => Err(LuaError::external(
                    "task.scope: second argument must be a function",
                )),
            }
        }
        _ => Err(LuaError::external(
            "task.scope: first argument must be a function or a name string",
        )),
    }
}

/// Default grace period between cooperative cancel and hard abort on the
/// `task.with_timeout` teardown path when neither `opts.grace_ms` nor
/// the `AGENT_BLOCK_TASK_GRACE_MS` env var is set.
///
/// 1 s is the middle ground between "long enough for local resource
/// cleanup" (DB flush / file fsync / HTTP connection release can each
/// take hundreds of ms) and "short enough not to mask real hangs".
/// Process-shutdown runtimes (Kubernetes / ASP.NET Core / Spring Boot)
/// default to 30 s, but that scale fits whole-process graceful
/// shutdown, not per-scope teardown.  Callers with heavier cleanup
/// should override via `opts.grace_ms` (per call) or
/// `AGENT_BLOCK_TASK_GRACE_MS` (VM-wide).  Set to 0 for strict /
/// immediate abort.
const DEFAULT_GRACE_MS: u64 = 1000;

/// Resolve the default grace period for `task.with_timeout` from the
/// `AGENT_BLOCK_TASK_GRACE_MS` env var, falling back to
/// [`DEFAULT_GRACE_MS`] when the var is unset or unparseable.
///
/// Unparseable / negative env values are silently ignored rather than
/// raising: a bad shell env should not cause every `with_timeout` in
/// the VM to fail at call time.  Mirrors `default_driver` in spirit.
fn env_default_grace() -> Duration {
    let ms = std::env::var("AGENT_BLOCK_TASK_GRACE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_GRACE_MS);
    Duration::from_millis(ms)
}

/// Parse the `opts` table accepted by `task.with_timeout(ms, fn, opts?)`.
///
/// Currently only `grace_ms` is recognised; unknown keys are rejected for
/// symmetry with `parse_opts` and to catch typos (e.g. `grace` vs
/// `grace_ms`) at call time.  Precedence for the returned value:
/// `opts.grace_ms` > `AGENT_BLOCK_TASK_GRACE_MS` > `DEFAULT_GRACE_MS`.
fn parse_timeout_opts(opts: Option<&LuaTable>) -> LuaResult<Duration> {
    let Some(t) = opts else {
        return Ok(env_default_grace());
    };

    let mut grace: Option<Duration> = None;
    for pair in t.clone().pairs::<LuaValue, LuaValue>() {
        let (k, v) = pair?;
        let key = match &k {
            LuaValue::String(s) => s.to_str()?.to_string(),
            other => {
                return Err(LuaError::external(format!(
                    "task.with_timeout: opts keys must be strings, got {}",
                    other.type_name()
                )));
            }
        };
        match key.as_str() {
            "grace_ms" => {
                let ms = match v {
                    LuaValue::Integer(i) => i as f64,
                    LuaValue::Number(n) => n,
                    other => {
                        return Err(LuaError::external(format!(
                            "task.with_timeout: opts.grace_ms must be a number, got {}",
                            other.type_name()
                        )));
                    }
                };
                grace = Some(ms_to_duration(ms, "task.with_timeout: opts.grace_ms")?);
            }
            other => {
                return Err(LuaError::external(format!(
                    "task.with_timeout: unknown opts key '{other}' (expected 'grace_ms')"
                )));
            }
        }
    }
    Ok(grace.unwrap_or_else(env_default_grace))
}

/// `task.with_timeout(ms, fn, opts?)` — scope with deadline.
///
/// On deadline trip or body error, the scope's cancel token is set so
/// cooperative children (those calling `checkpoint` / cancel-aware
/// `sleep` / `yield`) observe cancellation and unwind.  A **grace window**
/// (`opts.grace_ms`, default 1 s; overridable via `AGENT_BLOCK_TASK_GRACE_MS`)
/// bounds how long the drain waits for them.  Children still running when
/// the grace expires are hard-aborted via tokio `AbortHandle`, then drained
/// a final time.  `grace_ms = 0` gives strict-abort semantics (no cooperative
/// window).
///
/// 3-stage teardown (Kubernetes / ASP.NET Core / Spring Boot pattern):
///   1. `token.cancel()`            — cooperative signal
///   2. `drain_scope` under timeout(grace)
///   3. `abort_all` + final drain   — only remaining non-cooperative tasks
pub(super) async fn with_timeout(
    _lua: Lua,
    (ms, func, opts): (f64, Function, Option<LuaTable>),
) -> LuaResult<Value> {
    let dur = ms_to_duration(ms, "task.with_timeout")?;
    let grace = parse_timeout_opts(opts.as_ref())?;
    let scope = Scope::new(None);
    let scope_for_body = scope.clone();
    let handle = ScopeHandle(scope.clone());

    LOCAL_SCOPE
        .scope(scope, async move {
            let guard = ScopeGuard::new(scope_for_body.clone());

            let user_fut = func.call_async::<Value>(handle);
            let timed = tokio::time::timeout(dur, user_fut).await;

            let user_result: LuaResult<Value> = match timed {
                Ok(r) => r,
                Err(_) => {
                    debug!(
                        target: "agent_block::task",
                        timeout_ms = ms,
                        grace_ms = grace.as_millis() as u64,
                        "with_timeout: deadline exceeded, issuing cooperative cancel",
                    );
                    scope_for_body.borrow().token.cancel();
                    Err(LuaError::external(format!(
                        "task.with_timeout: exceeded {ms} ms"
                    )))
                }
            };

            if user_result.is_err() {
                // Stage 1: cooperative cancel (idempotent with the timeout path above).
                scope_for_body.borrow().token.cancel();
                // Stage 2: give cooperative children `grace` ms to unwind.
                let drain_start = Instant::now();
                let drained = tokio::time::timeout(grace, drain_scope(&scope_for_body)).await;
                // Stage 3: anything still running gets hard-aborted.
                if drained.is_err() {
                    let children_total = scope_for_body.borrow().children.len();
                    warn!(
                        target: "agent_block::task",
                        grace_ms = grace.as_millis() as u64,
                        elapsed_ms = drain_start.elapsed().as_millis() as u64,
                        children_total,
                        "with_timeout: grace expired, hard-aborting non-cooperative children",
                    );
                    abort_all(&scope_for_body);
                    drain_scope(&scope_for_body).await;
                } else {
                    debug!(
                        target: "agent_block::task",
                        elapsed_ms = drain_start.elapsed().as_millis() as u64,
                        "with_timeout: cooperative drain completed within grace",
                    );
                }
            } else {
                drain_scope(&scope_for_body).await;
            }
            guard.disarm();

            user_result
        })
        .await
}
