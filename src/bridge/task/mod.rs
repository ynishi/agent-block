//! `std.task` — structured async task primitives for Lua scripts.
//!
//! # API surface
//!
//! - `std.task.spawn(fn, opts?)` — fire-and-forget child, returns `Handle`
//! - `std.task.sleep(ms)` / `std.task.yield()` — cancel-aware suspension
//! - `std.task.checkpoint()` — bare cancel yield point
//! - `std.task.scope(name?, fn)` — structured nursery
//! - `std.task.with_timeout(ms, fn, opts?)` — scope with deadline
//! - `std.task.cancel_token()` — standalone `CancelToken`
//! - `std.task.current()` — `{id, name, cancelled}`
//! - `Scope:spawn`, `:cancel`, `:token`, `.name`
//! - `Handle:join`, `:abort`, `:is_finished`, `:elapsed`, `.id`, `.name`
//! - `CancelToken:cancel`, `:is_cancelled`, `:check`
//!
//! # Structured concurrency
//!
//! `task.scope(fn)` creates a `Scope`, installs it as the task-local
//! **current scope** (`LOCAL_SCOPE`) for the duration of `fn(scope)`, and
//! — regardless of how `fn` exits — waits for every task spawned into that
//! scope to finish before returning.  On error the scope's `CancelToken`
//! is set so cooperative children unwind; `scope` itself performs **no
//! hard abort** (matches Trio / Swift `TaskGroup` / Kotlin
//! `coroutineScope` / tokio-util `TaskTracker`).  A non-cooperative child
//! (never reaching a cancel checkpoint) therefore blocks the scope
//! indefinitely — the caller is expected to wrap with `task.with_timeout`
//! to bound teardown.  Top-level `task.spawn` attaches to the VM root
//! scope when no scope is installed.
//!
//! `LOCAL_SCOPE` is propagated via `tokio::task_local!` rather than a
//! shared VM-wide stack so a grandchild spawned with `task.spawn`
//! attaches to the correct ancestor scope even when concurrent siblings
//! are running their own `task.scope` bodies across `await` points.
//!
//! # Cancellation
//!
//! Cancellation is **cooperative + level-triggered** (Trio model): every
//! `std.task.*` suspension point (`sleep`, `yield`, `checkpoint`, and the
//! `coroutine` driver's `coroutine.yield`) consults the effective cancel
//! token (see `cancel::effective_token`) and raises `"task cancelled"`
//! when it fires.  `pcall`-swallowed cancellations reappear at the next
//! checkpoint, so cleanup code cannot accidentally suppress a cancel.
//!
//! `task.with_timeout(ms, fn, opts?)` layers a **3-stage graceful-abort**
//! pattern on top (Kubernetes / ASP.NET Core / Spring Boot):
//!   1. deadline trips → `token.cancel()`
//!   2. `drain_scope` runs under `timeout(grace_ms)` (default 1 s, overridable
//!      via `opts.grace_ms` or `AGENT_BLOCK_TASK_GRACE_MS`)
//!   3. any child still alive is hard-aborted via tokio `AbortHandle`
//!      and a final drain reaps it
//!
//! `grace_ms = 0` yields strict/immediate-abort semantics.  The scope's
//! RAII `ScopeGuard` also aborts children if the entire scope future
//! is dropped mid-await (outer timeout, VM teardown).
//!
//! # Error aggregation
//!
//! When a child raises, its error flows to the `Handle` returned by
//! `spawn` / `scope:spawn`, observed only through explicit `h:join()`.
//! A child whose handle is dropped without being joined is **silent** —
//! its error is logged but not propagated into the scope body.  This is
//! `Task.WhenAll`-style first-error semantics (Swift / .NET); we do not
//! synthesise Python-style `ExceptionGroup`s because Lua lacks an
//! idiomatic destructuring form for them.  If the scope body itself
//! raises, that error wins and is returned from `scope` unchanged.
//!
//! # Runtime contract
//!
//! Must run inside a `tokio::task::LocalSet` driven by a current-thread
//! runtime.  `mlua-isle::AsyncIsle` satisfies this.  All primitives are
//! `!Send`; tasks share an `Rc<RefCell<Scope>>` across task-locals.
//!
//! # Drivers
//!
//! - `async_fn` (default) — drives the user function via
//!   `Function::call_async`, so `sleep` / `yield` / `checkpoint` suspend
//!   through mlua's async bridge.
//! - `coroutine` (opt-in) — drives a raw Lua thread via `Thread::resume`
//!   in a loop; `coroutine.yield()` yields cooperatively and
//!   `coroutine.yield(ms)` sleeps (cancel-aware).  Selected via
//!   `opts.driver = "coroutine"` per-spawn or the
//!   `AGENT_BLOCK_TASK_DRIVER=coroutine` env var.
//!
//! Every task is wrapped in a `tracing::info_span!("task", id, name,
//! driver)` so downstream tool logs (sh / mesh / mcp / sql) carry task
//! context.  `std.task.current()` inside a spawned task returns
//! `{id, name, cancelled}` for Lua-side introspection.
//!
//! # Design decisions
//!
//! Condensed rationale for the non-obvious calls made above.  Kept here
//! (not in per-item docs) so a future maintainer can sanity-check the
//! whole design without chasing cross-references.
//!
//! - **`scope` is cooperative-only (no hard abort).**  `task.scope` issues
//!   `token.cancel()` + `drain_scope` and never calls `abort_all`.  This
//!   matches Trio / Swift `withThrowingTaskGroup` / Kotlin
//!   `coroutineScope` / Rust `moro` / `tokio-util::TaskTracker`.  A prior
//!   iteration added a defensive `abort_all` on the error path; it
//!   silenced cleanup omissions and was removed.  Hard abort is reserved
//!   for `with_timeout` where the grace window forms the user contract.
//!
//! - **3-stage graceful abort in `with_timeout`.**  Only place hard abort
//!   is justified.  Pattern follows Kubernetes `terminationGracePeriodSeconds`,
//!   ASP.NET Core `ShutdownTimeout`, Spring Boot `shutdown.grace-period`:
//!   cancel → drain under `timeout(grace_ms)` → abort + final drain.
//!   `grace_ms = 0` degenerates to immediate abort.
//!
//! - **Grace default 1 s, env-overridable.**  Local cleanup (DB flush,
//!   fsync, HTTP release) commonly runs in the low hundreds of ms; 1 s is
//!   the middle ground between covering that and not masking real hangs.
//!   Per-call via `opts.grace_ms`, VM-wide via `AGENT_BLOCK_TASK_GRACE_MS`.
//!   Unparseable env values fall back silently so a typo in a dev shell
//!   does not brick every `with_timeout` at call time (symmetry with
//!   `AGENT_BLOCK_TASK_DRIVER`).
//!
//! - **First-error / `Task.WhenAll` semantics for child errors.**  Errors
//!   flow only through `handle:join()`; dropped handles log via
//!   `tracing::error` but do not re-raise.  No Python-style
//!   `ExceptionGroup` synthesis — Lua has no idiomatic destructuring form
//!   (`except*`).  All-errors-observed semantics require the caller to
//!   join every handle.
//!
//! - **Level-triggered cancellation (Trio).**  Every suspension point
//!   consults `effective_token()` on wake, so `pcall`-swallowed
//!   cancellations reappear at the next checkpoint.  Users cannot
//!   accidentally suppress cancel by wrapping cleanup in `pcall`.
//!
//! - **Per-scope child cap: 32.**  `Scope::attach` amortises its finished-
//!   handle GC at 32 entries.  Higher fan-out should batch through an
//!   explicit worker pool; the hard cap at `scope:spawn` time forces that
//!   pattern rather than letting the child list grow unbounded.
//!
//! - **`task_local!` instead of a VM-wide scope stack.**  `LOCAL_SCOPE`
//!   propagates via `tokio::task_local!`, so a grandchild spawned with
//!   `task.spawn` inside a running task sees its **own** ancestor scope
//!   even when concurrent siblings are inside their own `task.scope`
//!   bodies across `await` points.  A shared `Vec` would interleave.
//!
//! - **Tracing: span name `"task"`, teardown event target
//!   `"agent_block::task"`.**  Every task runs inside
//!   `info_span!("task", id, name, driver)`; the `with_timeout` 3-stage
//!   events use `target: "agent_block::task"` so subscribers can filter
//!   them without matching every span attribute.
//!
//! # Module layout
//!
//! - `cancel` — `CancelToken` + `effective_token` + cancel-aware sleep/yield
//! - `scope`  — `Scope` + `ScopeGuard` + `drain_scope` / `abort_all` + `ScopeHandle`
//! - `driver` — `Driver` enum + `parse_opts` + `run_coroutine` + `Handle` + `spawn_into`
//! - `api`    — Lua-facing `std.task.*` callables (`spawn`, `scope`, `with_timeout`, …)

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use mlua::prelude::*;

