//! `bus.*` — EventBus Lua bridge.
//!
//! Exposes three Lua APIs:
//!
//! - `bus.on(kind, fn)`        — register a kind-specific handler.
//! - `bus.on_any(fn)`          — register a fallback handler for unmatched kinds.
//! - `bus.serve()`             — block until SIGTERM / Ctrl+C, driving the
//!   serial dispatcher inside the Isle thread.
//!
//! # Concurrency model (see `concurrency-analysis.md` §1)
//!
//! | Primitive                               | Where                          |
//! |-----------------------------------------|--------------------------------|
//! | `Arc<std::sync::Mutex<Option<EventBus>>>` | handler registration, serve take |
//! | `tokio::sync::mpsc::Sender<Event>`      | sources push events            |
//! | `tokio_util::sync::CancellationToken`   | shutdown fan-out               |
//! | `tokio::signal::unix::signal(SIGTERM)`  | POSIX signal install           |
//! | `tokio::signal::ctrl_c`                 | Ctrl+C race                    |
//! | `tokio::select!`                        | SIGTERM / Ctrl+C / bus.run     |
//! | `AtomicBool` (`serving`)                | single-serve guard             |
//!
//! # Why `std::sync::Mutex`?
//!
//! `bus.on` / `bus.on_any` are registered as `create_async_function`, but the
//! only lock acquisition they perform is a brief `std::sync::Mutex::lock()`
//! that is released (via `drop(guard)`) **before** any `.await`. The
//! `bus.serve` async path likewise locks the mutex only long enough to
//! `Option::take()` the `EventBus`, releasing the guard before the long
//! `run()` await. This avoids the `await-holding-lock` anti-pattern even
//! though the registration helpers are now async.
//!
//! # Handler Isle forwarding (Subtask 2)
//!
//! Lua handlers passed to `bus.on` / `bus.on_any` are serialized via
//! `Function::dump(true)` on the main Isle and reloaded on the dedicated
//! **handler Isle** via `Lua::load(bytes).set_mode(ChunkMode::Binary)`. The
//! function is then stored in `__bus_handlers[kind]` / `__bus_on_any` on the
//! handler Isle. The `LuaHandler` registered on the `EventBus` therefore
//! dispatches against `ctx.handler_isle`, leaving the main Isle's LocalSet
//! free to drive `bus.serve` grace timers / signal wake-ups.
//!
//! **Upvalue semantics**: bytecode transfer does not preserve upvalue
//! identity — the handler Isle reloads the chunk in its own Lua state, so
//! any upvalues captured by the handler closure are re-initialized to `nil`.
//! State shared between script init and event handlers must go through the
//! `kv.*`, `sql.*`, or `mesh.*` bridges (or any other bridge registered on
//! both Isles), not through Lua closure captures.
//!
//! **C functions are not dump-able**: `Function::dump` returns an empty
//! byte string for C functions / Rust-bound callbacks. `bus.on` detects
//! this via `Function::info().what` and returns a clear error rather than
//! silently installing a non-callable handler.
//!
//! # wf-sim verdict doc comments
//!
//! The doc comments on `bus.on` and `bus.on_any` (below) encode the wf-sim
//! verdicts. Do not remove.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mlua::prelude::*;
use mlua_isle::{AsyncIsle, CancelToken, IsleError};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::bus::{AckResult, EventBus, Handler};
use crate::error::BlockError;
use crate::host::HostContext;

/// Name of the Lua-side dispatcher function injected by [`register`].
///
/// Called by [`LuaHandler::call`] via
/// [`AsyncIsle::spawn_coroutine_call`] with
/// `(kind, id, payload_json, meta_json)` string arguments. It routes to the
/// Lua handler stored in `__bus_handlers[kind]` (or `__bus_on_any`) and
/// returns the handler's return value encoded as a JSON string.
const BUS_DISPATCH_FN: &str = "__bus_dispatch";

/// Lua table global holding kind-specific handlers (`kind` → `function`).
const BUS_HANDLERS_TBL: &str = "__bus_handlers";

