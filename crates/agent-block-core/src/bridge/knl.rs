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
//! userdata returned by `knl.open(opts?)`, so the invariants are enforced
//! by the shape of the API rather than by convention:
//!
//! - **I1 append-only.**  There is deliberately no `update`, `delete` or
//!   `replace`.  `events()` / `view()` hand back freshly built tables, so
//!   a caller that mutates a returned value cannot reach recorded state.
//!   `seq` and `epoch_ms` are assigned by the kernel and overwrite any
//!   caller-supplied field of the same name; so is a `model_response`'s
//!   `turn`.
//! - **Scope is the session.**  There is no per-event author.  The session
//!   carries one `owner` (a principal id, or the reserved "anon" /
//!   "system"), read with `s:owner()`, and holds only its own events — so
//!   `view("usage")`, the budget charge and the turn numbering key on the
//!   `kind`: they count every `model_response`.  Appending a
//!   `model_response` *is* the recorded, numbered, charged call, and
//!   appending a `run_finished` records a line without ending a run that
//!   only `close` can end.
//! - **I3 budget monotonicity.**  `spend(n)` accepts non-negative whole
//!   amounts only and the balance can only decrease (floored at `0`).
//!   There is no API to raise or reset it.
//! - **I6 run scope.**  All state lives inside the userdata — no
//!   module-level statics, no Lua globals — so two sessions are fully
//!   independent.  `knl.open()` records `run_started` and
//!   `close(reason?)` records `run_finished`, after which `append` /
//!   `spend` are errors while reads keep working.
//! - **K2 model call.**  There is no composite call and the session keeps
//!   no backend of its own.  The driver calls the backend itself and
//!   appends the `model_response` directly; that one append is the
//!   recorded, numbered and charged call.
//!
//! ```lua
//! local s = knl.open({
//!     owner  = "user-42",              -- default: the reserved "anon"
//!     budget = { tokens = 10000 },
//! })
//! s:append({ kind = "msg_user", content = "hi" })
//! -- Drive the beat yourself: call the backend, append the response,
//! -- then read the kernel-assigned turn back.
//! s:append({ kind = "model_response", content = blocks, usage = u })
//! local turn = s:turns()                 -- the kernel-assigned turn
//! local events = s:events(from)          -- the record, from `from` on
//! local usage  = s:view("usage")         -- token totals + `at_seq`
//! local tail   = s:view("tail", { n = 5 })  -- the last events, verbatim
//! s:close("done")
//! ```
//!
//! Three read faces and no more: the events, the account, the last of the
//! record.  Turning events into a request for a provider — which role a
//! kind takes, whether a system message goes in front, where to cut the
//! history off — is the shell's policy, so it is written in Lua over
//! `events(from)` rather than named as a view here.
//!
//! Scope: in-memory only.  Effect execution, the Lua-side projection seam
//! and persistence are separate steps of the kernel/shell base design.

use std::cell::RefCell;

use mlua::prelude::*;
use serde_json::{Map, Value};

use super::{json_to_lua, lua_to_json};
use crate::knl;

/// K5 run scope: the only handle the Lua side has on kernel state.
struct Session {
    // `RefCell` (not `Mutex`): an Isle drives a single Lua VM on one
    // thread, and every borrow below is released before control returns
    // to Lua, so no borrow can overlap another.
    state: RefCell<knl::Session>,
}

impl Session {
    /// Open a run for `owner` with an optional token budget.
    fn new(owner: String, budget_tokens: Option<i64>) -> Self {
        Self {
            state: RefCell::new(knl::Session::new(owner, budget_tokens)),
        }
    }
}

/// The `knl: <method>: <reason>` attribution, as text.
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

impl LuaUserData for Session {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        // s:id() -> string
        methods.add_method("id", |_, this, ()| Ok(this.state.borrow().id().to_string()));

        // s:owner() -> string
        //
        // The principal the session belongs to (a real id, or the reserved
        // "anon" / "system").  Total — never nil.
        methods.add_method("owner", |_, this, ()| {
            Ok(this.state.borrow().owner().to_string())
        });

        // s:turns() -> number of model responses recorded so far
        //
        // Also the turn the last `model_response` append was numbered with:
        // after appending one, this is the kernel-assigned turn the shell
        // stamps onto its `tool_call` / `tool_result` events.
        methods.add_method("turns", |_, this, ()| Ok(this.state.borrow().turns()));