mod api;
mod cancel;
mod driver;
mod scope;

use cancel::CancelToken;
use scope::Scope;

/// Lua-visible descriptor returned by `std.task.current()`.  Carried via
/// the `TASK_INFO` task-local rather than threaded through the Lua function
/// signature so any frame inside a spawned task can query it.
#[derive(Clone)]
struct TaskInfo {
    id: String,
    name: Option<String>,
}

tokio::task_local! {
    /// Set by `spawn_into` for the duration of a spawned task so that
    /// `task.checkpoint()` can consult the task's cancellation token
    /// without the caller threading it through manually.
    static TASK_TOKEN: CancelToken;
    /// Set by `spawn_into` for the duration of a spawned task so that
    /// `std.task.current()` can return id/name without threading them
    /// through the Lua function signature.
    static TASK_INFO: TaskInfo;
    /// The scope enclosing the currently-running task body.  Set by
    /// `task.scope` / `task.with_timeout` for their user function, and
    /// by `spawn_into` for each spawned child — so `task.spawn` always
    /// attaches to the correct scope without a shared VM-wide stack
    /// (which would interleave across concurrent tasks).
    static LOCAL_SCOPE: Rc<RefCell<Scope>>;
}

fn duration_to_ms(d: Duration) -> f64 {
    // f64 loses precision past ~2^53 ns (~104 days).  Acceptable because
    // `Handle::elapsed()` is short-lived observation of a live task, not a
    // persisted timestamp; any task whose elapsed time approaches that
    // range has already broken the single-thread LocalSet contract by
    // starving sibling tasks.
    (d.as_nanos() as f64) / 1_000_000.0
}