/// Lua global holding the `on_any` fallback handler (function or nil).
const BUS_ON_ANY_GLOBAL: &str = "__bus_on_any";

/// Concrete [`Handler`] implementation that delegates to a Lua function via
/// the Isle thread's coroutine call path.
///
/// Each registration (`bus.on(kind, fn)` / `bus.on_any(fn)`) installs one
/// `LuaHandler` into the [`EventBus`]; the Lua function itself lives in a
/// Lua table (`__bus_handlers[kind]`) because function values are `!Send`
/// and can only be invoked from the Isle thread.
struct LuaHandler {
    isle: Arc<AsyncIsle>,
}

#[async_trait]
impl Handler for LuaHandler {
    async fn call(&self, kind: String, id: String, payload: Value, meta: Value) -> AckResult {
        // Encode payload and meta as JSON strings — AsyncIsle's coroutine
        // call channel only carries `&[&str]` arguments. The Lua-side
        // dispatcher re-decodes them with `std.json.decode` before invoking
        // the user handler.
        let payload_str = serde_json::to_string(&payload).map_err(|e| {
            tracing::error!(%kind, %id, error = %e, "bus: payload JSON encode failed");
            BlockError::Bus(format!("payload encode: {e}"))
        })?;
        let meta_str = serde_json::to_string(&meta).map_err(|e| {
            tracing::error!(%kind, %id, error = %e, "bus: meta JSON encode failed");
            BlockError::Bus(format!("meta encode: {e}"))
        })?;

        let args: [&str; 4] = [&kind, &id, &payload_str, &meta_str];
        // Spawn the coroutine call as an AsyncTask so we retain a cancel
        // handle. If this future is dropped (e.g. `run_with_grace` timed
        // out and is dropping the dispatcher chain), the Drop guard fires
        // the cancel token, which the Isle's debug hook picks up at the
        // next HOOK_INTERVAL. Without this guard the Isle thread would
        // run the Lua handler to completion — defeating the grace window.
        let task = self.isle.spawn_coroutine_call(BUS_DISPATCH_FN, &args);
        struct CancelOnDrop(CancelToken);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let guard = CancelOnDrop(task.cancel_token().clone());
        let result_str = task.await.map_err(|e| {
            tracing::error!(%kind, %id, error = %e, "bus: Lua dispatch failed");
            match e {
                IsleError::Cancelled => BlockError::Bus("handler cancelled".into()),
                IsleError::Shutdown => BlockError::Bus("isle shut down".into()),
                other => BlockError::Bus(format!("isle error: {other}")),
            }
        })?;
        // Normal completion: the coroutine already finished, so cancelling
        // is a no-op but sends a spurious signal to the next caller if the
        // CancelToken is later reused. Forget the guard to skip cancel.
        std::mem::forget(guard);

        // Empty string ≈ Lua nil (see `lua_value_to_string` in mlua-isle).
        if result_str.is_empty() {
            return Ok(Value::Null);
        }

        match serde_json::from_str::<Value>(&result_str) {
            Ok(v) => Ok(v),
            Err(e) => {
                // The Lua handler returned something that could not be
                // parsed as JSON. Wrap the raw string so the source still
                // receives a deterministic ack rather than silently drop.
                tracing::warn!(
                    %kind, %id, error = %e,
                    "bus: handler return value is not valid JSON; falling back to string"
                );
                Ok(Value::String(result_str))
            }
        }
    }
}

