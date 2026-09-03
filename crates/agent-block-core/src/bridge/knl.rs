//! `knl.*` — Lua surface of the kernel syscall layer.
//!
//! This module is an adapter, nothing more.  The domain rules live in
//! [`crate::knl`] (pure Rust, unit-tested without a VM); here we only:
//!
//! 1. define the `Session` userdata and bind its methods,
//! 2. convert Lua tables ⇄ `serde_json::Value`,
//! 3. attribute failures as `knl: <method>: <reason>`.
//!
//! Keeping the conversion in one place is what makes the re-entrancy
//! discipline checkable: walking a Lua table can call back into Lua, so
//! every conversion happens *outside* an active borrow of the session,
//! and the kernel core never sees a Lua value at all.
//!
//! The shell reaches kernel state only through the methods of the
//! userdata returned by `knl.session(opts?)`, so the invariants are
//! enforced by the shape of the API rather than by convention:
//!
//! - **I1 append-only.**  There is deliberately no `update`, `delete` or
//!   `replace`.  `events()` / `view()` hand back freshly built tables, so
//!   a caller that mutates a returned value cannot reach recorded state.
//!   `seq`, `epoch_ms` and `author` are assigned by the kernel and
//!   overwrite any caller-supplied field of the same name.
//! - **Author.**  Every event carries `author = "kernel"` or `"caller"`,
//!   stamped from the path it took.  `append` takes any kind, the ones
//!   the kernel writes included — a `model_response` from an earlier
//!   conversation belongs in the dialogue and is appended like anything
//!   else — while `view("usage")`, the budget charge and the turn
//!   numbering read the kernel's events only.  So the one way to add to
//!   what this run is accounted for is to make the call that pays for it.
//! - **I3 budget monotonicity.**  `spend(n)` accepts non-negative whole
//!   amounts only and the balance can only decrease (floored at `0`).
//!   There is no API to raise or reset it.
//! - **I6 run scope.**  All state lives inside the userdata — no
//!   module-level statics, no Lua globals — so two sessions are fully
//!   independent.  `knl.session()` records `run_started` and
//!   `close(reason?)` records `run_finished`, after which `append` /
//!   `spend` are errors while reads keep working.
//! - **K2 model call.**  `s:call(req, meta?)` runs the bound backend and
//!   returns only after the response is in the history and its tokens are
//!   charged.  There is no way to make the model call without the record:
//!   the two are one operation.
//!
//! ```lua
//! local s = knl.session({
//!     budget  = { tokens = 10000 },
//!     backend = function(req) ... end,   -- optional, see `s:call`
//! })
//! s:append({ kind = "msg_user", content = "hi" })
//! local mine = s:has_backend()                  -- who makes the call
//! local out, err = s:call({ messages = ... })   -- records + charges
//! local dialogue = s:view("dialogue")   -- provider-neutral rows
//! local usage    = s:view("usage")      -- token totals
//! s:close("done")
//! ```
//!
//! Scope: in-memory only.  Effect execution, the Lua-side projection seam
//! and persistence are separate steps of the kernel/shell base design.

use std::cell::RefCell;

use mlua::prelude::*;
use serde_json::{Map, Value};

use super::{json_to_lua, lua_to_json};
use crate::knl;

/// A backend as the session keeps it between calls.
enum Bound {
    /// A Lua closure.  Only the marker is here: the function itself is
    /// the session userdata's *user value*, which is the one place a
    /// closure can live for exactly the session's lifetime while staying
    /// visible to Lua's collector.
    ///
    /// The alternative — a registry key held in this struct — keeps the
    /// closure alive just as reliably and is invisible to the GC, so a
    /// backend that captures its own session (`local s; s = knl.session {
    /// backend = function() s:append(...) end }`, which the tests and
    /// `tool_loop` both do) forms a cycle the collector cannot see
    /// through and neither half is ever freed.  As a user value the same
    /// cycle is an ordinary Lua one: unreachable, and collected.
    Lua,
    /// The name of a built-in (Rust) backend.  The slot is reserved so
    /// the interface does not have to change when the first one lands;
    /// today no name resolves, and `call` says so.
    Builtin(String),
}

/// A backend resolved for one call.
enum Callable {
    /// The closure to run.
    Lua(LuaFunction),
    /// A built-in name, still unresolvable.
    Builtin(String),
}

/// K5 run scope: the only handle the Lua side has on kernel state.
struct Session {
    // `RefCell` (not `Mutex`): an Isle drives a single Lua VM on one
    // thread, and every borrow below is released before control returns
    // to Lua, so no borrow can overlap another.
    state: RefCell<knl::Session>,
    /// Which backend `call` uses when a call does not bring its own — a
    /// built-in name, or the marker saying the closure is in the user
    /// value.  Not inside the `RefCell`: it is fixed at open, and the
    /// call sequence reads it while the backend is running.
    backend: Option<Bound>,
}

impl Session {
    /// Open a run with an optional token budget and backend.
    fn new(budget_tokens: Option<i64>, backend: Option<Bound>) -> Self {
        Self {
            state: RefCell::new(knl::Session::new(budget_tokens)),
            backend,
        }
    }
}

/// The `knl: <method>: <reason>` attribution, as text.
///
/// `call` reports its failures as a returned string rather than as a
/// raise, so the attribution is built here and wrapped only when the
/// method in question raises.
fn attributed(method: &str, reason: impl std::fmt::Display) -> String {
    format!("knl: {method}: {reason}")
}

/// Build a `knl:`-attributed error for `method`.
fn err(method: &str, reason: impl std::fmt::Display) -> LuaError {
    LuaError::external(attributed(method, reason))
}

/// Interpret a Lua value as a whole number (any sign), or `None`.
fn as_whole(value: &LuaValue) -> Option<i64> {
    match value {
        LuaValue::Integer(i) => Some(*i),
        LuaValue::Number(n) => (n.is_finite() && n.fract() == 0.0).then_some(*n as i64),
        _ => None,
    }
}

/// Interpret a Lua value as a non-negative whole number, or `None`.
fn as_whole_non_negative(value: &LuaValue) -> Option<i64> {
    as_whole(value).filter(|n| *n >= 0)
}

/// Render a Lua value for an error message (value when numeric, else
/// type).
fn lua_value_for_msg(value: &LuaValue) -> String {
    match value {
        LuaValue::Integer(i) => i.to_string(),
        LuaValue::Number(n) => n.to_string(),
        other => other.type_name().to_string(),
    }
}

/// Convert the Lua table `noun` into a JSON object for `method`.
///
/// Runs before any borrow of the session: the walk may re-enter Lua,
/// which must not observe a held borrow.
fn table_to_object(
    lua: &Lua,
    method: &str,
    noun: &str,
    value: LuaValue,
) -> LuaResult<Map<String, Value>> {
    if !matches!(value, LuaValue::Table(_)) {
        return Err(err(
            method,
            format!("{noun} must be a table, got {}", value.type_name()),
        ));
    }
    match lua_to_json(lua, value).map_err(|e| err(method, e))? {
        Value::Object(obj) => Ok(obj),
        _ => Err(err(
            method,
            format!("{noun} must be a table with string keys"),
        )),
    }
}

/// Read a `backend` slot: a Lua function, a built-in name, or nothing.
///
/// `at` names the slot in the message (`backend` when a session is
/// opened, `meta.backend` for one call), so a caller is told which of the
/// two it got wrong.
fn parse_backend(value: LuaValue, at: &str) -> Result<Option<Callable>, String> {
    match value {
        LuaValue::Nil => Ok(None),
        LuaValue::Function(backend) => Ok(Some(Callable::Lua(backend))),
        LuaValue::String(name) => match name.to_str() {
            Ok(name) => Ok(Some(Callable::Builtin(name.to_string()))),
            Err(e) => Err(format!("{at} must be valid UTF-8: {e}")),
        },
        other => Err(format!(
            "{at} must be a function or a string, got {}",
            other.type_name()
        )),
    }
}

/// The per-call override, if the caller passed one.
///
/// `meta` carries the override and nothing else in v1 — in particular not
/// a turn number, which the kernel owns.
fn parse_meta_backend(meta: LuaValue) -> Result<Option<Callable>, String> {
    match meta {
        LuaValue::Nil => Ok(None),
        LuaValue::Table(meta) => {
            let backend: LuaValue = meta.get("backend").map_err(|e| e.to_string())?;
            parse_backend(backend, "meta.backend")
        }
        other => Err(format!("meta must be a table, got {}", other.type_name())),
    }
}