/// Convert a Lua `ms` argument into a `Duration`, rejecting non-finite
/// (NaN / ±∞), negative, and out-of-range values.  `ctx` is the caller
/// name used in the error message.
///
/// Upper bound: `ms * 1e6` must fit in a `u64` nanosecond count.  A float
/// `> u64::MAX / 1e6` (≈ 1.844e13 ms ≈ 584 years) would otherwise
/// *saturate* on the `as u64` cast (Rust-defined, no UB) and silently
/// become `u64::MAX` ns — a seemingly-infinite sleep.  Reject explicitly.
fn ms_to_duration(ms: f64, ctx: &str) -> LuaResult<Duration> {
    if !ms.is_finite() || ms < 0.0 {
        return Err(LuaError::external(format!(
            "{ctx}: invalid duration (ms={ms})"
        )));
    }
    const MAX_MS: f64 = u64::MAX as f64 / 1_000_000.0;
    if ms > MAX_MS {
        return Err(LuaError::external(format!(
            "{ctx}: duration out of range (ms={ms}, max≈{MAX_MS:.3e})"
        )));
    }
    Ok(Duration::from_nanos((ms * 1_000_000.0) as u64))
}

fn lua_to_string(v: &LuaValue, ctx: &str) -> LuaResult<String> {
    match v {
        LuaValue::String(s) => Ok(s.to_str()?.to_string()),
        other => Err(LuaError::external(format!(
            "{ctx}: expected string, got {}",
            other.type_name()
        ))),
    }
}

pub fn register(lua: &Lua) -> LuaResult<()> {
    // Install the root scope as app_data.  The root scope lives for the VM
    // lifetime and catches top-level `task.spawn` calls that are not inside
    // any `task.scope` body.  Its Drop triggers a last-resort abort on
    // outstanding fire-and-forget tasks during VM teardown.
    let root = Scope::new(Some("root".to_string()));
    lua.set_app_data::<Rc<RefCell<Scope>>>(root);

    let task = lua.create_table()?;
    task.set("spawn", lua.create_function(api::spawn)?)?;
    task.set("sleep", lua.create_async_function(api::sleep)?)?;
    task.set("yield", lua.create_async_function(api::yield_now)?)?;
    task.set("checkpoint", lua.create_async_function(api::checkpoint)?)?;
    task.set("cancel_token", lua.create_function(api::cancel_token)?)?;
    task.set("current", lua.create_function(api::current)?)?;
    task.set("scope", lua.create_async_function(api::task_scope)?)?;
    task.set(
        "with_timeout",
        lua.create_async_function(api::with_timeout)?,
    )?;

    let std_ns: LuaTable = lua.globals().get("std")?;
    std_ns.set("task", task)?;
    Ok(())
}