/// Register `bus.on` / `bus.on_any` / `bus.serve` on `lua` (the **main Isle**).
///
/// Must be called **before** `mesh::register` because the mesh bridge's
/// `mesh.on` alias reads the `bus` global produced here (`bridge/mod.rs`).
///
/// # Concurrency
///
/// Call this function on the main Isle Lua VM only. `bus.on` / `bus.on_any`
/// are registered via `create_async_function`; they serialize the caller's
/// Lua handler as bytecode (`Function::dump`) and forward it to the handler
/// Isle via `handler_isle.exec(...)` before registering a [`LuaHandler`] on
/// the [`EventBus`].
///
/// - The `std::sync::Mutex` guard on `event_bus` is dropped before the
///   `.await` on `handler_isle.exec` (the lock is acquired **after** the
///   exec resolves), so no lock is held across any `.await`.
/// - Cancel safety: if the `bus.on` future is dropped after `Function::dump`
///   but before `handler_isle.exec` resolves, the handler is never registered
///   on the handler Isle and never added to the [`EventBus`]. Because
///   `bus.on` may only be called **before** `bus.serve`, this leaves the
///   bus in a consistent (handler-absent) state.
/// - `bus.serve` takes ownership of the `EventBus` from the `Mutex` before
///   entering the async dispatch loop; no lock is held across any `.await`.
/// - `Arc<AsyncIsle>` (`handler_isle`) is `Send + Sync` and safe to clone
///   into async closures on the multi-thread runtime.
/// - Panic: returns `LuaError::external` on mutex poisoning or Isle forward
///   failure; never panics.
///
/// For the handler Isle, call [`install_bus_dispatcher_on_handler_isle`]
/// instead; it installs the `__bus_dispatch` / `__bus_handlers` /
/// `__bus_on_any` globals on the handler Isle and does **not** expose the
/// `bus.*` Lua table there.
pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let bus_tbl = lua.create_table()?;

    // NOTE: The `__bus_handlers` / `__bus_on_any` globals and the
    // `__bus_dispatch` function live on the **handler Isle** (installed by
    // `install_bus_dispatcher_on_handler_isle`). The main Isle only exposes
    // the `bus.*` Lua table (on / on_any / serve).

    // Shared ownership of the event bus. `Option::take` in `bus.serve`
    // moves the `EventBus` out of the mutex before any `.await`, so no
    // std-Mutex guard is held across an await point.
    let event_bus_for_on = Arc::clone(&ctx.event_bus);
    let event_bus_for_on_any = Arc::clone(&ctx.event_bus);
    let event_bus_for_serve = Arc::clone(&ctx.event_bus);

    let handler_isle_for_on = Arc::clone(&ctx.handler_isle);
    let handler_isle_for_on_any = Arc::clone(&ctx.handler_isle);

    // ── bus.on ────────────────────────────────────────────────────────
    // Register a handler for the given event `kind`.
    //
    // **Duplicate kind registration = last-write-wins (silent overwrite)**.
    // If `bus.on("foo", handler_a)` is called followed by `bus.on("foo",
    // handler_b)`, `handler_b` replaces `handler_a` without warning. Callers
    // should not rely on this silent behavior for intentional dynamic
    // rebinding; future revisions may emit a tracing::warn on overwrite.
    //
    // (doc verbatim from subtask-1.md §wf-sim Counter-WF)
    //
    // **Upvalue caveat** (Subtask 2): the handler closure is serialized with
    // `Function::dump(true)` on the main Isle and reloaded on the handler
    // Isle. Bytecode transfer does not preserve upvalue identity; upvalues
    // captured by the closure are re-initialized to `nil` on the handler
    // Isle. Share state via the `kv.*`, `sql.*`, `mesh.*`, or `std.*`
    // bridges instead of Lua closure captures.
    //
    // **C function rejection**: `Function::dump` returns an empty byte
    // string for C functions / Rust-bound callbacks (the Lua bytecode
    // encoder cannot serialize native code). `bus.on` detects this case
    // via `Function::info().what` and returns a clear error message rather
    // than silently installing a handler that would fail at dispatch time.
    bus_tbl.set(
        "on",
        lua.create_async_function(move |_, (kind, func): (String, LuaFunction)| {
            let handler_isle = Arc::clone(&handler_isle_for_on);
            let event_bus = Arc::clone(&event_bus_for_on);
            async move {
                // 1. Serialize the Lua handler as bytecode. The main Isle
                //    thread owns `func`; `dump` runs synchronously on that
                //    thread (we are still inside the create_async_function
                //    outer layer executed by the Isle).
                if func.info().what != "Lua" {
                    return Err(LuaError::external(
                        "bus.on: handler must be a pure Lua function (C functions and Rust-bound callbacks are not supported)",
                    ));
                }
                let bytecode = func.dump(true);
                if bytecode.is_empty() {
                    // Should be unreachable after the `what == "Lua"` check,
                    // but guard against edge cases (e.g. already-dumped
                    // closures with no code) with an explicit error rather
                    // than silently installing a dead handler.
                    return Err(LuaError::external(
                        "bus.on: Function::dump returned empty bytecode (handler not serializable)",
                    ));
                }

                // 2. Forward the bytecode to the handler Isle.
                let kind_for_exec = kind.clone();
                let bytecode_name = format!("@bus_handler[{kind_for_exec}]");
                handler_isle
                    .exec(move |lua| {
                        let loaded: LuaFunction = lua
                            .load(bytecode.as_slice())
                            .set_mode(mlua::ChunkMode::Binary)
                            .set_name(&bytecode_name)
                            .into_function()
                            .map_err(|e| IsleError::Lua(format!("bus.on load: {e}")))?;
                        let tbl: LuaTable = lua
                            .globals()
                            .get(BUS_HANDLERS_TBL)
                            .map_err(|e| IsleError::Lua(format!("bus.on handlers tbl: {e}")))?;
                        tbl.set(kind_for_exec.as_str(), loaded)
                            .map_err(|e| IsleError::Lua(format!("bus.on set: {e}")))?;
                        Ok(String::new())
                    })
                    .await
                    .map_err(|e| {
                        tracing::error!(%kind, error = %e, "bus.on: handler isle load failed");
                        LuaError::external(format!("bus.on: handler isle load failed: {e}"))
                    })?;

                // 3. Register (or replace) the LuaHandler on the EventBus.
                //    The EventBus dedupes by kind (last-write-wins), matching
                //    the handler Isle-side overwrite above.
                let handler: Arc<dyn Handler> = Arc::new(LuaHandler {
                    isle: Arc::clone(&handler_isle),
                });
                let mut guard = event_bus
                    .lock()
                    .map_err(|_| LuaError::external("bus mutex poisoned"))?;
                match guard.as_mut() {
                    Some(bus) => bus
                        .on(kind.clone(), handler)
                        .map_err(|e| LuaError::external(format!("bus.on: {e}")))?,
                    None => {
                        return Err(LuaError::external(
                            "bus.on: bus.serve() has already taken ownership; register handlers before calling bus.serve()",
                        ));
                    }
                }
                drop(guard);
                Ok(())
            }
        })?,
    )?;

    // ── bus.on_any ────────────────────────────────────────────────────
    // Register a fallback handler for events whose `kind` has no specialized
    // handler.
    //
    // Invoked only when no `bus.on(kind)` handler matches the event's `kind`.
    // This is an **unmatched-event fallback**, NOT a fan-out/tap (that is a
    // separate follow-up API).
    //
    // Observability fan-out (invoke on every event regardless of specialized
    // handlers) is out of scope for this task and tracked as a follow-up
    // (`bus.tap` or equivalent).
    //
    // (doc verbatim from subtask-1.md §wf-sim R3)
    //
    // Same bytecode-transfer / upvalue / C-function caveats as `bus.on`.
    bus_tbl.set(
        "on_any",
        lua.create_async_function(move |_, func: LuaFunction| {
            let handler_isle = Arc::clone(&handler_isle_for_on_any);
            let event_bus = Arc::clone(&event_bus_for_on_any);
            async move {
                if func.info().what != "Lua" {
                    return Err(LuaError::external(
                        "bus.on_any: handler must be a pure Lua function (C functions and Rust-bound callbacks are not supported)",
                    ));
                }
                let bytecode = func.dump(true);
                if bytecode.is_empty() {
                    return Err(LuaError::external(
                        "bus.on_any: Function::dump returned empty bytecode (handler not serializable)",
                    ));
                }

                let bytecode_name = "@bus_handler[__on_any]".to_string();
                handler_isle
                    .exec(move |lua| {
                        let loaded: LuaFunction = lua
                            .load(bytecode.as_slice())
                            .set_mode(mlua::ChunkMode::Binary)
                            .set_name(&bytecode_name)
                            .into_function()
                            .map_err(|e| IsleError::Lua(format!("bus.on_any load: {e}")))?;
                        lua.globals()
                            .set(BUS_ON_ANY_GLOBAL, loaded)
                            .map_err(|e| IsleError::Lua(format!("bus.on_any set: {e}")))?;
                        Ok(String::new())
                    })
                    .await
                    .map_err(|e| {
                        tracing::error!(error = %e, "bus.on_any: handler isle load failed");
                        LuaError::external(format!("bus.on_any: handler isle load failed: {e}"))
                    })?;

                let handler: Arc<dyn Handler> = Arc::new(LuaHandler {
                    isle: Arc::clone(&handler_isle),
                });
                let mut guard = event_bus
                    .lock()
                    .map_err(|_| LuaError::external("bus mutex poisoned"))?;
                match guard.as_mut() {
                    Some(bus) => bus
                        .on_any(handler)
                        .map_err(|e| LuaError::external(format!("bus.on_any: {e}")))?,
                    None => {
                        return Err(LuaError::external(
                            "bus.on_any: bus.serve() has already taken ownership; register handlers before calling bus.serve()",
                        ));
                    }
                }
                drop(guard);
                Ok(())
            }
        })?,
    )?;

    // ── bus.serve ─────────────────────────────────────────────────────
    // Atomic guard rejects a second call to bus.serve() from the Lua script.
    let serving = Arc::new(AtomicBool::new(false));

    bus_tbl.set(
        "serve",
        lua.create_async_function(move |_, ()| {
            let event_bus = Arc::clone(&event_bus_for_serve);
            let serving = Arc::clone(&serving);
            async move {
                // Single-serve guard (first check, then take).
                if serving.swap(true, Ordering::SeqCst) {
                    return Err(LuaError::external("bus.serve: already running"));
                }

                // Take the EventBus out of the mutex BEFORE any await.
                let bus = {
                    let mut guard = event_bus
                        .lock()
                        .map_err(|_| LuaError::external("bus mutex poisoned"))?;
                    match guard.take() {
                        Some(b) => b,
                        None => {
                            // Someone else already took it (shouldn't happen
                            // given the AtomicBool guard above, but we roll
                            // back the guard to keep invariants tight).
                            serving.store(false, Ordering::SeqCst);
                            return Err(LuaError::external("bus.serve: bus already consumed"));
                        }
                    }
                    // guard drops here, before we await anything
                };

                let shutdown = CancellationToken::new();

                // Spawn the signal race task. It cancels `shutdown` when
                // SIGTERM or Ctrl+C arrives. If SIGTERM cannot be installed
                // (e.g. on a platform without unix signals) we fall back
                // to Ctrl+C only.
                let signal_task = spawn_signal_task(shutdown.clone());

                // Grace window: once `shutdown` has been cancelled, the
                // dispatcher loop is expected to exit promptly after the
                // current in-flight handler finishes. We cap this wait at
                // `AGENT_BLOCK_TASK_GRACE_MS` so a misbehaving handler can
                // never block process exit indefinitely.
                let grace_ms = crate::bridge::config::task_grace_ms();
                let run_result =
                    run_with_grace(bus, shutdown.clone(), Duration::from_millis(grace_ms)).await;

                // Best-effort cleanup of the signal task. If `shutdown`
                // was cancelled via the signal branch, the task has
                // already exited; otherwise we abort it.
                signal_task.abort();

                if let Err(e) = run_result {
                    tracing::error!(error = %e, "bus.serve: dispatcher loop returned error");
                    return Err(LuaError::external(format!("bus.serve: {e}")));
                }
                tracing::info!("bus.serve: dispatcher loop exited cleanly");
                Ok(())
            }
        })?,
    )?;

    lua.globals().set("bus", bus_tbl)?;
    Ok(())
}