/// The message a backend gave when it returned `nil`.
fn backend_error(err: &LuaValue) -> String {
    match err {
        LuaValue::String(message) => message.to_string_lossy(),
        LuaValue::Nil => "returned nil without a message".to_string(),
        other => format!(
            "returned nil and a {} in place of a message",
            other.type_name()
        ),
    }
}

/// Step [6]: `{ turn, content, usage, stop_reason, remaining, exhausted }`.
///
/// Built from the checked result rather than from the table the backend
/// returned, so a caller that mutates what it gets back reaches neither
/// the backend's value nor the history.
///
/// `content` is the array the model answered with, empty included — an
/// empty one crosses back as an array rather than as an empty mapping
/// because the JSON bridge tags it.  `stop_reason` is absent when the
/// provider named none, which is the same thing the record says.
fn out_table(
    lua: &Lua,
    result: &knl::ModelResult,
    outcome: &knl::CallOutcome,
) -> LuaResult<LuaValue> {
    let out = lua.create_table()?;
    out.set("turn", outcome.turn)?;
    out.set(
        "content",
        json_to_lua(lua, Value::Array(result.content().to_vec()))?,
    )?;
    out.set(
        "usage",
        json_to_lua(lua, Value::Object(result.usage().clone()))?,
    )?;
    if let Some(stop_reason) = result.stop_reason() {
        out.set("stop_reason", stop_reason)?;
    }
    // Absent rather than zero when the run has no budget: there is no
    // balance to report, which is not the same as having none left.
    if let Some(remaining) = outcome.remaining {
        out.set("remaining", remaining)?;
    }
    out.set("exhausted", outcome.exhausted)?;
    Ok(LuaValue::Table(out))
}

/// Note a call that produced no result and hand back its reason.
///
/// Every way a call can fail once the backend was reachable comes through
/// here — the transport, and a result the kernel refuses to record — so
/// the history says a call was attempted in each of them.  Best effort by
/// design: a closed run cannot take the note, and turning that into a
/// second failure would replace the one actually worth reporting.  A
/// userdata that will not borrow is the same case: step [1] borrowed it
/// already and nothing takes it mutably, so it does not happen, and if it
/// did the failure being reported is still the one to keep.
fn failed(this: &LuaAnyUserData, reason: &str) -> String {
    if let Ok(session) = this.borrow::<Session>() {
        session.state.borrow_mut().record_model_call_failure(reason);
    }
    reason.to_string()
}

/// The kernel sequence of `s:call` — steps [1]–[6] of the design.
///
/// Returns the `out` table, or the reason for a `nil, err` return.
/// Nothing in here raises: a caller then has one failure shape to handle,
/// and a loop that already treats every failure as a result does not have
/// to wrap its own syscall in a `pcall`.
///
/// Takes the userdata rather than a borrow of what is inside it, because
/// step [1] reads the bound closure out of its user value (see
/// [`Bound::Lua`]).  Each borrow below is taken and released inside one
/// step: none is held while the backend runs or while a Lua value is
/// walked, both of which re-enter Lua.
fn call(
    lua: &Lua,
    this: &LuaAnyUserData,
    req: LuaValue,
    meta: LuaValue,
) -> Result<LuaValue, String> {
    // [1] Resolved first, so a call with nowhere to go is not reported as
    // one that failed at the provider.
    let backend = match resolve(this, parse_meta_backend(meta)?)? {
        Callable::Lua(backend) => backend,
        Callable::Builtin(name) => return Err(format!("unknown builtin backend: {name}")),
    };

    // Not one of the six steps, and deliberately ahead of [2]: a closed
    // run can record nothing, so running the backend would spend a real
    // model call to produce a response the kernel must then drop — the one
    // thing `call` exists to rule out.  What the caller sees is what the
    // closed case gives anyway (`nil, err`, nothing written), reached
    // without the call.
    let closed = this
        .borrow::<Session>()
        .map_err(|e| e.to_string())?
        .state
        .borrow()
        .is_closed();
    if closed {
        return Err("session is closed".to_string());
    }

    // [2] Run with no borrow held: the backend may re-enter the session
    // (`s:append`), and what it records lands ahead of the response, which
    // is where it happened.
    let value = match backend.call::<(LuaValue, LuaValue)>(req) {
        Ok((LuaValue::Nil, err)) => Err(backend_error(&err)),
        Ok((value, _)) => Ok(value),
        // A raise and a `nil, err` are one event to the kernel: no result
        // came back.
        Err(raised) => Err(raised.to_string()),
    };
    let value = match value {
        Ok(value) => value,
        Err(reason) => return Err(failed(this, &format!("backend: {reason}"))),
    };

    // [3] Converted outside any borrow — walking a Lua table can call back
    // into Lua — and checked before a single write.  A result the kernel
    // cannot record changes neither the history's account of the run nor
    // the budget: no `model_response`, no charge, and the turn stays
    // available for the call that gets it right.  What it does leave is a
    // `model_call_failed`, the same note a transport failure leaves — the
    // model was asked and this run has nothing to show for it either way,
    // and the two differ only in the reason the note carries.
    let value = match lua_to_json(lua, value) {
        Ok(value) => value,
        Err(e) => {
            return Err(failed(
                this,
                &format!("backend result is not representable: {e}"),
            ))
        }
    };
    let result = match knl::validate_backend_result(&value) {
        Ok(result) => result,
        Err(e) => return Err(failed(this, &e.to_string())),
    };

    // [4][5] The only mutable borrow of the sequence, taken after the
    // shell is done and released before the return value is built.
    let outcome = this
        .borrow::<Session>()
        .map_err(|e| e.to_string())?
        .state
        .borrow_mut()
        .record_model_response(&result)
        .map_err(|e| e.to_string())?;

    // [6] The record is already in place, so a failure to build the return
    // value says so rather than implying nothing happened.
    out_table(lua, &result, &outcome).map_err(|e| {
        format!(
            "turn {} was recorded and charged, but its result could not be built: {e}",
            outcome.turn
        )
    })
}

/// Step [1]: the backend for this call — the one it brought, else the
/// bound one.  An override applies to this call only; the binding is
/// never replaced.
fn resolve(this: &LuaAnyUserData, over: Option<Callable>) -> Result<Callable, String> {
    if let Some(over) = over {
        return Ok(over);
    }
    // The name is copied out and the borrow released before the user value
    // is read: that read goes through Lua, which must not find a borrow of
    // the userdata held across it.
    let builtin = {
        let session = this.borrow::<Session>().map_err(|e| e.to_string())?;
        match &session.backend {
            None => return Err("no backend bound".to_string()),
            Some(Bound::Lua) => None,
            Some(Bound::Builtin(name)) => Some(name.clone()),
        }
    };
    match builtin {
        Some(name) => Ok(Callable::Builtin(name)),
        None => this
            .user_value::<LuaFunction>()
            .map(Callable::Lua)
            .map_err(|e| format!("bound backend is unreachable: {e}")),
    }
}

impl LuaUserData for Session {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        // s:id() -> string
        methods.add_method("id", |_, this, ()| Ok(this.state.borrow().id().to_string()));

        // s:append(event) -> seq
        //
        // K1: the only way to add to the history, and there is no way to
        // change what is already in it.
        methods.add_method("append", |lua, this, event: LuaValue| {
            let obj = table_to_object(lua, "append", "event", event)?;
            this.state
                .borrow_mut()
                .append(obj)
                .map_err(|e| err("append", e))
        });

        // s:call(req, meta?) -> out | nil, err
        //
        // K2: the model call and its record are one operation.  `req` is
        // handed to the backend untouched — what goes on the wire is the
        // shell's policy — and what comes back is recorded and charged
        // before this returns, so there is no arrangement of the caller's
        // code in which a call happened and the history does not say so.
        //
        // Failures come back as `nil, err` rather than as a raise; `out`
        // carries the kernel-stamped turn plus the budget state after the
        // charge.
        //
        // A function rather than a method: the sequence needs the userdata
        // itself to reach the bound backend in its user value, which a
        // `&Session` cannot get to.  Called as `s:call(...)`, so the first
        // argument is the session either way.
        methods.add_function(
            "call",
            |lua, (this, req, meta): (LuaAnyUserData, LuaValue, LuaValue)| match call(
                lua, &this, req, meta,
            ) {
                Ok(out) => Ok((out, LuaValue::Nil)),
                Err(reason) => {
                    let message = lua.create_string(attributed("call", reason))?;
                    Ok((LuaValue::Nil, LuaValue::String(message)))
                }
            },
        );

