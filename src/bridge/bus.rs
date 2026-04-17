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
//! `bus.on` / `bus.on_any` are registered as **sync** Lua functions (not
//! `create_async_function`), so they run inline on the Isle Lua thread.
//! A `std::sync::Mutex` can be locked briefly from sync context, and the
//! `bus.serve` async path also needs to lock it once (to `.take()` the
//! `EventBus`) — crucially, the lock is released **before** any `.await`,
//! avoiding the `await-holding-lock` anti-pattern.
//!
//! # wf-sim verdict doc comments
//!
//! The doc comments on `bus.on` and `bus.on_any` (below) encode the wf-sim
//! verdicts recorded in `workspace/tasks/event-bus/wf-sim-bus-on-*.md` and
//! `workspace/tasks/event-bus/wf-sim-bus-on-any-*.md`. Do not remove.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use mlua::prelude::*;
use mlua_isle::{AsyncIsle, IsleError};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::bridge::{json_to_lua, lua_to_json};
use crate::bus::{AckResult, Handler};
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
        let result_str = self
            .isle
            .coroutine_call(BUS_DISPATCH_FN, &args)
            .await
            .map_err(|e| {
                tracing::error!(%kind, %id, error = %e, "bus: Lua dispatch failed");
                match e {
                    IsleError::Cancelled => BlockError::Bus("handler cancelled".into()),
                    IsleError::Shutdown => BlockError::Bus("isle shut down".into()),
                    other => BlockError::Bus(format!("isle error: {other}")),
                }
            })?;

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