        // s:append(event) -> seq
        //
        // K1: the only way to add to the history, and there is no way to
        // change what is already in it.  A `model_response` is more than a
        // record: appending one numbers it (the kernel's turn, overwriting
        // any the caller supplied) and charges its usage against the
        // budget.  Read the number back with `s:turns()`.
        methods.add_method("append", |lua, this, event: LuaValue| {
            let obj = table_to_object(lua, "append", "event", event)?;
            this.state
                .borrow_mut()
                .append(obj)
                .map_err(|e| err("append", e))
        });

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
        // `usage` (totals plus the `at_seq` they reach) and `tail`
        // (`opts.n` events from the end).  Named folds only, and an
        // unknown name is an error: a projection the kernel does not name
        // is the shell's to build from `events(from)`.
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

/// Read `opts.owner`: the principal the session belongs to.
///
/// Total: an absent owner is the reserved anonymous id rather than `nil`,
/// so the policy layer above the kernel always has a real key to read.
fn parse_owner(opts: Option<&LuaTable>) -> LuaResult<String> {
    let Some(opts) = opts else {
        return Ok(knl::ANON.to_string());
    };
    let value: LuaValue = opts.get("owner")?;
    match value {
        LuaValue::Nil => Ok(knl::ANON.to_string()),
        LuaValue::String(owner) => Ok(owner.to_str()?.to_string()),
        other => Err(err(
            "session",
            format!("owner must be a string, got {}", other.type_name()),
        )),
    }
}

/// Build a session userdata from `opts` — the body of `knl.open`.
///
/// `opts.owner` is the principal (default the reserved anonymous id) and
/// `opts.budget` the token budget.
fn open_session(lua: &Lua, opts: LuaValue) -> LuaResult<LuaAnyUserData> {
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
    let owner = parse_owner(opts.as_ref())?;
    let budget_tokens = parse_budget(opts.as_ref())?;
    lua.create_userdata(Session::new(owner, budget_tokens))
}

/// Register the `knl` global.  No [`crate::host::HostContext`] is needed —
/// this layer keeps all state inside the session userdata.
///
/// `knl.open(opts?)` is the constructor (owner-aware).
pub fn register(lua: &Lua) -> LuaResult<()> {
    let knl_tbl = lua.create_table()?;

    // knl.open(opts?) -> Session userdata
    knl_tbl.set("open", lua.create_function(open_session)?)?;

    lua.globals().set("knl", knl_tbl)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helpers loaded into every VM.
    const FIXTURES: &str = r#"
        -- The recorded kinds in order, as one comparable string.
        function kinds_of(s)
            local names = {}
            for _, e in ipairs(s:events()) do
                table.insert(names, e.kind)
            end
            return table.concat(names, ",")
        end
    "#;

    /// Fresh Lua VM with only the `knl` bridge registered.
    fn vm() -> Lua {
        let lua = Lua::new();
        register(&lua).expect("register knl");
        lua.load(FIXTURES).exec().expect("fixtures");
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
            local s = knl.open()
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
            local s = knl.open()
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
            local s = knl.open()
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

    /// (I1) `seq` / `epoch_ms` are kernel-owned: a caller-supplied value is
    /// overwritten rather than trusted.  There is no `author` field.
    #[test]
    fn kernel_owned_fields_override_caller_values() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open()
            local seq = s:append({ kind = "user_msg", seq = 999, epoch_ms = 1 })
            assert(seq == 2, "returned seq: " .. tostring(seq))
            local e = s:events(2)[1]
            assert(e.seq == 2, "stored seq: " .. tostring(e.seq))
            assert(e.epoch_ms ~= 1, "epoch_ms must be kernel-assigned")
            assert(e.author == nil, "there is no per-event author anymore")
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
            local s = knl.open()
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

        let msg = expect_err(&lua, r#"knl.open():append({ text = "no kind" })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("kind is required"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open():append({ kind = 42 })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("kind must be a string"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open():append("not a table")"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("event must be a table"), "{msg}");

        // A rejected append leaves no trace in the history.
        lua.load(
            r#"
            local s = knl.open()
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
            local s = knl.open({ budget = { tokens = 100 } })
            s:spend(-1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("non-negative"), "{msg}");

        lua.load(
            r#"
            local s = knl.open({ budget = { tokens = 100 } })
            pcall(function() s:spend(-1) end)
            assert(s:remaining() == 100, "balance changed: " .. tostring(s:remaining()))
            -- A negative spend is rejected even without a budget.
            local ok = pcall(function() knl.open():spend(-1) end)
            assert(not ok, "negative spend must be rejected without a budget too")
            -- So is a non-numeric amount.
            local ok2 = pcall(function() knl.open():spend("many") end)
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
            local s = knl.open({ budget = { tokens = 1000 } })
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
            local s = knl.open()
            assert(s:remaining() == nil, "remaining must be nil without a budget")
            assert(s:spend(50) == nil, "spend must return nil without a budget")
            assert(s:exhausted() == false, "no budget can never be exhausted")

            -- An empty opts table behaves the same way.
            local s2 = knl.open({})
            assert(s2:remaining() == nil)
        "#,
        )
        .exec()
        .expect("no-budget chunk");
    }

    /// (attribution) Malformed `budget` options are rejected by
    /// `knl.open` itself.
    #[test]
    fn session_validates_budget_options() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.open({ budget = { tokens = -1 } })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("budget.tokens"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open({ budget = {} })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("required"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open({ budget = 100 })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("budget must be a table"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open("nope")"#);
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
            local a = knl.open({ budget = { tokens = 100 } })
            local b = knl.open({ budget = { tokens = 100 } })

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
            local s = knl.open()
            s:close()
            s:append({ kind = "after_close" })
        "#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        let msg = expect_err(
            &lua,
            r#"
            local s = knl.open({ budget = { tokens = 10 } })
            s:close()
            s:spend(1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        lua.load(
            r#"
            local s = knl.open({ budget = { tokens = 10 } })
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
            local s = knl.open()
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
            local s = knl.open()
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
            local s = knl.open()
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
            local d = knl.open()
            d:close()
            assert(d:events()[2].reason == "closed", "default reason")
        "#,
        )
        .exec()
        .expect("run boundary chunk");

        let msg = expect_err(&lua, r#"knl.open():close({ not_a = "string" })"#);
        assert!(msg.contains("knl: close:"), "missing attribution: {msg}");
        assert!(msg.contains("reason must be a string"), "{msg}");
    }

    /// The reserved kinds the shell writes are checked by the kernel; the
    /// required fields are named in an attributed error and nothing is
    /// recorded.
    #[test]
    fn reserved_kinds_are_validated_with_attributed_errors() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.open():append({ kind = "msg_user" })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("msg_user"), "{msg}");
        assert!(msg.contains("content"), "{msg}");

        let msg = expect_err(
            &lua,
            r#"knl.open():append({ kind = "tool_result", turn = 1, call_id = "c", ok = "yes", result = 1 })"#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("a boolean"), "{msg}");

        let msg = expect_err(
            &lua,
            r#"knl.open():append({ kind = "tool_call", turn = 1, call_id = "c1", args = {} })"#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("name"), "{msg}");

        lua.load(
            r#"
            local s = knl.open()
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

    /// The run scope is `close`, not an event: appending `run_finished`
    /// writes a line and leaves the session open.
    #[test]
    fn an_appended_run_finished_does_not_close_the_run() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ budget = { tokens = 100 } })
            s:append({ kind = "run_finished", reason = "carried over" })

            -- Still open: the writes a closed run refuses both go through.
            assert(s:append({ kind = "note" }) == 3, "the appended event ended the run")
            assert(s:spend(10) == 90)

            s:close("done")
            local evs = s:events()
            assert(kinds_of(s) == "run_started,run_finished,note,run_finished",
                   "recorded: " .. kinds_of(s))
            assert(evs[2].reason == "carried over" and evs[4].reason == "done",
                   "both run_finished events are recorded, the appended one and the close")

            local ok = pcall(function() s:append({ kind = "note" }) end)
            assert(not ok, "a closed run took a write")
        "#,
        )
        .exec()
        .expect("appended run_finished chunk");
    }

    /// `view("tail", { n = k })` returns the last k events verbatim.
    #[test]
    fn view_tail_returns_the_last_events() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open()
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

        let msg = expect_err(&lua, r#"knl.open():view("tail", { n = -1 })"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("non-negative"), "{msg}");
    }

    /// (attribution) The view vocabulary is closed: an unknown name is an
    /// error, and so is a non-string name or non-table opts.
    #[test]
    fn view_rejects_unknown_names_and_bad_arguments() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.open():view("dialog")"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains(r#"unknown view "dialog""#), "{msg}");

        let msg = expect_err(&lua, r#"knl.open():view(42)"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("name must be a string"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open():view("tail", "n=2")"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("opts must be a table"), "{msg}");
    }

    /// (I1) A view is a fresh table every call: mutating it cannot reach
    /// the cached fold or the history.
    #[test]
    fn view_returns_a_fresh_table_each_call() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open()
            s:append({ kind = "msg_user", content = "hi" })

            local t = s:view("tail", { n = 1 })
            t[1].kind = "TAMPERED"
            t[1].content = nil
            table.insert(t, { kind = "ghost" })

            local again = s:view("tail", { n = 1 })
            assert(#again == 1, "tail length changed: " .. tostring(#again))
            assert(again[1].kind == "msg_user", "kind changed: " .. tostring(again[1].kind))
            assert(again[1].content == "hi", "content changed")

            local u = s:view("usage")
            u.input_tokens = 999
            u.at_seq = 999
            local fresh = s:view("usage")
            assert(fresh.input_tokens == 0, "usage cache was reachable")
            assert(fresh.at_seq == 2, "at_seq: " .. tostring(fresh.at_seq))
        "#,
        )
        .exec()
        .expect("view copy chunk");
    }
}