/// Install the `__bus_handlers` table, `__bus_on_any` fallback slot, and
/// the `__bus_dispatch(kind, id, payload_json, meta_json)` Lua dispatcher on
/// the **handler Isle**.
///
/// After Subtask 2 these globals no longer live on the main Isle; the main
/// Isle exposes only the `bus.*` Lua table (see [`register`]). Callers must
/// invoke this function from inside the handler Isle's bridge registration
/// (`bridge::register_all_handler_side`).
///
/// The dispatcher function:
/// 1. Looks up the user-registered handler (`__bus_handlers[kind]` first,
///    then `__bus_on_any`).
/// 2. Decodes `payload_json` and `meta_json` into Lua tables.
/// 3. Calls the handler with `(event_table)` where `event_table` contains
///    `kind`, `id`, `payload`, `meta`.
/// 4. JSON-encodes the return value and returns it as a string.
///
/// Errors are propagated as Lua errors — the Isle converts them into
/// [`IsleError::Lua`], which [`LuaHandler::call`] wraps into
/// [`BlockError::Bus`] so the dispatcher can deliver a NACK.
///
/// # Concurrency
///
/// Runs synchronously on the handler Isle's Lua thread (called from inside
/// an `AsyncIsle::exec` closure, which owns exclusive access to the Lua VM).
pub(crate) fn install_bus_dispatcher_on_handler_isle(lua: &Lua) -> LuaResult<()> {
    lua.globals().set(BUS_HANDLERS_TBL, lua.create_table()?)?;
    lua.globals().set(BUS_ON_ANY_GLOBAL, LuaValue::Nil)?;

    // __bus_dispatch must be a pure-Lua function (not a Rust C function).
    // The Isle invokes it via `coroutine_call`, which wraps it in a Lua
    // thread. The user handler inside is expected to be able to yield
    // (e.g. await `sh.exec`, `http.get`, `mesh.request`). If __bus_dispatch
    // were a `create_function` C closure, the yield inside the handler
    // would cross the C-call boundary and Lua would raise
    // "attempt to yield across a C-call boundary" immediately, making
    // every async bridge unusable from bus handlers. Writing the
    // dispatcher in Lua removes that boundary: yields from the user
    // handler propagate up through pure Lua frames into the enclosing
    // coroutine managed by the Isle.
    //
    // JSON encode/decode relies on `std.json` from `mlua-batteries`,
    // registered in `build_isle_init` (host.rs) on both the main Isle
    // and the handler Isle.
    let src = r#"
        local BUS_HANDLERS_TBL = "__bus_handlers"
        local BUS_ON_ANY_GLOBAL = "__bus_on_any"
        return function(kind, id, payload_json, meta_json)
            local handlers = _G[BUS_HANDLERS_TBL]
            local h = handlers and handlers[kind]
            if type(h) ~= "function" then
                h = _G[BUS_ON_ANY_GLOBAL]
            end
            if type(h) ~= "function" then
                error("no Lua handler for kind `" .. tostring(kind) .. "`")
            end
            local ok_payload, payload = pcall(std.json.decode, payload_json)
            if not ok_payload then
                error("payload decode: " .. tostring(payload))
            end
            local ok_meta, meta = pcall(std.json.decode, meta_json)
            if not ok_meta then
                error("meta decode: " .. tostring(meta))
            end
            local ev = {
                kind = kind,
                id = id,
                payload = payload,
                meta = meta,
            }
            local ret = h(ev)
            if ret == nil then
                return ""
            end
            return std.json.encode(ret)
        end
    "#;
    let dispatch: LuaFunction = lua
        .load(src)
        .set_name("@agent_block:__bus_dispatch")
        .eval()?;
    lua.globals().set(BUS_DISPATCH_FN, dispatch)?;
    Ok(())
}