/// Register `bus.on` / `bus.on_any` / `bus.serve` on `lua`.
///
/// Must be called **before** `mesh::register` because the mesh bridge's
/// `mesh.on` alias reads the `bus` global produced here (`bridge/mod.rs`).
pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let bus_tbl = lua.create_table()?;

    // ── Lua-side dispatcher & storage ─────────────────────────────────
    lua.globals().set(BUS_HANDLERS_TBL, lua.create_table()?)?;
    lua.globals().set(BUS_ON_ANY_GLOBAL, LuaValue::Nil)?;
    install_lua_dispatcher(lua)?;

    // Shared ownership of the event bus. `Option::take` in `bus.serve`
    // moves the `EventBus` out of the mutex before any `.await`, so no
    // std-Mutex guard is held across an await point.
    let event_bus_for_on = Arc::clone(&ctx.event_bus);
    let event_bus_for_on_any = Arc::clone(&ctx.event_bus);
    let event_bus_for_serve = Arc::clone(&ctx.event_bus);

    let isle_for_on = Arc::clone(&ctx.isle);
    let isle_for_on_any = Arc::clone(&ctx.isle);

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
    bus_tbl.set(
        "on",
        lua.create_function(move |lua, (kind, func): (String, LuaFunction)| {
            // 1. Store the Lua function under __bus_handlers[kind] so the
            //    Lua-side dispatcher can find it.
            let tbl: LuaTable = lua.globals().get(BUS_HANDLERS_TBL)?;
            tbl.set(kind.as_str(), func)?;

            // 2. Register a (or replace the) LuaHandler on the EventBus.
            //    The EventBus dedupes by kind (last-write-wins), matching
            //    the Lua-side overwrite above.
            let handler: Arc<dyn Handler> = Arc::new(LuaHandler {
                isle: Arc::clone(&isle_for_on),
            });
            let mut guard = event_bus_for_on
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
    bus_tbl.set(
        "on_any",
        lua.create_function(move |lua, func: LuaFunction| {
            lua.globals().set(BUS_ON_ANY_GLOBAL, func)?;

            let handler: Arc<dyn Handler> = Arc::new(LuaHandler {
                isle: Arc::clone(&isle_for_on_any),
            });
            let mut guard = event_bus_for_on_any
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
                let mut bus = {
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

                // Drive the dispatcher loop. This is the long-lived await
                // point; the std Mutex is NOT held across it.
                let run_result = bus.run(shutdown.clone()).await;

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

/// Install the `__bus_dispatch(kind, id, payload_json, meta_json)` Lua
/// function.
///
/// The function:
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
fn install_lua_dispatcher(lua: &Lua) -> LuaResult<()> {
    let dispatch = lua.create_function(
        |lua, (kind, id, payload_json, meta_json): (String, String, String, String)| {
            // Resolve handler via __bus_handlers[kind], falling back to
            // __bus_on_any. Mirrors EventBus::dispatch on the Rust side.
            let handlers: LuaTable = lua.globals().get(BUS_HANDLERS_TBL)?;
            let handler: LuaValue = handlers.get(kind.as_str())?;
            let handler: LuaFunction = match handler {
                LuaValue::Function(f) => f,
                _ => {
                    let any: LuaValue = lua.globals().get(BUS_ON_ANY_GLOBAL)?;
                    match any {
                        LuaValue::Function(f) => f,
                        _ => {
                            // No handler at all — shouldn't happen because
                            // the Rust EventBus also checks, but return an
                            // explicit error so the ack surfaces the issue.
                            return Err(LuaError::external(format!(
                                "no Lua handler for kind `{kind}`"
                            )));
                        }
                    }
                }
            };

            // Decode payload / meta JSON strings into Lua values.
            let payload_val: Value = serde_json::from_str(&payload_json)
                .map_err(|e| LuaError::external(format!("payload decode: {e}")))?;
            let meta_val: Value = serde_json::from_str(&meta_json)
                .map_err(|e| LuaError::external(format!("meta decode: {e}")))?;
            let payload_lua = json_to_lua(lua, payload_val)?;
            let meta_lua = json_to_lua(lua, meta_val)?;

            // Build the event table the Lua handler sees.
            let ev_tbl = lua.create_table()?;
            ev_tbl.set("kind", kind.as_str())?;
            ev_tbl.set("id", id.as_str())?;
            ev_tbl.set("payload", payload_lua)?;
            ev_tbl.set("meta", meta_lua)?;

            // Invoke. The handler may yield (e.g. call `mesh.request`)
            // because __bus_dispatch is reached via `coroutine_call`, so
            // the entire call chain is already inside a Lua coroutine.
            let ret: LuaValue = handler.call(ev_tbl)?;

            // Convert the return value back to a JSON string. `LuaHandler`
            // on the Rust side parses it.
            let ret_json = lua_to_json(lua, ret)?;
            let s = serde_json::to_string(&ret_json)
                .map_err(|e| LuaError::external(format!("return encode: {e}")))?;
            Ok(s)
        },
    )?;
    lua.globals().set(BUS_DISPATCH_FN, dispatch)?;
    Ok(())
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
        lua.globals()
            .set(BUS_HANDLERS_TBL, lua.create_table().unwrap())
            .unwrap();
        lua.globals().set(BUS_ON_ANY_GLOBAL, LuaValue::Nil).unwrap();
        install_lua_dispatcher(&lua).unwrap();

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
        lua.globals()
            .set(BUS_HANDLERS_TBL, lua.create_table().unwrap())
            .unwrap();
        lua.globals().set(BUS_ON_ANY_GLOBAL, LuaValue::Nil).unwrap();
        install_lua_dispatcher(&lua).unwrap();

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
        lua.globals()
            .set(BUS_HANDLERS_TBL, lua.create_table().unwrap())
            .unwrap();
        lua.globals().set(BUS_ON_ANY_GLOBAL, LuaValue::Nil).unwrap();
        install_lua_dispatcher(&lua).unwrap();

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
        lua.globals()
            .set(BUS_HANDLERS_TBL, lua.create_table().unwrap())
            .unwrap();
        lua.globals().set(BUS_ON_ANY_GLOBAL, LuaValue::Nil).unwrap();
        install_lua_dispatcher(&lua).unwrap();

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
}