        // s:has_backend() -> boolean
        //
        // Whether the session was opened with a backend of its own — the
        // one thing a caller with a backend to lend has to know before it
        // calls, because it decides whether to pass one.  Asked rather
        // than discovered by making a call and reading the failure: that
        // works, but it ties the caller to the wording of an error
        // message, and it answers only after a call has already been
        // attempted.
        //
        // A per-call override says nothing about the binding, so this
        // does not move: it reports what `knl.session(opts)` was given.
        methods.add_method("has_backend", |_, this, ()| Ok(this.backend.is_some()));

        // s:events(from?) -> array of event tables (deep copy)
        //
        // K1: the returned tables are freshly built from the stored JSON
        // on every call, so mutating them cannot reach kernel state.
        methods.add_method("events", |lua, this, from: Option<u64>| {
            let selected = this.state.borrow().events(from.unwrap_or(0));
            // The borrow is released above: json_to_lua re-enters Lua.
            json_to_lua(lua, Value::Array(selected))
        });

        // s:len() -> number of recorded events
        methods.add_method("len", |_, this, ()| Ok(this.state.borrow().len() as u64));

        // s:view(name, opts?) -> projection (fresh table each call)
        //
        // Named folds only; an unknown name is an error.
        methods.add_method("view", |lua, this, (name, opts): (LuaValue, LuaValue)| {
            let LuaValue::String(name) = name else {
                return Err(err(
                    "view",
                    format!("name must be a string, got {}", name.type_name()),
                ));
            };
            let name = name.to_str()?.to_string();
            let opts = match opts {
                LuaValue::Nil => None,
                table @ LuaValue::Table(_) => Some(table_to_object(lua, "view", "opts", table)?),
                other => {
                    return Err(err(
                        "view",
                        format!("opts must be a table, got {}", other.type_name()),
                    ));
                }
            };
            let value = this
                .state
                .borrow_mut()
                .view(&name, opts.as_ref())
                .map_err(|e| err("view", e))?;
            // The borrow is released above: json_to_lua re-enters Lua.
            json_to_lua(lua, value)
        });

        // s:spend(n) -> remaining (nil when the session has no budget)
        //
        // K4: non-negative amounts only, and the balance never rises.
        methods.add_method("spend", |_, this, amount: LuaValue| {
            let Some(amount) = as_whole(&amount) else {
                return Err(err(
                    "spend",
                    format!(
                        "amount must be a non-negative whole number, got {}",
                        lua_value_for_msg(&amount)
                    ),
                ));
            };
            let remaining = this
                .state
                .borrow_mut()
                .spend(amount)
                .map_err(|e| err("spend", e))?;
            Ok(match remaining {
                Some(remaining) => LuaValue::Integer(remaining),
                None => LuaValue::Nil,
            })
        });

        // s:remaining() -> number or nil (no budget)
        methods.add_method("remaining", |_, this, ()| {
            Ok(this.state.borrow().remaining())
        });

        // s:exhausted() -> boolean (always false without a budget)
        methods.add_method("exhausted", |_, this, ()| {
            Ok(this.state.borrow().exhausted())
        });

        // s:close(reason?) — records `run_finished` and ends the run
        // scope.  Idempotent.
        methods.add_method("close", |_, this, reason: LuaValue| {
            let reason = match reason {
                LuaValue::Nil => None,
                LuaValue::String(s) => Some(s.to_str()?.to_string()),
                other => {
                    return Err(err(
                        "close",
                        format!("reason must be a string, got {}", other.type_name()),
                    ));
                }
            };
            this.state.borrow_mut().close(reason.as_deref());
            Ok(())
        });
    }
}

/// Read `opts.budget.tokens` into the initial balance.
fn parse_budget(opts: Option<&LuaTable>) -> LuaResult<Option<i64>> {
    let Some(opts) = opts else {
        return Ok(None);
    };
    let budget: LuaValue = opts.get("budget")?;
    let budget = match budget {
        LuaValue::Nil => return Ok(None),
        LuaValue::Table(t) => t,
        other => {
            return Err(err(
                "session",
                format!("budget must be a table, got {}", other.type_name()),
            ));
        }
    };
    let tokens: LuaValue = budget.get("tokens")?;
    if matches!(tokens, LuaValue::Nil) {
        return Err(err(
            "session",
            "budget.tokens is required (non-negative whole number)",
        ));
    }
    as_whole_non_negative(&tokens).map(Some).ok_or_else(|| {
        err(
            "session",
            format!(
                "budget.tokens must be a non-negative whole number, got {}",
                lua_value_for_msg(&tokens)
            ),
        )
    })
}

/// Read `opts.backend`: what the session will remember between calls.
///
/// The session has to be the thing that remembers it, because `s:call(req)`
/// names no backend.  Where a closure is kept is [`register`]'s business —
/// it goes in the userdata's user value, which only exists once the
/// userdata does.
fn parse_session_backend(opts: Option<&LuaTable>) -> LuaResult<Option<Callable>> {
    let Some(opts) = opts else {
        return Ok(None);
    };
    let value: LuaValue = opts.get("backend")?;
    parse_backend(value, "backend").map_err(|e| err("session", e))
}