/// Drive the dispatcher loop with a bounded grace window.
///
/// Under normal operation `bus.run(shutdown)` returns as soon as `shutdown`
/// is cancelled (after the current in-flight handler finishes, see
/// `bus::dispatcher`). `run_with_grace` adds a hard cap on how long we wait
/// after the cancel signal: once `shutdown.cancelled()` fires, the
/// dispatcher has at most `grace` to finish. A misbehaving handler that
/// refuses to yield cannot block process exit indefinitely.
///
/// `grace` comes from `AGENT_BLOCK_TASK_GRACE_MS` (default 1000ms, see
/// `bridge::config::task_grace_ms`).
async fn run_with_grace(
    mut bus: EventBus,
    shutdown: CancellationToken,
    grace: Duration,
) -> Result<(), BlockError> {
    let run_fut = bus.run(shutdown.clone());
    tokio::pin!(run_fut);
    tokio::select! {
        res = &mut run_fut => res,
        _ = shutdown.cancelled() => {
            // Shutdown fired first; the dispatcher is either about to
            // break out of `select!` in `bus.run`, or it is still inside
            // a handler's await. Bound the remaining wait by `grace`.
            match tokio::time::timeout(grace, &mut run_fut).await {
                Ok(res) => res,
                Err(_) => {
                    tracing::warn!(
                        grace_ms = grace.as_millis() as u64,
                        "bus.serve: grace window exceeded; forcing exit"
                    );
                    Ok(())
                }
            }
        }
    }
}

/// Spawn a task that cancels `shutdown` on SIGTERM or Ctrl+C.
///
/// Returns the [`tokio::task::JoinHandle`] so `bus.serve` can abort it once
/// the dispatcher loop has exited.
fn spawn_signal_task(shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // On non-unix platforms `tokio::signal::unix` is unavailable; we
        // gate on `cfg(unix)` so the crate still builds elsewhere.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let term = match signal(SignalKind::terminate()) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::error!(error = %e, "bus.serve: SIGTERM install failed; Ctrl+C only");
                    None
                }
            };
            match term {
                Some(mut term) => {
                    tokio::select! {
                        _ = term.recv() => tracing::info!("bus.serve: SIGTERM received"),
                        sig = tokio::signal::ctrl_c() => match sig {
                            Ok(()) => tracing::info!("bus.serve: Ctrl+C received"),
                            Err(e) => tracing::error!(error = %e, "bus.serve: ctrl_c error"),
                        },
                    }
                }
                None => {
                    if let Err(e) = tokio::signal::ctrl_c().await {
                        tracing::error!(error = %e, "bus.serve: ctrl_c error");
                    } else {
                        tracing::info!("bus.serve: Ctrl+C received");
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            if let Err(e) = tokio::signal::ctrl_c().await {
                tracing::error!(error = %e, "bus.serve: ctrl_c error");
            } else {
                tracing::info!("bus.serve: Ctrl+C received");
            }
        }
        shutdown.cancel();
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `__bus_dispatch` resolves a kind-specific handler, decodes JSON, and
    /// JSON-encodes the return value. This is a pure Lua-side test; the
    /// Rust `LuaHandler` is exercised indirectly via a production-like
    /// integration in ST4.
    #[test]
    fn dispatcher_resolves_kind_and_encodes_return() {
        let lua = Lua::new();
        mlua_batteries::register_all(&lua, "std").unwrap();
        install_bus_dispatcher_on_handler_isle(&lua).unwrap();

        // Register a handler that echoes the payload with a field added.
        lua.load(
            r#"
            __bus_handlers["mesh"] = function(ev)
                return { echoed = ev.payload.value, id = ev.id }
            end
        "#,
        )
        .exec()
        .unwrap();

        let dispatch: LuaFunction = lua.globals().get(BUS_DISPATCH_FN).unwrap();
        let payload = serde_json::to_string(&json!({"value": 42})).unwrap();
        let meta = serde_json::to_string(&json!({"from": "peer"})).unwrap();
        let out: String = dispatch
            .call(("mesh", "evt-1", payload.as_str(), meta.as_str()))
            .unwrap();

        let got: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(got, json!({"echoed": 42, "id": "evt-1"}));
    }

    #[test]
    fn dispatcher_falls_back_to_on_any() {
        let lua = Lua::new();
        mlua_batteries::register_all(&lua, "std").unwrap();
        install_bus_dispatcher_on_handler_isle(&lua).unwrap();

        lua.load(
            r#"
            __bus_on_any = function(ev)
                return { from_any = ev.kind }
            end
        "#,
        )
        .exec()
        .unwrap();

        let dispatch: LuaFunction = lua.globals().get(BUS_DISPATCH_FN).unwrap();
        let out: String = dispatch.call(("custom", "e1", "{}", "{}")).unwrap();
        let got: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(got, json!({"from_any": "custom"}));
    }

    #[test]
    fn dispatcher_errors_when_no_handler_registered() {
        let lua = Lua::new();
        mlua_batteries::register_all(&lua, "std").unwrap();
        install_bus_dispatcher_on_handler_isle(&lua).unwrap();

        let dispatch: LuaFunction = lua.globals().get(BUS_DISPATCH_FN).unwrap();
        let err = dispatch
            .call::<String>(("nope", "e1", "{}", "{}"))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no Lua handler for kind `nope`"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn dispatcher_reports_invalid_payload_json() {
        let lua = Lua::new();
        mlua_batteries::register_all(&lua, "std").unwrap();
        install_bus_dispatcher_on_handler_isle(&lua).unwrap();

        lua.load(r#"__bus_handlers["x"] = function() return nil end"#)
            .exec()
            .unwrap();

        let dispatch: LuaFunction = lua.globals().get(BUS_DISPATCH_FN).unwrap();
        let err = dispatch
            .call::<String>(("x", "e1", "not-json", "{}"))
            .unwrap_err();
        assert!(
            err.to_string().contains("payload decode"),
            "unexpected error: {err}"
        );
    }

    /// Round-trip: `Function::dump(true)` on one Lua VM, `Lua::load` with
    /// `ChunkMode::Binary` on a second Lua VM, then invoke through the
    /// `__bus_dispatch` path. Exercises the bytecode transfer mechanism
    /// that `bus.on` uses to forward handlers from the main Isle to the
    /// handler Isle (without requiring a real `AsyncIsle` spawn).
    #[test]
    fn bytecode_round_trip_to_second_lua_vm_dispatches() {
        // Source VM: compile a Lua handler and dump to bytecode.
        let src = Lua::new();
        let func: LuaFunction = src
            .load(
                r#"
                return function(ev)
                    return { got = ev.payload.value, kind = ev.kind }
                end
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(func.info().what, "Lua");
        let bytecode = func.dump(true);
        assert!(!bytecode.is_empty(), "Lua function dump must be non-empty");

        // Destination VM: stand in for the handler Isle. Load the bytecode
        // and register it under __bus_handlers[kind].
        let dst = Lua::new();
        mlua_batteries::register_all(&dst, "std").unwrap();
        install_bus_dispatcher_on_handler_isle(&dst).unwrap();
        let loaded: LuaFunction = dst
            .load(bytecode.as_slice())
            .set_mode(mlua::ChunkMode::Binary)
            .set_name("@bus_handler[mesh]")
            .into_function()
            .unwrap();
        let handlers: LuaTable = dst.globals().get(BUS_HANDLERS_TBL).unwrap();
        handlers.set("mesh", loaded).unwrap();

        // Dispatch and verify the reconstructed closure ran.
        let dispatch: LuaFunction = dst.globals().get(BUS_DISPATCH_FN).unwrap();
        let payload = serde_json::to_string(&json!({"value": 7})).unwrap();
        let out: String = dispatch
            .call(("mesh", "evt-rt", payload.as_str(), "{}"))
            .unwrap();
        let got: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(got, json!({"got": 7, "kind": "mesh"}));
    }

    /// `Function::info().what` distinguishes Lua-defined closures (dumpable)
    /// from Rust-bound C functions (not dumpable). `bus.on` relies on this
    /// discriminator to reject handlers that would otherwise produce an
    /// empty bytecode blob and fail at dispatch time.
    #[test]
    fn c_function_is_detected_via_info_what() {
        let lua = Lua::new();
        let rust_fn: LuaFunction = lua.create_function(|_, ()| Ok(())).unwrap();
        assert_ne!(
            rust_fn.info().what,
            "Lua",
            "Rust-bound callbacks should not report info().what == \"Lua\""
        );

        let lua_fn: LuaFunction = lua.load("return function() end").eval().unwrap();
        assert_eq!(lua_fn.info().what, "Lua");
    }
}