/// Register the `knl` global.  No [`crate::host::HostContext`] is needed —
/// this layer keeps all state inside the session userdata.
pub fn register(lua: &Lua) -> LuaResult<()> {
    let knl_tbl = lua.create_table()?;

    // knl.session(opts?) -> Session userdata
    knl_tbl.set(
        "session",
        lua.create_function(|lua, opts: LuaValue| {
            let opts = match opts {
                LuaValue::Nil => None,
                LuaValue::Table(t) => Some(t),
                other => {
                    return Err(err(
                        "session",
                        format!("opts must be a table, got {}", other.type_name()),
                    ));
                }
            };
            let budget_tokens = parse_budget(opts.as_ref())?;
            let backend = parse_session_backend(opts.as_ref())?;
            let bound = match &backend {
                None => None,
                Some(Callable::Lua(_)) => Some(Bound::Lua),
                Some(Callable::Builtin(name)) => Some(Bound::Builtin(name.clone())),
            };

            // The closure is parked in the userdata's user value rather
            // than in the registry, so that a backend which captures this
            // very session is a cycle Lua can collect (see `Bound::Lua`).
            let session = lua.create_userdata(Session::new(budget_tokens, bound))?;
            if let Some(Callable::Lua(closure)) = backend {
                session.set_user_value(closure)?;
            }
            Ok(session)
        })?,
    )?;

    lua.globals().set("knl", knl_tbl)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures for the `s:call` cases: a conforming backend result, and
    /// a backend that hands back queued results while counting its runs.
    ///
    /// A queued entry that is a function is called with the request
    /// instead, which is how a case makes the backend fail, or look at
    /// the session while it is running.
    const CALL_FIXTURES: &str = r#"
        function result(text, usage)
            return {
                content = { { type = "text", text = text or "ok" } },
                usage = usage or { input_tokens = 10, output_tokens = 3 },
                stop_reason = "end_turn",
            }
        end

        calls = 0

        function backend(...)
            local queue = { ... }
            return function(req)
                calls = calls + 1
                local next_result = table.remove(queue, 1)
                assert(next_result ~= nil, "the backend ran more often than the case queued")
                if type(next_result) == "function" then
                    return next_result(req)
                end
                return next_result
            end
        end

        -- The recorded kinds in order, as one comparable string.
        function kinds_of(s)
            local names = {}
            for _, e in ipairs(s:events()) do
                table.insert(names, e.kind)
            end
            return table.concat(names, ",")
        end

        -- The `turn` of every recorded response, in order.
        function turns_of(s)
            local turns = {}
            for _, e in ipairs(s:events()) do
                if e.kind == "model_response" then
                    table.insert(turns, e.turn)
                end
            end
            return table.concat(turns, ",")
        end

        -- The first event of `kind`, or nil.
        function first_of(s, kind)
            for _, e in ipairs(s:events()) do
                if e.kind == kind then
                    return e
                end
            end
        end
    "#;

    /// Fresh Lua VM with only the `knl` bridge registered.
    fn vm() -> Lua {
        let lua = Lua::new();
        register(&lua).expect("register knl");
        lua.load(CALL_FIXTURES).exec().expect("call fixtures");
        lua
    }

    /// Run a chunk that is expected to fail, returning the error message.
    fn expect_err(lua: &Lua, chunk: &str) -> String {
        lua.load(chunk)
            .exec()
            .expect_err("chunk was expected to fail")
            .to_string()
    }

    /// (Happy path) append assigns strictly increasing seq numbers, `len`
    /// tracks them, and `events()` exposes the caller fields plus the
    /// kernel-owned `seq` / `epoch_ms`.  Seq 1 is the kernel's own
    /// `run_started`.
    #[test]
    fn append_assigns_monotonic_seq_and_len_tracks() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            assert(s:len() == 1, "a fresh session holds run_started")
            local a = s:append({ kind = "user_msg", text = "hi" })
            local b = s:append({ kind = "note", name = "sh" })
            assert(a == 2, "first caller seq: " .. tostring(a))
            assert(b == 3, "second caller seq: " .. tostring(b))
            assert(s:len() == 3, "len: " .. tostring(s:len()))

            local evs = s:events()
            assert(#evs == 3, "events len: " .. tostring(#evs))
            assert(evs[1].kind == "run_started")
            assert(evs[2].kind == "user_msg")
            assert(evs[2].text == "hi")
            assert(evs[2].seq == 2)
            assert(type(evs[2].epoch_ms) == "number", "epoch_ms must be a number")
            assert(evs[3].kind == "note")
            assert(evs[3].seq == 3)
        "#,
        )
        .exec()
        .expect("happy path chunk");
    }

    /// (I1) No mutation API is reachable on the session userdata.
    #[test]
    fn session_exposes_no_mutation_api() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            s:append({ kind = "user_msg" })
            for _, name in ipairs({ "update", "delete", "replace", "set", "insert",
                                    "remove", "clear", "truncate", "pop" }) do
                local ok, v = pcall(function() return s[name] end)
                assert(not ok or v == nil, "mutation API must not exist: " .. name)
            end
        "#,
        )
        .exec()
        .expect("mutation-surface chunk");
    }

    /// (I1) The table returned by `events()` is a deep copy: mutating it,
    /// including nested tables and the array itself, leaves the history
    /// untouched.
    #[test]
    fn events_returns_a_deep_copy() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            s:append({ kind = "user_msg", text = "hi", meta = { tag = "a" } })

            local evs = s:events()
            evs[2].kind = "TAMPERED"
            evs[2].text = nil
            evs[2].extra = "injected"
            evs[2].meta.tag = "b"
            table.insert(evs, { kind = "ghost" })

            local again = s:events()
            assert(#again == 2, "history length changed: " .. tostring(#again))
            assert(again[2].kind == "user_msg", "kind changed: " .. tostring(again[2].kind))
            assert(again[2].text == "hi", "text changed")
            assert(again[2].extra == nil, "field injected into history")
            assert(again[2].meta.tag == "a", "nested table changed")
        "#,
        )
        .exec()
        .expect("deep copy chunk");
    }

    /// (I1) `seq` / `epoch_ms` / `author` are kernel-owned: a
    /// caller-supplied value is overwritten rather than trusted.
    #[test]
    fn kernel_owned_fields_override_caller_values() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            local seq = s:append({ kind = "user_msg", seq = 999, epoch_ms = 1, author = "kernel" })
            assert(seq == 2, "returned seq: " .. tostring(seq))
            local e = s:events(2)[1]
            assert(e.seq == 2, "stored seq: " .. tostring(e.seq))
            assert(e.epoch_ms ~= 1, "epoch_ms must be kernel-assigned")
            assert(e.author == "caller", "author: " .. tostring(e.author))
        "#,
        )
        .exec()
        .expect("kernel-owned field chunk");
    }

    /// `events(from)` returns the tail with `seq >= from`.
    #[test]
    fn events_from_filters_by_seq() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            for i = 1, 3 do s:append({ kind = "e" .. i }) end
            local tail = s:events(3)
            assert(#tail == 2, "tail len: " .. tostring(#tail))
            assert(tail[1].seq == 3 and tail[2].seq == 4)
            assert(#s:events(5) == 0, "past-the-end filter must be empty")
            assert(#s:events(0) == 4, "from=0 must return everything")
        "#,
        )
        .exec()
        .expect("events(from) chunk");
    }

    /// (attribution) `append` rejects a missing / non-string `kind` and a
    /// non-table event, with `knl: append:` in the message.
    #[test]
    fn append_validates_event_shape_with_attributed_errors() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.session():append({ text = "no kind" })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("kind is required"), "{msg}");

        let msg = expect_err(&lua, r#"knl.session():append({ kind = 42 })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("kind must be a string"), "{msg}");

        let msg = expect_err(&lua, r#"knl.session():append("not a table")"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("event must be a table"), "{msg}");

        // A rejected append leaves no trace in the history.
        lua.load(
            r#"
            local s = knl.session()
            pcall(function() s:append({ text = "no kind" }) end)
            assert(s:len() == 1, "rejected append was recorded")
            assert(s:append({ kind = "ok" }) == 2, "seq must not be consumed by a failure")
        "#,
        )
        .exec()
        .expect("rejected-append chunk");
    }

    /// (I3) A negative `spend` is an error, attributed to `knl: spend:`,
    /// and leaves the balance untouched.
    #[test]
    fn spend_rejects_negative_amounts() {
        let lua = vm();
        let msg = expect_err(
            &lua,
            r#"
            local s = knl.session({ budget = { tokens = 100 } })
            s:spend(-1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("non-negative"), "{msg}");

        lua.load(
            r#"
            local s = knl.session({ budget = { tokens = 100 } })
            pcall(function() s:spend(-1) end)
            assert(s:remaining() == 100, "balance changed: " .. tostring(s:remaining()))
            -- A negative spend is rejected even without a budget.
            local ok = pcall(function() knl.session():spend(-1) end)
            assert(not ok, "negative spend must be rejected without a budget too")
            -- So is a non-numeric amount.
            local ok2 = pcall(function() knl.session():spend("many") end)
            assert(not ok2, "a non-numeric amount must be rejected")
        "#,
        )
        .exec()
        .expect("negative-spend chunk");
    }

    /// (I3) `remaining` is non-increasing across a call sequence, is
    /// floored at zero, and `exhausted()` flips once the budget is used
    /// up.
    #[test]
    fn spend_is_monotonic_and_flips_exhausted() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({ budget = { tokens = 1000 } })
            assert(s:remaining() == 1000)
            assert(s:exhausted() == false)

            local prev = s:remaining()
            for _, n in ipairs({ 120, 0, 300, 80 }) do
                local r = s:spend(n)
                assert(r == s:remaining(), "spend must return the new balance")
                assert(r <= prev, "remaining rose: " .. tostring(prev) .. " -> " .. tostring(r))
                prev = r
            end
            assert(s:remaining() == 500, "remaining: " .. tostring(s:remaining()))
            assert(s:exhausted() == false)

            -- Overspending floors at zero and never goes negative.
            local r = s:spend(9999)
            assert(r == 0, "floor: " .. tostring(r))
            assert(s:remaining() == 0)
            assert(s:exhausted() == true, "exhausted must flip after overspending")
            assert(s:spend(1) == 0, "spending past zero stays at zero")
        "#,
        )
        .exec()
        .expect("budget monotonicity chunk");
    }

    /// (I3) Without a budget, `spend` returns nil, `remaining()` is nil
    /// and the session is never exhausted.
    #[test]
    fn session_without_budget_reports_nil() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            assert(s:remaining() == nil, "remaining must be nil without a budget")
            assert(s:spend(50) == nil, "spend must return nil without a budget")
            assert(s:exhausted() == false, "no budget can never be exhausted")

            -- An empty opts table behaves the same way.
            local s2 = knl.session({})
            assert(s2:remaining() == nil)
        "#,
        )
        .exec()
        .expect("no-budget chunk");
    }

    /// (attribution) Malformed `budget` options are rejected by
    /// `knl.session` itself.
    #[test]
    fn session_validates_budget_options() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.session({ budget = { tokens = -1 } })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("budget.tokens"), "{msg}");

        let msg = expect_err(&lua, r#"knl.session({ budget = {} })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("required"), "{msg}");

        let msg = expect_err(&lua, r#"knl.session({ budget = 100 })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("budget must be a table"), "{msg}");

        let msg = expect_err(&lua, r#"knl.session("nope")"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("opts must be a table"), "{msg}");
    }

    /// (I6) Two sessions share nothing: ids differ and history / budget
    /// of one is invisible to the other.
    #[test]
    fn two_sessions_are_independent() {
        let lua = vm();
        lua.load(
            r#"
            local a = knl.session({ budget = { tokens = 100 } })
            local b = knl.session({ budget = { tokens = 100 } })

            assert(type(a:id()) == "string" and #a:id() > 0, "id must be a non-empty string")
            assert(a:id() ~= b:id(), "session ids must be unique")

            a:append({ kind = "only_in_a" })
            a:spend(60)

            assert(a:len() == 2 and b:len() == 1, "history leaked between sessions")
            assert(#b:events(2) == 0, "b holds only its own run_started")
            assert(a:remaining() == 40 and b:remaining() == 100, "budget leaked between sessions")

            -- Closing one leaves the other usable.
            a:close()
            assert(b:append({ kind = "still_open" }) == 2)
        "#,
        )
        .exec()
        .expect("session independence chunk");
    }

    /// (I6) After `close()`, `append` and `spend` are errors; read-only
    /// methods keep working and `close()` is idempotent.
    #[test]
    fn closed_session_rejects_append_and_spend() {
        let lua = vm();

        let msg = expect_err(
            &lua,
            r#"
            local s = knl.session()
            s:close()
            s:append({ kind = "after_close" })
        "#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        let msg = expect_err(
            &lua,
            r#"
            local s = knl.session({ budget = { tokens = 10 } })
            s:close()
            s:spend(1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        lua.load(
            r#"
            local s = knl.session({ budget = { tokens = 10 } })
            s:append({ kind = "before_close" })
            s:spend(4)
            s:close()
            s:close() -- idempotent

            -- Reads still work after the run scope ends.
            assert(s:len() == 3, "run_started + before_close + run_finished")
            assert(s:events()[2].kind == "before_close")
            assert(s:remaining() == 6)
            assert(s:exhausted() == false)
            assert(type(s:id()) == "string")
        "#,
        )
        .exec()
        .expect("closed-session read chunk");
    }

    /// (I6) The bridge installs exactly one global and keeps no state
    /// there: a second VM starts with its own fresh session.
    #[test]
    fn state_lives_in_the_userdata_not_in_globals() {
        let lua_a = vm();
        lua_a
            .load(
                r#"
            local s = knl.session()
            s:append({ kind = "in_vm_a" })
            assert(s:len() == 2)
            -- `knl` itself carries no session state.
            assert(knl.events == nil and knl.append == nil and knl.spend == nil)
        "#,
            )
            .exec()
            .expect("vm a chunk");

        let lua_b = vm();
        lua_b
            .load(
                r#"
            local s = knl.session()
            assert(s:len() == 1, "a second VM starts with only its own run_started")
            assert(s:events()[1].kind == "run_started")
        "#,
            )
            .exec()
            .expect("vm b chunk");
    }

    /// The kernel brackets the run: `run_started` on open, `run_finished`
    /// on close, with the caller's reason (or the default).
    #[test]
    fn run_boundaries_are_recorded_by_the_kernel() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            local opened = s:events()[1]
            assert(opened.kind == "run_started", "kind: " .. tostring(opened.kind))
            assert(opened.seq == 1)

            s:close("budget_exhausted")
            local evs = s:events()
            assert(#evs == 2, "close must record run_finished")
            assert(evs[2].kind == "run_finished")
            assert(evs[2].reason == "budget_exhausted")

            s:close("ignored")
            assert(s:len() == 2, "close is idempotent")

            -- Without a reason the kernel records its default.
            local d = knl.session()
            d:close()
            assert(d:events()[2].reason == "closed", "default reason")
        "#,
        )
        .exec()
        .expect("run boundary chunk");

        let msg = expect_err(&lua, r#"knl.session():close({ not_a = "string" })"#);
        assert!(msg.contains("knl: close:"), "missing attribution: {msg}");
        assert!(msg.contains("reason must be a string"), "{msg}");
    }

    /// The reserved kinds the shell writes are checked by the kernel; the
    /// required fields are named in an attributed error and nothing is
    /// recorded.
    #[test]
    fn reserved_kinds_are_validated_with_attributed_errors() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.session():append({ kind = "msg_user" })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("msg_user"), "{msg}");
        assert!(msg.contains("content"), "{msg}");

        let msg = expect_err(
            &lua,
            r#"knl.session():append({ kind = "tool_result", turn = 1, call_id = "c", ok = "yes", result = 1 })"#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("a boolean"), "{msg}");

        let msg = expect_err(
            &lua,
            r#"knl.session():append({ kind = "tool_call", turn = 1, call_id = "c1", args = {} })"#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("name"), "{msg}");

        lua.load(
            r#"
            local s = knl.session()
            pcall(function() s:append({ kind = "msg_user" }) end)
            assert(s:len() == 1, "a rejected reserved event was recorded")

            -- The documented shapes are accepted.
            s:append({ kind = "msg_user", content = "hi" })
            s:append({ kind = "tool_call", turn = 1, call_id = "c1",
                       name = "sh", args = { cmd = "ls" } })
            s:append({ kind = "tool_result", turn = 1, call_id = "c1",
                       ok = false, result = "boom" })
            assert(s:len() == 4)
        "#,
        )
        .exec()
        .expect("reserved kind chunk");
    }

    /// (author) A shell continuing an earlier conversation appends the
    /// assistant turns it already has: they take their place in the
    /// dialogue and change nothing the kernel accounts for.  The
    /// difference is the `author` stamp, which the shell does not get to
    /// write.
    #[test]
    fn a_carried_over_conversation_is_appended_and_never_billed() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({ budget = { tokens = 100 }, backend = backend(result("real")) })

            s:append({ kind = "msg_user", content = "asked before" })
            s:append({ kind = "model_response", turn = 1,
                       content = { { type = "text", text = "answered before" } },
                       usage = { input_tokens = 9000, output_tokens = 9000 },
                       author = "kernel" })

            -- Recorded, and recorded as the caller's however it was labelled.
            assert(kinds_of(s) == "run_started,msg_user,model_response", "recorded: " .. kinds_of(s))
            local carried = s:events()[3]
            assert(carried.author == "caller", "author: " .. tostring(carried.author))
            assert(s:events()[1].author == "kernel", "run_started must be the kernel's")

            -- In the dialogue, which is what it was appended for.
            local d = s:view("dialogue")
            assert(#d == 2 and d[2].role == "assistant", "dialogue rows: " .. tostring(#d))
            assert(d[2].content[1].text == "answered before")

            -- Out of the account: nothing counted, nothing charged, and
            -- the turn the kernel is about to hand out is still 1.
            assert(s:view("usage").model_calls == 0, "a carried-over response was counted")
            assert(s:view("usage").input_tokens == 0, "a carried-over response was summed")
            assert(s:remaining() == 100, "a carried-over response was charged")

            local out, err = s:call({})
            assert(err == nil, "call failed: " .. tostring(err))
            assert(out.turn == 1, "turn: " .. tostring(out.turn))
            assert(s:view("usage").model_calls == 1, "the call the run made is the one it counts")
            assert(s:view("usage").input_tokens == 10)
            assert(s:remaining() == 87, "remaining: " .. tostring(s:remaining()))
            assert(s:events()[4].author == "kernel", "the kernel's own record")
        "#,
        )
        .exec()
        .expect("carried-over conversation chunk");
    }

    /// (author) The run scope is `close`, not an event: appending
    /// `run_finished` writes a line and leaves the session open.
    #[test]
    fn an_appended_run_finished_does_not_close_the_run() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({ budget = { tokens = 100 } })
            s:append({ kind = "run_finished", reason = "carried over" })

            -- Still open: the writes a closed run refuses both go through.
            assert(s:append({ kind = "note" }) == 3, "the appended event ended the run")
            assert(s:spend(10) == 90)

            s:close("done")
            local evs = s:events()
            assert(kinds_of(s) == "run_started,run_finished,note,run_finished",
                   "recorded: " .. kinds_of(s))
            assert(evs[2].author == "caller" and evs[4].author == "kernel",
                   "the two run_finished events must differ by author")
            assert(evs[4].reason == "done")

            local ok = pcall(function() s:append({ kind = "note" }) end)
            assert(not ok, "a closed run took a write")
        "#,
        )
        .exec()
        .expect("appended run_finished chunk");
    }

    /// Open kinds carry any payload: the shell owns their shape.
    #[test]
    fn open_kinds_pass_through_unchecked() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            s:append({ kind = "decision" })
            s:append({ kind = "note", nested = { deep = { 1, 2, 3 } }, n = 4 })
            -- A near-miss of a reserved name is still an open kind.
            s:append({ kind = "user_msg" })
            assert(s:len() == 4)
            local evs = s:events(3)
            assert(evs[1].nested.deep[3] == 3, "payload survived")
            assert(evs[1].n == 4)
        "#,
        )
        .exec()
        .expect("open kind chunk");
    }

    /// `view("dialogue")` folds the conversational kinds, in seq order,
    /// and drops everything else.
    #[test]
    fn view_dialogue_folds_the_conversation() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({ backend = backend(result("ok")) })
            s:append({ kind = "msg_user", content = "hi" })
            -- Through a call: this run's own turn, recorded as it is charged.
            assert(s:call({}) ~= nil, "call failed")
            s:append({ kind = "tool_call", turn = 1, call_id = "c1",
                       name = "sh", args = { cmd = "ls" } })
            s:append({ kind = "tool_result", turn = 1, call_id = "c1",
                       ok = false, result = "boom" })
            s:append({ kind = "note", text = "not dialogue" })

            local d = s:view("dialogue")
            assert(#d == 3, "dialogue rows: " .. tostring(#d))
            assert(d[1].role == "user" and d[1].content == "hi")
            assert(d[2].role == "assistant" and d[2].content[1].text == "ok")
            assert(d[3].role == "tool" and d[3].call_id == "c1")
            assert(d[3].ok == false and d[3].result == "boom")
            -- Envelope fields do not leak into a dialogue row.
            assert(d[1].seq == nil and d[1].kind == nil)

            -- The fold grows with the history and stays consistent.
            s:append({ kind = "msg_user", content = "again" })
            local d2 = s:view("dialogue")
            assert(#d2 == 4 and d2[4].content == "again")
        "#,
        )
        .exec()
        .expect("dialogue view chunk");
    }

    /// `view("usage")` sums the model responses and counts the calls.
    #[test]
    fn view_usage_sums_model_responses() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({
                backend = backend(result("a", { input_tokens = 10, output_tokens = 3 }),
                                  result("b", { input_tokens = 5, thinking_tokens = 7 })),
            })
            local u = s:view("usage")
            assert(u.input_tokens == 0 and u.model_calls == 0, "empty totals")

            assert(s:call({}) ~= nil, "first call failed")
            s:append({ kind = "msg_user", content = "ignored by usage" })
            assert(s:call({}) ~= nil, "second call failed")

            u = s:view("usage")
            assert(u.input_tokens == 15, "input: " .. tostring(u.input_tokens))
            assert(u.output_tokens == 3, "output: " .. tostring(u.output_tokens))
            assert(u.thinking_tokens == 7, "thinking: " .. tostring(u.thinking_tokens))
            assert(u.model_calls == 2, "calls: " .. tostring(u.model_calls))

            -- Reading twice does not double-fold.
            local again = s:view("usage")
            assert(again.input_tokens == 15 and again.model_calls == 2)
        "#,
        )
        .exec()
        .expect("usage view chunk");
    }

    /// `view("tail", { n = k })` returns the last k events verbatim.
    #[test]
    fn view_tail_returns_the_last_events() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            for i = 1, 5 do s:append({ kind = "e" .. i }) end

            local t = s:view("tail", { n = 2 })
            assert(#t == 2, "tail len: " .. tostring(#t))
            assert(t[1].kind == "e4" and t[2].kind == "e5")
            assert(t[2].seq == 6, "tail keeps the envelope")

            assert(#s:view("tail", { n = 99 }) == 6, "n larger than the history")
            assert(#s:view("tail", { n = 0 }) == 0)
            assert(#s:view("tail") == 6, "n defaults to 20")
        "#,
        )
        .exec()
        .expect("tail view chunk");

        let msg = expect_err(&lua, r#"knl.session():view("tail", { n = -1 })"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("non-negative"), "{msg}");
    }

    /// (attribution) The view vocabulary is closed: an unknown name is an
    /// error, and so is a non-string name or non-table opts.
    #[test]
    fn view_rejects_unknown_names_and_bad_arguments() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.session():view("dialog")"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains(r#"unknown view "dialog""#), "{msg}");

        let msg = expect_err(&lua, r#"knl.session():view(42)"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("name must be a string"), "{msg}");

        let msg = expect_err(&lua, r#"knl.session():view("tail", "n=2")"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("opts must be a table"), "{msg}");
    }

    // -----------------------------------------------------------------
    // K2 — `s:call`
    // -----------------------------------------------------------------

    /// (K2 happy path) One call runs the backend once, and by the time it
    /// returns the response is in the history with the kernel's turn on
    /// it and its tokens are charged.  The second call's backend proves
    /// the first record was written before `call` returned rather than
    /// after.
    #[test]
    fn a_call_records_the_response_and_charges_it_before_returning() {
        let lua = vm();
        lua.load(
            r#"
            local s
            s = knl.session({
                budget = { tokens = 100 },
                backend = backend(result("first"), function()
                    assert(kinds_of(s) == "run_started,model_response",
                           "the first response was not recorded before call returned: " .. kinds_of(s))
                    return result("second")
                end),
            })

            local out, err = s:call({ messages = { "hi" } })
            assert(err == nil, "call failed: " .. tostring(err))
            assert(calls == 1, "backend runs: " .. tostring(calls))

            assert(out.turn == 1, "turn: " .. tostring(out.turn))
            assert(out.content[1].text == "first")
            assert(out.usage.input_tokens == 10 and out.usage.output_tokens == 3)
            assert(out.stop_reason == "end_turn")
            assert(out.remaining == 87, "remaining: " .. tostring(out.remaining))
            assert(out.exhausted == false)
            assert(s:remaining() == out.remaining, "out disagrees with the session")
            assert(s:exhausted() == out.exhausted)

            local recorded = s:events()[2]
            assert(recorded.kind == "model_response", "kind: " .. tostring(recorded.kind))
            assert(recorded.turn == 1 and recorded.stop_reason == "end_turn")
            assert(recorded.content[1].text == "first")
            assert(recorded.usage.output_tokens == 3)

            local second = s:call({})
            assert(second.turn == 2, "turn: " .. tostring(second.turn))
            assert(s:view("usage").model_calls == 2)
            assert(s:remaining() == 74, "remaining: " .. tostring(s:remaining()))
        "#,
        )
        .exec()
        .expect("call happy path chunk");
    }

    /// (K2) The request is the shell's: it reaches the backend as the very
    /// table that was passed, and the kernel never looks inside it — a
    /// value the kernel could not convert rides along untouched.
    #[test]
    fn the_request_reaches_the_backend_untouched() {
        let lua = vm();
        lua.load(
            r#"
            local seen
            local s = knl.session({
                backend = function(req) seen = req; return result() end,
            })

            local req = { messages = { { role = "user" } }, on_request = function() end }
            local out, err = s:call(req)
            assert(err == nil, "call failed: " .. tostring(err))
            assert(seen == req, "the request was copied on its way to the backend")
            assert(s:len() == 2, "recorded: " .. kinds_of(s))
        "#,
        )
        .exec()
        .expect("opaque request chunk");
    }

    /// (K2) A result the kernel cannot record buys the run nothing: no
    /// `model_response`, no charge, and the turn stays available for the
    /// call that gets it right.  What it does leave is the note a failed
    /// call leaves — the model was asked either way, and a run whose
    /// history says nothing happened would be the misleading one.
    #[test]
    fn a_result_that_breaks_the_contract_is_noted_and_charges_nothing() {
        let lua = vm();
        lua.load(
            r#"
            local cases = {
                -- An untagged empty table is a mapping, which is not an
                -- array however few blocks an answer has: a backend with
                -- nothing to report says so with a tagged empty array.
                { content = {}, usage = {}, stop_reason = "end_turn" },
                { content = "text", usage = {}, stop_reason = "end_turn" },
                { usage = {}, stop_reason = "end_turn" },
                { content = { { type = "text" } }, stop_reason = "end_turn" },
                { content = { { type = "text" } }, usage = 5, stop_reason = "end_turn" },
                { content = { { type = "text" } }, usage = {}, stop_reason = 7 },
                "not a table",
            }

            for i, bad in ipairs(cases) do
                local s = knl.session({
                    budget = { tokens = 50 },
                    backend = backend(bad, result("recovered")),
                })
                local out, err = s:call({})
                assert(out == nil, "case " .. i .. " was accepted")
                assert(err:find("knl: call:", 1, true) == 1, "case " .. i .. ": " .. tostring(err))

                -- The response is not in the history; the attempt is.
                assert(kinds_of(s) == "run_started,model_call_failed",
                       "case " .. i .. " wrote " .. kinds_of(s))
                local noted = s:events()[2]
                assert(noted.turn == 1, "case " .. i .. " noted turn: " .. tostring(noted.turn))
                assert(tostring(noted.error):find("backend result", 1, true) ~= nil,
                       "case " .. i .. " noted: " .. tostring(noted.error))
                assert(err:find(noted.error, 1, true) ~= nil,
                       "case " .. i .. ": the note and the return disagree")

                assert(s:remaining() == 50, "case " .. i .. " charged the budget")
                assert(s:view("usage").model_calls == 0, "case " .. i .. " counted as a call")

                local good = s:call({})
                assert(good.turn == 1, "case " .. i .. " consumed a turn: " .. tostring(good.turn))
            end
        "#,
        )
        .exec()
        .expect("contract violation chunk");
    }

    /// (K2 §2) An answer with no blocks is recorded as the answer it is.
    /// It costs its tokens, takes its turn and reaches the dialogue as an
    /// empty assistant row — because the alternative, standing an invented
    /// block in for it, puts words in the history the model never said.
    #[test]
    fn an_answer_with_no_blocks_is_recorded_as_it_arrived() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({
                budget = { tokens = 100 },
                backend = function()
                    return {
                        content = setmetatable({}, { __jsontype = "array" }),
                        usage = { input_tokens = 7, output_tokens = 0 },
                        stop_reason = "end_turn",
                    }
                end,
            })

            local out, err = s:call({})
            assert(err == nil, "an empty answer was refused: " .. tostring(err))
            assert(out.turn == 1, "turn: " .. tostring(out.turn))
            assert(type(out.content) == "table" and #out.content == 0,
                   "blocks came back: " .. tostring(#out.content))
            assert(getmetatable(out.content).__jsontype == "array",
                   "the empty answer came back as a mapping")
            assert(out.stop_reason == "end_turn")
            assert(out.remaining == 93, "remaining: " .. tostring(out.remaining))

            local recorded = s:events()[2]
            assert(recorded.kind == "model_response", "recorded: " .. kinds_of(s))
            assert(#recorded.content == 0, "the record grew blocks")
            assert(getmetatable(recorded.content).__jsontype == "array",
                   "the record holds a mapping where the answer had an array")
            assert(s:view("usage").input_tokens == 7, "an empty answer was not counted")

            -- The row is in the conversation, empty and all.
            local d = s:view("dialogue")
            assert(#d == 1 and d[1].role == "assistant", "dialogue rows: " .. tostring(#d))
            assert(#d[1].content == 0, "the dialogue row grew blocks")
        "#,
        )
        .exec()
        .expect("empty answer chunk");
    }

    /// (K2 §2) A provider that names no stop reason still answered.  The
    /// field is absent from what comes back and from the record, which is
    /// the same thing said twice rather than an empty label invented once.
    #[test]
    fn an_answer_without_a_stop_reason_is_recorded_without_one() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({
                backend = function()
                    return {
                        content = { { type = "text", text = "ok" } },
                        usage = { input_tokens = 3 },
                    }
                end,
            })

            local out, err = s:call({})
            assert(err == nil, "an unlabelled answer was refused: " .. tostring(err))
            assert(out.stop_reason == nil, "stop_reason: " .. tostring(out.stop_reason))
            assert(out.content[1].text == "ok")

            local recorded = s:events()[2]
            assert(recorded.kind == "model_response", "recorded: " .. kinds_of(s))
            assert(recorded.stop_reason == nil, "the record named a reason nobody gave")
            assert(s:view("usage").model_calls == 1)
        "#,
        )
        .exec()
        .expect("unlabelled answer chunk");
    }

    /// (K2 §4) A backend that raises and one that returns `nil, err` are
    /// the same event: the call is noted in the history and reported as
    /// `nil, err`, with the turn left unconsumed.
    #[test]
    fn a_failed_backend_is_noted_and_reported_the_same_way_either_way() {
        let lua = vm();
        lua.load(
            r#"
            for _, case in ipairs({
                { backend = function() error("transport exploded") end, text = "transport exploded" },
                { backend = function() return nil, "HTTP 500 after 3 attempts" end,
                  text = "HTTP 500 after 3 attempts" },
            }) do
                local s = knl.session({ budget = { tokens = 50 }, backend = case.backend })

                local out, err = s:call({})
                assert(out == nil, "a failed backend returned a result")
                assert(err:find("knl: call: backend:", 1, true) == 1, "err: " .. tostring(err))
                assert(err:find(case.text, 1, true) ~= nil, "err: " .. tostring(err))

                assert(kinds_of(s) == "run_started,model_call_failed", "recorded: " .. kinds_of(s))
                local noted = s:events()[2]
                assert(noted.turn == 1, "noted turn: " .. tostring(noted.turn))
                assert(noted.error:find(case.text, 1, true) ~= nil, "note: " .. tostring(noted.error))

                assert(s:remaining() == 50, "a failed call charged the budget")
                assert(s:view("usage").model_calls == 0)

                -- The turn was not consumed by the failure.
                local ok = knl.session({ backend = backend(result()) })
                assert(ok:call({}).turn == 1)
            end
        "#,
        )
        .exec()
        .expect("backend failure chunk");
    }

    /// (K2 §4) A closed run takes no note either — and the backend is not
    /// run at all, because a model call whose result provably cannot be
    /// recorded is the one thing `call` exists to prevent.
    #[test]
    fn a_closed_session_neither_calls_the_backend_nor_notes_anything() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({ backend = backend(result("never asked for")) })
            s:close("done")

            local out, err = s:call({})
            assert(out == nil, "a closed run returned a result")
            assert(err:find("knl: call:", 1, true) == 1, "err: " .. tostring(err))
            assert(err:find("session is closed", 1, true) ~= nil, "err: " .. tostring(err))
            assert(calls == 0, "the backend ran for a run that can record nothing")
            assert(kinds_of(s) == "run_started,run_finished", "recorded: " .. kinds_of(s))
        "#,
        )
        .exec()
        .expect("closed session chunk");
    }

    /// (K2 §6) The backend may write to the session while it runs, and
    /// what it writes lands ahead of the response — which is where it
    /// happened.
    #[test]
    fn what_the_backend_records_lands_before_the_response() {
        let lua = vm();
        lua.load(
            r#"
            local s
            s = knl.session({
                backend = function()
                    s:append({ kind = "note", text = "asked the provider" })
                    return result()
                end,
            })

            local out, err = s:call({})
            assert(err == nil, "call failed: " .. tostring(err))
            assert(kinds_of(s) == "run_started,note,model_response", "recorded: " .. kinds_of(s))

            local evs = s:events()
            assert(evs[2].seq < evs[3].seq, "the note did not precede the response")
            assert(out.turn == 1)
        "#,
        )
        .exec()
        .expect("re-entrancy chunk");
    }

    /// (K2 §7) A call with nowhere to go says so, and says it differently
    /// from a call that reached a backend and failed.  A named backend is
    /// a reserved slot: the name is carried, and no name resolves yet.
    #[test]
    fn an_unusable_backend_is_reported_as_configuration_not_as_a_failed_call() {
        let lua = vm();
        lua.load(
            r#"
            local none = knl.session()
            local out, err = none:call({})
            assert(out == nil and err == "knl: call: no backend bound", "err: " .. tostring(err))
            assert(kinds_of(none) == "run_started", "a call that never happened was recorded")

            local named = knl.session({ backend = "genai" })
            local out2, err2 = named:call({})
            assert(out2 == nil, "a built-in backend resolved")
            assert(err2 == "knl: call: unknown builtin backend: genai", "err: " .. tostring(err2))
            assert(kinds_of(named) == "run_started", "a slot that does not resolve is not a failed call")

            -- Same slot, same answer, per call.
            local _, err3 = knl.session():call({}, { backend = "genai" })
            assert(err3 == "knl: call: unknown builtin backend: genai", "err: " .. tostring(err3))

            -- Anything that is neither a function nor a name is rejected.
            local _, err4 = knl.session():call({}, { backend = 42 })
            assert(err4:find("meta.backend must be a function or a string", 1, true) ~= nil,
                   "err: " .. tostring(err4))
            local _, err5 = knl.session():call({}, "nope")
            assert(err5:find("meta must be a table", 1, true) ~= nil, "err: " .. tostring(err5))
        "#,
        )
        .exec()
        .expect("backend slot chunk");

        // The session conf is validated where it is given, and raises
        // there like the other malformed options.
        let msg = expect_err(&lua, r#"knl.session({ backend = 42 })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(
            msg.contains("backend must be a function or a string"),
            "{msg}"
        );
    }

    /// (K2 §1) A per-call backend wins for that call and only that call:
    /// the binding is not replaced, and the override's turn counts like
    /// any other.
    #[test]
    fn a_per_call_backend_overrides_the_binding_for_one_call() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({ backend = backend(result("bound")) })
            local strong = function() return result("strong") end

            local out = s:call({}, { backend = strong })
            assert(out.content[1].text == "strong", "the override was ignored")
            assert(out.turn == 1)
            assert(calls == 0, "the bound backend ran despite the override")

            local next_out = s:call({})
            assert(next_out.content[1].text == "bound", "the override outlived its call")
            assert(next_out.turn == 2, "the override's turn did not count")
            assert(calls == 1, "backend runs: " .. tostring(calls))
        "#,
        )
        .exec()
        .expect("override chunk");
    }

    /// (K2 §1) `has_backend` answers the one question a caller with a
    /// backend to lend has: does this session already have one?  It
    /// reports the binding — an override is not one, and asking costs
    /// neither a call nor an event.
    #[test]
    fn has_backend_reports_the_binding_without_making_a_call() {
        let lua = vm();
        lua.load(
            r#"
            assert(knl.session():has_backend() == false, "a session with nothing claimed a backend")
            assert(knl.session({}):has_backend() == false)
            assert(knl.session({ budget = { tokens = 1 } }):has_backend() == false)
            assert(knl.session({ backend = function() end }):has_backend() == true)
            -- A named built-in is a binding too, even though no name resolves yet.
            assert(knl.session({ backend = "genai" }):has_backend() == true)

            -- Asking records nothing and calls nobody.
            local none = knl.session()
            assert(none:has_backend() == false)
            assert(kinds_of(none) == "run_started", "asking recorded " .. kinds_of(none))
            assert(calls == 0)

            -- A per-call backend answers that call and nothing else, so the
            -- binding — and the answer — is what it was.
            local out, err = none:call({}, { backend = function() return result() end })
            assert(err == nil, "call failed: " .. tostring(err))
            assert(none:has_backend() == false, "an override was mistaken for a binding")

            local bound = knl.session({ backend = backend(result()) })
            assert(bound:has_backend() == true)
            bound:call({})
            assert(bound:has_backend() == true, "the binding did not survive its call")

            -- Still true once the run is over: it describes how the session
            -- was opened, not what it can still do.
            bound:close()
            assert(bound:has_backend() == true)
        "#,
        )
        .exec()
        .expect("has_backend chunk");
    }

    /// (I6) A bound backend lives in the session's user value, so a
    /// closure that captures its own session — the shape every caller
    /// that records around its calls writes — is an ordinary Lua cycle:
    /// unreachable, and collected.  Held in the registry instead, the two
    /// would keep each other alive for the life of the VM, and nothing in
    /// the run's own behaviour would ever say so.
    #[test]
    fn a_session_and_a_backend_that_captures_it_are_collected_together() {
        let lua = vm();
        lua.load(
            r#"
            collected = false

            -- Control: the same sentinel, captured by a closure a global
            -- holds. Nothing about it may be collected, which is what makes
            -- the case below mean something.
            local rooted = setmetatable({}, { __gc = function() rooted_collected = true end })
            rooted_collected = false
            still_reachable = function() return rooted end

            do
                -- Reachable only through the backend closure, which is
                -- reachable only through the session: its finalizer runs
                -- exactly when the pair is collected.
                local sentinel = setmetatable({}, { __gc = function() collected = true end })
                local s
                s = knl.session({
                    backend = function()
                        local _ = sentinel
                        s:append({ kind = "note", text = "asked the provider" })
                        return result()
                    end,
                })

                local out, err = s:call({})
                assert(err == nil, "call failed: " .. tostring(err))
                assert(kinds_of(s) == "run_started,note,model_response", "recorded: " .. kinds_of(s))
                assert(collected == false, "collected while still in use")
            end

            collectgarbage("collect")
            collectgarbage("collect")
            assert(collected, "the session and its backend kept each other alive")
            assert(rooted_collected == false, "the control was collected while still reachable")
        "#,
        )
        .exec()
        .expect("gc cycle chunk");
    }

    /// (I1) What a call returns is a copy: mutating it reaches neither
    /// the backend's own table nor the history.
    #[test]
    fn the_call_result_is_a_copy_of_what_was_recorded() {
        let lua = vm();
        lua.load(
            r#"
            local shared = result("original")
            local s = knl.session({ backend = function() return shared end })

            local out = s:call({})
            out.content[1].text = "TAMPERED"
            out.content[2] = { type = "text", text = "ghost" }
            out.usage.input_tokens = 999
            out.turn = 99

            assert(shared.content[1].text == "original", "the backend's table was reachable")
            assert(#shared.content == 1 and shared.usage.input_tokens == 10)

            local recorded = s:events()[2]
            assert(recorded.content[1].text == "original", "the history was reachable")
            assert(#recorded.content == 1, "blocks: " .. tostring(#recorded.content))
            assert(recorded.usage.input_tokens == 10)
            assert(recorded.turn == 1, "turn: " .. tostring(recorded.turn))
            assert(s:view("usage").input_tokens == 10)
        "#,
        )
        .exec()
        .expect("call copy chunk");
    }

    /// (K2) Turns are 1, 2, 3 … over the recorded responses, and a call
    /// that failed in between leaves no gap: the number it was going to
    /// take is the one the next success gets.
    #[test]
    fn turns_are_consecutive_across_a_failed_call() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({
                backend = backend(result("a"), function() return nil, "boom" end,
                                  result("b"), result("c")),
            })

            assert(s:call({}).turn == 1)
            local out, err = s:call({})
            assert(out == nil and err ~= nil, "the failure was not reported")
            assert(s:call({}).turn == 2, "a failed call consumed a turn")
            assert(s:call({}).turn == 3)

            assert(turns_of(s) == "1,2,3", "turns: " .. turns_of(s))
            assert(first_of(s, "model_call_failed").turn == 2,
                   "the note must name the turn the retry then took")
        "#,
        )
        .exec()
        .expect("turn numbering chunk");
    }

    /// (K2 / WF2) Turn numbering belongs to the session, not to whatever
    /// loop is driving it: a second pass over the same session continues
    /// where the first left off instead of starting again at 1.
    #[test]
    fn turns_stay_monotonic_across_several_loops_over_one_session() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({
                backend = backend(result(), result(), result(), result()),
            })

            -- Each pass is one loop's worth of calls, as a second
            -- `tool_loop.run` over the same session would be.
            local function pass(n)
                local seen = {}
                for _ = 1, n do
                    local out, err = s:call({})
                    assert(err == nil, "call failed: " .. tostring(err))
                    table.insert(seen, out.turn)
                end
                return table.concat(seen, ",")
            end

            assert(pass(2) == "1,2", "first pass")
            assert(pass(2) == "3,4", "the second pass restarted the numbering")
            assert(turns_of(s) == "1,2,3,4", "turns: " .. turns_of(s))
            assert(s:view("usage").model_calls == 4)
        "#,
        )
        .exec()
        .expect("session-wide turn chunk");
    }

    /// (K2 §3.4) Running out of budget sets a flag; it does not close the
    /// kernel's door.  Whether to open another turn is the caller's
    /// decision, so there is only one place where a run stops.
    #[test]
    fn an_exhausted_budget_is_a_flag_and_not_a_stop() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session({ budget = { tokens = 10 }, backend = backend(result(), result()) })

            local first = s:call({})
            assert(first.remaining == 0, "remaining: " .. tostring(first.remaining))
            assert(first.exhausted == true, "the flag was not set")

            local second = s:call({})
            assert(second ~= nil, "the kernel refused a call after the budget ran out")
            assert(second.turn == 2 and second.exhausted == true)
            assert(s:view("usage").model_calls == 2, "both calls were recorded")
        "#,
        )
        .exec()
        .expect("exhausted budget chunk");
    }

    /// (I1) A view is a fresh table every call: mutating it cannot reach
    /// the cached fold or the history.
    #[test]
    fn view_returns_a_fresh_table_each_call() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.session()
            s:append({ kind = "msg_user", content = "hi" })

            local d = s:view("dialogue")
            d[1].role = "TAMPERED"
            d[1].content = nil
            table.insert(d, { role = "ghost" })

            local again = s:view("dialogue")
            assert(#again == 1, "fold length changed: " .. tostring(#again))
            assert(again[1].role == "user", "role changed: " .. tostring(again[1].role))
            assert(again[1].content == "hi", "content changed")

            local u = s:view("usage")
            u.input_tokens = 999
            assert(s:view("usage").input_tokens == 0, "usage cache was reachable")
        "#,
        )
        .exec()
        .expect("view copy chunk");
    }
}
