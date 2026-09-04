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
//!   `view("usage")` and the turn numbering key on the `kind`: they count
//!   every `model_response`.  Appending a `model_response` records and
//!   numbers it, and appending a `run_finished` records a line without
//!   ending a run that only `close` can end.
//! - **I3 budget monotonicity.**  The budget is a quota an owner grants
//!   the run, not a ledger of what it used.  `reserve(n)` asks *before*
//!   spending — it deducts and returns `true`, or refuses with `false,
//!   tag` and leaves the balance exactly as it was — and `spend(n)`
//!   settles afterwards.  Both take non-negative whole amounts and the
//!   balance can only decrease; there is no API to raise or reset it, and
//!   no `append` moves it.  What a run actually consumed is
//!   `view("usage")`, which is independent of the counter.
//! - **I6 run scope.**  All state lives inside the userdata — no
//!   module-level statics, no Lua globals — so two sessions are fully
//!   independent.  `knl.open()` records `run_started` — carrying the
//!   grant, so the log says what was allowed — and the run is closed by
//!   `run_finished`, after which `append` / `reserve` / `spend` are errors
//!   while reads keep working.  Three paths reach that boundary and the
//!   log never loses it: `close(reason?)` said explicitly; a
//!   `local s <close> = knl.open{...}` scope ending, which records
//!   `scope_exit` — or `error` with the message in `detail` when the block
//!   raised; and the drop backstop, `dropped`, for a handle nobody closed.
//!   Whichever runs first wins and the rest are no-ops, because `close`
//!   is idempotent — so an explicit reason is never overwritten by the
//!   scope or the collector that follows it.
//! - **K2 model call.**  There is no composite call and the session keeps
//!   no backend of its own.  The driver reserves what it estimates the
//!   call will cost, calls the backend itself, appends the
//!   `model_response`, and settles the difference with `spend`.
//!
//! ```lua
//! local s = knl.open({
//!     owner  = "user-42",              -- default: the reserved "anon"
//!     budget = { amount = 10000, tag = "tokens" },
//! })
//! s:append({ kind = "msg_user", content = "hi" })
//! -- Drive the beat yourself: ask for the estimate first, call the
//! -- backend, append the response, then settle what it really cost.
//! local ok, tag = s:reserve(est)
//! if not ok then return { budget_stopped = true, tag = tag } end
//! s:append({ kind = "model_response", content = blocks, usage = u })
//! local turn = s:turns()                 -- the kernel-assigned turn
//! s:spend(math.max(actual - est, 0))     -- the settlement
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
//! Storage backend.  `knl.open` takes an optional `store`: absent or
//! `"mem"` keeps the in-memory log (the default), while
//! `{ sqlite = "<path>" }` opens a durable, per-session SQLite stream (only
//! in a build with the `sqlite` feature).  `knl.resume({ store = { sqlite =
//! "<path>" }, session = "<id>", budget? })` reopens a persisted stream and
//! re-folds it, so a resumed session takes new turns whose numbering and
//! accounting continue from the recorded state — it behaves exactly like a
//! fresh session, only pre-loaded.
//!
//! Scope: effect execution and the Lua-side projection seam are separate
//! steps of the kernel/shell base design.

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
    /// Wrap a kernel session as the Lua userdata.
    fn from_state(state: knl::Session) -> Self {
        Self {
            state: RefCell::new(state),
        }
    }

    /// Open a run for `owner` with an optional budget grant, on the
    /// in-memory store.
    fn new(owner: String, grant: Option<knl::BudgetGrant>) -> LuaResult<Self> {
        let state = knl::Session::new(owner, grant).map_err(|e| err("open", e))?;
        Ok(Self::from_state(state))
    }
}

/// The backstop under `close` and `<close>`: a handle nobody ended still
/// records the run's boundary, here, where the value dies.
///
/// A dropped handle is the one close path with no caller left to tell, so
/// it cannot fail loudly the way the other two do: a failed append is a
/// `warn!` and nothing else.  Panicking in `drop` would abort the process
/// (a Lua collection cycle is not a place to unwind from), and a run
/// already past its last reader is not worth that.  What the boundary
/// costs is one line in the log; what it buys is a resumed or audited
/// stream that is not silently open forever.
impl Drop for Session {
    fn drop(&mut self) {
        // `try_borrow_mut`, not `borrow_mut`: a panic unwinding out of a
        // method leaves the borrow live, and this runs during that unwind.
        let Ok(mut state) = self.state.try_borrow_mut() else {
            tracing::warn!("knl: session dropped while borrowed; run_finished was not recorded");
            return;
        };
        if state.is_closed() {
            return;
        }
        if let Err(e) = state.close(Some(knl::CLOSE_REASON_DROPPED)) {
            tracing::warn!(
                session = %state.id(),
                error = %e,
                "knl: dropped session could not record run_finished"
            );
        }
    }
}

/// The longest `detail` a close records, in characters.
///
/// A `run_finished` says why a run ended, and an error message can be a
/// whole traceback; the cap keeps one bad turn from putting a page into the
/// log.  Counted in `chars` so the cut never lands inside one.
const DETAIL_MAX_CHARS: usize = 200;

/// The error a `<close>` scope was unwinding with, as `detail` text.
///
/// Read without re-entering Lua (no `__tostring` call): the value arrives
/// while the VM is already unwinding, and a metamethod raising there would
/// replace the error the log is trying to record.
fn error_detail(error: &LuaValue) -> String {
    let text = match error {
        LuaValue::String(s) => s.to_string_lossy(),
        LuaValue::Error(e) => e.to_string(),
        LuaValue::Integer(i) => i.to_string(),
        LuaValue::Number(n) => n.to_string(),
        other => format!("<{}>", other.type_name()),
    };
    if text.chars().count() <= DETAIL_MAX_CHARS {
        return text;
    }
    text.chars().take(DETAIL_MAX_CHARS).collect::<String>() + "..."
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
        // change what is already in it.  Appending a `model_response`
        // numbers it (the kernel's turn, overwriting any the caller
        // supplied); read the number back with `s:turns()`.  No append
        // touches the budget — that is `reserve` before the call and
        // `spend` after it.
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
            let selected = this
                .state
                .borrow()
                .events(from.unwrap_or(0))
                .map_err(|e| err("events", e))?;
            // The borrow is released above: json_to_lua re-enters Lua.
            json_to_lua(lua, Value::Array(selected))
        });

        // s:len() -> number of recorded events
        methods.add_method("len", |_, this, ()| {
            let n = this.state.borrow().len().map_err(|e| err("len", e))?;
            Ok(n as u64)
        });

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

        // s:reserve(n) -> true | false, tag
        //
        // K4, the decision point: ask before spending.  `true` means the
        // amount was taken off the balance; `false` means it would not fit
        // and *nothing* was taken, with the grant's `tag` as the second
        // return so a caller can name the allowance that stopped it
        // without reading the log.  Always `true` without a budget.
        methods.add_method("reserve", |_, this, amount: LuaValue| {
            let Some(amount) = as_whole(&amount) else {
                return Err(err(
                    "reserve",
                    format!(
                        "amount must be a non-negative whole number, got {}",
                        lua_value_for_msg(&amount)
                    ),
                ));
            };
            let mut state = this.state.borrow_mut();
            let granted = state.reserve(amount).map_err(|e| err("reserve", e))?;
            // The tag rides along only on a refusal: it answers "which
            // budget stopped you", which is a question only then.
            let tag = if granted {
                None
            } else {
                state.grant().and_then(|grant| grant.tag.clone())
            };
            Ok((granted, tag))
        });

        // s:spend(n) -> remaining (nil when the session has no budget)
        //
        // K4: the settlement after a reservation.  Non-negative amounts
        // only, and the balance never rises.
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
            // A close whose `run_finished` append fails (CAS conflict on a
            // shared durable stream, busy database) surfaces here: the
            // session stays open and the caller knows the boundary was not
            // recorded, instead of a silent closed=true with no record.
            this.state
                .borrow_mut()
                .close(reason.as_deref())
                .map_err(|e| err("close", e))?;
            Ok(())
        });

        // __close(self, err) — the Lua 5.4 to-be-closed variable:
        //
        //     do
        //         local s <close> = knl.open({ owner = "u" })
        //         ...
        //     end   -- the run's boundary is recorded here
        //
        // The reason says how the scope ended, not what went wrong: a clean
        // exit is "scope_exit", an unwinding one "error", with the message
        // in `detail`.  Folding the message into the reason would make every
        // distinct failure its own reason and the vocabulary unreadable.
        //
        // An explicit `close` earlier in the block already ended the run, and
        // this is a no-op then: the caller's reason is the one in the log.
        // A failed append surfaces as an error, exactly as it does from
        // `close` — Lua 5.4 propagates it out of the scope — because a close
        // that reports success with no `run_finished` recorded is the one
        // outcome the boundary exists to rule out.
        methods.add_meta_method(LuaMetaMethod::Close, |_, this, error: LuaValue| {
            if this.state.borrow().is_closed() {
                return Ok(());
            }
            // Computed before the borrow: nothing about the error value is
            // read while the session is held.
            let (reason, detail) = match error {
                LuaValue::Nil => (knl::CLOSE_REASON_SCOPE_EXIT, None),
                error => (knl::CLOSE_REASON_ERROR, Some(error_detail(&error))),
            };
            this.state
                .borrow_mut()
                .close_with(Some(reason), detail.as_deref())
                .map_err(|e| err("close", e))?;
            Ok(())
        });
    }
}

/// The fields a `budget` table may carry.  Anything else is a typo, and a
/// typo in a quota must not be read as "no limit on that axis".
const BUDGET_FIELDS: [&str; 3] = ["amount", "tag", "desc"];

/// Read an optional string field of the `budget` table.
fn budget_string(budget: &LuaTable, field: &str) -> LuaResult<Option<String>> {
    let value: LuaValue = budget.get(field)?;
    match value {
        LuaValue::Nil => Ok(None),
        LuaValue::String(s) => Ok(Some(s.to_str()?.to_string())),
        other => Err(err(
            "session",
            format!("budget.{field} must be a string, got {}", other.type_name()),
        )),
    }
}

/// Read `opts.budget` into the run's grant: `{ amount, tag?, desc? }`.
///
/// `amount` is the quota and the only field the kernel interprets; `tag`
/// names its unit and `desc` records what was allowed and why, both of
/// which ride onto `run_started` verbatim.  An unknown field is an error
/// rather than a value quietly ignored: a misspelt cap that reads as
/// "no cap" is exactly the failure a budget exists to prevent.
fn parse_budget(opts: Option<&LuaTable>) -> LuaResult<Option<knl::BudgetGrant>> {
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

    for pair in budget.clone().pairs::<LuaValue, LuaValue>() {
        let (key, _) = pair?;
        let LuaValue::String(name) = &key else {
            return Err(err(
                "session",
                format!("budget fields must be named, got a {}", key.type_name()),
            ));
        };
        let name = name.to_str()?.to_string();
        if !BUDGET_FIELDS.contains(&name.as_str()) {
            return Err(err(
                "session",
                format!("unknown budget field {name:?} (expected amount / tag / desc)"),
            ));
        }
    }

    let amount: LuaValue = budget.get("amount")?;
    if matches!(amount, LuaValue::Nil) {
        return Err(err(
            "session",
            "budget.amount is required (non-negative whole number)",
        ));
    }
    let Some(amount) = as_whole_non_negative(&amount) else {
        return Err(err(
            "session",
            format!(
                "budget.amount must be a non-negative whole number, got {}",
                lua_value_for_msg(&amount)
            ),
        ));
    };

    Ok(Some(knl::BudgetGrant {
        amount,
        tag: budget_string(&budget, "tag")?,
        desc: budget_string(&budget, "desc")?,
    }))
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
        LuaValue::String(owner) => {
            let owner = owner.to_str()?.to_string();
            // The reserved ids are the kernel's own namespace: an untrusted Lua
            // caller must not claim ANON or SYSTEM, or it could impersonate a
            // reserved principal on `run_started`.  Compared against the consts,
            // not literal strings, so the guard tracks the kernel's definition.
            // Unspecified owner still defaults to the kernel-assigned ANON above.
            if owner == knl::ANON || owner == knl::SYSTEM {
                return Err(err("open", format!("owner {owner:?} is reserved")));
            }
            Ok(owner)
        }
        other => Err(err(
            "session",
            format!("owner must be a string, got {}", other.type_name()),
        )),
    }
}

/// The storage backend `opts.store` asks for.
enum StoreSpec {
    /// The in-memory store (the default): absent or `"mem"`.
    Mem,
    /// A durable SQLite stream at the given path.
    Sqlite(String),
}

/// Read `opts.store`: absent / `"mem"` → in-memory, `{ sqlite = "<path>" }`
/// → durable.  `method` names the caller (`open` / `resume`) for attribution.
fn parse_store(method: &str, opts: Option<&LuaTable>) -> LuaResult<StoreSpec> {
    let Some(opts) = opts else {
        return Ok(StoreSpec::Mem);
    };
    let store: LuaValue = opts.get("store")?;
    match store {
        LuaValue::Nil => Ok(StoreSpec::Mem),
        LuaValue::String(name) => {
            let name = name.to_str()?.to_string();
            if name == "mem" {
                Ok(StoreSpec::Mem)
            } else {
                Err(err(
                    method,
                    format!(r#"unknown store {name:?} (expected "mem" or {{ sqlite = <path> }})"#),
                ))
            }
        }
        LuaValue::Table(table) => {
            let sqlite: LuaValue = table.get("sqlite")?;
            match sqlite {
                LuaValue::String(path) => Ok(StoreSpec::Sqlite(path.to_str()?.to_string())),
                LuaValue::Nil => Err(err(
                    method,
                    "store table must carry a sqlite = <path> field",
                )),
                other => Err(err(
                    method,
                    format!(
                        "store.sqlite must be a string path, got {}",
                        other.type_name()
                    ),
                )),
            }
        }
        other => Err(err(
            method,
            format!(
                r#"store must be "mem" or a table {{ sqlite = <path> }}, got {}"#,
                other.type_name()
            ),
        )),
    }
}

/// Open a NEW durable session on the SQLite stream at `path`.
///
/// The stream id is minted here and adopted as the session's own id, so the
/// id `knl.open` reports (`s:id()`) is exactly the stream a later
/// `knl.resume` reopens — the durable identity is one string, not two.
#[cfg(feature = "sqlite")]
fn open_sqlite(owner: String, grant: Option<knl::BudgetGrant>, path: &str) -> LuaResult<Session> {
    let stream = uuid::Uuid::new_v4().to_string();
    let store = knl::SqliteEventStore::open(std::path::Path::new(path), stream.clone())
        .map_err(|e| err("open", e))?;
    let mut state =
        knl::Session::open_on(owner, grant, Box::new(store)).map_err(|e| err("open", e))?;
    state.adopt_id(stream);
    Ok(Session::from_state(state))
}

/// Without the `sqlite` feature a durable store cannot be built, so the
/// request is a clear error rather than a silent fall back to memory.
#[cfg(not(feature = "sqlite"))]
fn open_sqlite(
    _owner: String,
    _grant: Option<knl::BudgetGrant>,
    _path: &str,
) -> LuaResult<Session> {
    Err(err(
        "open",
        "a sqlite store needs the 'sqlite' feature, which this build does not have",
    ))
}

/// Reopen the durable SQLite stream `session_id` at `path` and resume it.
///
/// `Session::resume` re-folds the persisted log; the reopened stream's id is
/// adopted so `s:id()` matches the stream the caller named.
#[cfg(feature = "sqlite")]
fn resume_sqlite(
    grant: Option<knl::BudgetGrant>,
    path: &str,
    session_id: String,
) -> LuaResult<Session> {
    let store = knl::SqliteEventStore::open(std::path::Path::new(path), session_id.clone())
        .map_err(|e| err("resume", e))?;
    let mut state = knl::Session::resume(grant, Box::new(store)).map_err(|e| err("resume", e))?;
    // The open path refuses an untrusted caller claiming a reserved
    // principal (parse_owner); resume must hold the same line, or Lua could
    // reopen a SYSTEM-owned stream and write into the reserved namespace.
    // ANON streams stay resumable — they are what unspecified-owner Lua
    // sessions (and pre-owner logs) record as.
    if state.owner() == knl::SYSTEM {
        return Err(err(
            "resume",
            format!("stream owner {:?} is reserved", knl::SYSTEM),
        ));
    }
    state.adopt_id(session_id);
    Ok(Session::from_state(state))
}

/// Without the `sqlite` feature there is no durable stream to resume.
#[cfg(not(feature = "sqlite"))]
fn resume_sqlite(
    _grant: Option<knl::BudgetGrant>,
    _path: &str,
    _session_id: String,
) -> LuaResult<Session> {
    Err(err(
        "resume",
        "a sqlite store needs the 'sqlite' feature, which this build does not have",
    ))
}

/// Build a session userdata from `opts` — the body of `knl.open`.
///
/// `opts.owner` is the principal (default the reserved anonymous id),
/// `opts.budget` the grant (`{ amount, tag?, desc? }`), and `opts.store`
/// the backend (in-memory by default, or `{ sqlite = "<path>" }` for a
/// durable stream).
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
    let grant = parse_budget(opts.as_ref())?;
    let session = match parse_store("open", opts.as_ref())? {
        StoreSpec::Mem => Session::new(owner, grant)?,
        StoreSpec::Sqlite(path) => open_sqlite(owner, grant, &path)?,
    };
    lua.create_userdata(session)
}

/// Resume a persisted session — the body of `knl.resume`.
///
/// Requires `opts.store = { sqlite = "<path>" }` and `opts.session =
/// "<stream id>"`.  `opts.budget` is optional and means the owner grants
/// *again*: it is recorded and added to the balance the log already
/// carries, rather than replacing it.  The returned userdata is the same
/// one `knl.open` returns, only pre-loaded with the recorded turn count
/// and the balance folded from the ledger.
fn resume_session(lua: &Lua, opts: LuaValue) -> LuaResult<LuaAnyUserData> {
    let opts = match opts {
        LuaValue::Table(t) => t,
        LuaValue::Nil => {
            return Err(err("resume", "opts must be a table with store and session"));
        }
        other => {
            return Err(err(
                "resume",
                format!("opts must be a table, got {}", other.type_name()),
            ));
        }
    };
    let grant = parse_budget(Some(&opts))?;
    let path = match parse_store("resume", Some(&opts))? {
        StoreSpec::Sqlite(path) => path,
        StoreSpec::Mem => {
            return Err(err(
                "resume",
                "resume needs a sqlite store (store = { sqlite = <path> })",
            ));
        }
    };
    let session: LuaValue = opts.get("session")?;
    let session_id = match session {
        LuaValue::String(id) => id.to_str()?.to_string(),
        LuaValue::Nil => {
            return Err(err(
                "resume",
                "session is required (the stream id to reopen)",
            ));
        }
        other => {
            return Err(err(
                "resume",
                format!("session must be a string, got {}", other.type_name()),
            ));
        }
    };
    let state = resume_sqlite(grant, &path, session_id)?;
    lua.create_userdata(state)
}

/// Register the `knl` global.  No [`crate::host::HostContext`] is needed —
/// this layer keeps all state inside the session userdata.
///
/// `knl.open(opts?)` is the constructor (owner- and store-aware);
/// `knl.resume(opts)` reopens a persisted SQLite session.
pub fn register(lua: &Lua) -> LuaResult<()> {
    let knl_tbl = lua.create_table()?;

    // knl.open(opts?) -> Session userdata
    knl_tbl.set("open", lua.create_function(open_session)?)?;

    // knl.resume(opts) -> Session userdata (durable stream re-folded)
    knl_tbl.set("resume", lua.create_function(resume_session)?)?;

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
            local s = knl.open({ budget = { amount = 100, tag = "tokens" } })
            s:spend(-1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("non-negative"), "{msg}");

        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 100, tag = "tokens" } })
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
            local s = knl.open({ budget = { amount = 1000, tag = "tokens" } })
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

        let msg = expect_err(&lua, r#"knl.open({ budget = { amount = -1 } })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("budget.amount"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open({ budget = {} })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("required"), "{msg}");

        // A misspelt field is an error, not a silently ignored cap: the
        // failure a budget exists to prevent is exactly "the limit I set
        // was not read".
        let msg = expect_err(&lua, r#"knl.open({ budget = { tokens = 100 } })"#);
        assert!(msg.contains("knl: session:"), "missing attribution: {msg}");
        assert!(msg.contains("unknown budget field"), "{msg}");
        assert!(msg.contains("tokens"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open({ budget = { amount = 10, tag = 7 } })"#);
        assert!(msg.contains("budget.tag must be a string"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open({ budget = { amount = 1.5 } })"#);
        assert!(msg.contains("budget.amount"), "{msg}");

        // The words are optional, and carried verbatim when given.
        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 42, tag = "tokens",
                                            desc = "one nightly run" } })
            assert(s:remaining() == 42, "remaining: " .. tostring(s:remaining()))
            local granted = s:events()[2]
            assert(granted.kind == "budget_granted", "kind: " .. tostring(granted.kind))
            assert(granted.amount == 42 and granted.tag == "tokens")
            assert(granted.desc == "one nightly run", "desc: " .. tostring(granted.desc))

            local bare = knl.open({ budget = { amount = 7 } })
            local g2 = bare:events()[2]
            assert(g2.amount == 7 and g2.tag == nil and g2.desc == nil,
                   "a grant with no words must invent none")
        "#,
        )
        .exec()
        .expect("grant options chunk");

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
            local a = knl.open({ budget = { amount = 100, tag = "tokens" } })
            local b = knl.open({ budget = { amount = 100, tag = "tokens" } })

            assert(type(a:id()) == "string" and #a:id() > 0, "id must be a non-empty string")
            assert(a:id() ~= b:id(), "session ids must be unique")

            a:append({ kind = "only_in_a" })
            a:spend(60)

            -- a: run_started, budget_granted, only_in_a, budget_spent.
            -- b: run_started, budget_granted.
            assert(a:len() == 4 and b:len() == 2, "history leaked between sessions")
            assert(#b:events(3) == 0, "b holds only its own opening")
            assert(a:remaining() == 40 and b:remaining() == 100, "budget leaked between sessions")

            -- Closing one leaves the other usable.
            a:close()
            assert(b:append({ kind = "still_open" }) == 3)
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
            local s = knl.open({ budget = { amount = 10, tag = "tokens" } })
            s:close()
            s:spend(1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        // A closed run cannot be granted more either.
        let msg = expect_err(
            &lua,
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "tokens" } })
            s:close()
            s:reserve(1)
        "#,
        );
        assert!(msg.contains("knl: reserve:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "tokens" } })
            s:append({ kind = "before_close" })
            s:spend(4)
            s:close()
            s:close() -- idempotent

            -- Reads still work after the run scope ends.
            assert(s:len() == 5,
                   "run_started + budget_granted + before_close + budget_spent + run_finished")
            assert(s:events()[3].kind == "before_close")
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

    /// The budget ledger is the kernel's: Lua can read those events but not
    /// write them.  Appending one by hand would be granting yourself the
    /// quota your owner set, so it is refused and the balance does not
    /// move.
    #[test]
    fn lua_cannot_append_the_budget_kinds_by_hand() {
        let lua = vm();

        let msg = expect_err(
            &lua,
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "tokens" } })
            s:append({ kind = "budget_reserved", amount = 5 })
        "#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("kernel only"), "{msg}");
        assert!(msg.contains("budget_reserved"), "{msg}");

        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "tokens" } })
            for _, ev in ipairs({
                { kind = "budget_granted", amount = 1000000 },
                { kind = "budget_reserved", amount = 5 },
                { kind = "budget_refused", amount = 5, remaining = 0 },
                { kind = "budget_spent", amount = 5 },
            }) do
                local ok = pcall(function() s:append(ev) end)
                assert(not ok, "a caller wrote " .. ev.kind)
            end

            assert(s:len() == 2, "a rejected budget event was recorded: " .. tostring(s:len()))
            assert(s:remaining() == 10, "a forged event moved the balance")

            -- Reading them is fine: the kernel's own writes are in the log
            -- like everything else.
            s:reserve(4)
            local evs = s:events()
            assert(evs[3].kind == "budget_reserved" and evs[3].amount == 4,
                   "the kernel's own reservation is readable")
        "#,
        )
        .exec()
        .expect("kernel-only kind chunk");
    }

    /// (K4) `reserve` is the decision point: it takes what fits, refuses
    /// what does not without moving the balance, and names the grant when
    /// it refuses.  Every answer is a fact in the log.
    #[test]
    fn reserve_grants_refuses_and_records_both() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 100, tag = "tokens" } })

            local ok, tag = s:reserve(30)
            assert(ok == true, "a covered reservation must be granted")
            assert(tag == nil, "a granted reservation names no budget")
            assert(s:remaining() == 70, "remaining: " .. tostring(s:remaining()))

            local ok2, tag2 = s:reserve(1000)
            assert(ok2 == false, "an uncovered reservation must be refused")
            assert(tag2 == "tokens", "a refusal must name the budget: " .. tostring(tag2))
            assert(s:remaining() == 70, "a refusal must not deduct")
            assert(s:exhausted() == false, "a refusal must not exhaust")

            -- Both answers are in the log, with what was asked for.
            local evs = s:events()
            assert(evs[3].kind == "budget_reserved" and evs[3].amount == 30)
            assert(evs[3].tag == "tokens")
            assert(evs[4].kind == "budget_refused" and evs[4].amount == 1000)
            assert(evs[4].remaining == 70, "the refusal records what there was")

            -- Exactly the balance is coverable, and zero always is.
            assert(s:reserve(70) == true)
            assert(s:remaining() == 0 and s:exhausted() == true)
            assert(s:reserve(0) == true, "zero fits even at zero")
            assert(s:reserve(1) == false, "nothing fits past zero")
        "#,
        )
        .exec()
        .expect("reserve chunk");

        // Without a budget every reservation is granted, and nothing is
        // recorded: a run with no quota keeps no ledger.
        lua.load(
            r#"
            local s = knl.open()
            local ok, tag = s:reserve(999999)
            assert(ok == true and tag == nil, "no budget must grant everything")
            assert(s:len() == 1, "a run with no quota recorded a ledger event")
        "#,
        )
        .exec()
        .expect("no-budget reserve chunk");

        let msg = expect_err(
            &lua,
            r#"knl.open({ budget = { amount = 10 } }):reserve(-1)"#,
        );
        assert!(msg.contains("knl: reserve:"), "missing attribution: {msg}");
        assert!(msg.contains("non-negative"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open():reserve("many")"#);
        assert!(msg.contains("knl: reserve:"), "missing attribution: {msg}");
    }

    /// The counter is a cache of the log: folding `granted − reserved −
    /// spent` over what Lua can read reproduces `remaining()` exactly.
    #[test]
    fn the_balance_lua_reads_is_the_fold_of_the_ledger() {
        let lua = vm();
        lua.load(
            r#"
            local function folded(s)
                local balance = nil
                for _, ev in ipairs(s:events()) do
                    if ev.kind == "budget_granted" then
                        balance = (balance or 0) + ev.amount
                    elseif ev.kind == "budget_reserved" or ev.kind == "budget_spent" then
                        balance = math.max(0, balance - ev.amount)
                    end
                end
                return balance
            end

            local s = knl.open({ budget = { amount = 500, tag = "tokens" } })
            assert(folded(s) == s:remaining())

            s:reserve(120)
            s:append({ kind = "model_response",
                       content = { { type = "text", text = "hi" } },
                       usage = { input_tokens = 100, output_tokens = 50 } })
            s:spend(30)          -- the call overran its estimate
            s:reserve(10000)     -- refused, and moves nothing
            s:spend(0)

            assert(s:remaining() == 350, "remaining: " .. tostring(s:remaining()))
            assert(folded(s) == s:remaining(), "the fold and the counter disagree")

            -- The usage view is the other, independent reading.
            local u = s:view("usage")
            assert(u.input_tokens == 100 and u.output_tokens == 50)
            assert(u.model_calls == 1)
        "#,
        )
        .exec()
        .expect("fold chunk");
    }

    /// The run scope is `close`, not an event: appending `run_finished`
    /// writes a line and leaves the session open.
    #[test]
    fn an_appended_run_finished_does_not_close_the_run() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 100, tag = "tokens" } })
            s:append({ kind = "run_finished", reason = "carried over" })

            -- Still open: the writes a closed run refuses both go through.
            assert(s:append({ kind = "note" }) == 4, "the appended event ended the run")
            assert(s:spend(10) == 90)

            s:close("done")
            local evs = s:events()
            assert(kinds_of(s) ==
                   "run_started,budget_granted,run_finished,note,budget_spent,run_finished",
                   "recorded: " .. kinds_of(s))
            assert(evs[3].reason == "carried over" and evs[6].reason == "done",
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

    /// `store = "mem"` is the in-memory default spelled out; an unknown
    /// store string is a `knl: open:` error.
    #[test]
    fn store_mem_is_the_default_and_unknown_stores_are_rejected() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ store = "mem", owner = "x", budget = { amount = 10, tag = "tokens" } })
            assert(s:len() == 2, "mem store opens like the default: run_started + the grant")
            assert(s:owner() == "x")
            assert(s:append({ kind = "note" }) == 3)
        "#,
        )
        .exec()
        .expect("mem store chunk");

        let msg = expect_err(&lua, r#"knl.open({ store = "postgres" })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("unknown store"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open({ store = { redis = "x" } })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("sqlite"), "{msg}");
    }

    /// (Fix 6) The reserved owner ids are the kernel's own namespace: an
    /// untrusted Lua caller cannot claim "system" or "anon" (a spoofing hole
    /// for the future permission layer), each rejected as a `knl: open:` error.
    /// An unspecified owner still defaults to the kernel-assigned "anon", and a
    /// real principal id is accepted verbatim.
    #[test]
    fn open_rejects_reserved_owner_ids_from_the_caller() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.open({ owner = "system" })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("reserved"), "{msg}");
        assert!(msg.contains("system"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open({ owner = "anon" })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("reserved"), "{msg}");
        assert!(msg.contains("anon"), "{msg}");

        lua.load(
            r#"
            -- Unspecified owner is the kernel-assigned reserved anon.
            assert(knl.open():owner() == "anon", "default owner must be anon")
            assert(knl.open({}):owner() == "anon", "empty opts default owner must be anon")
            -- A real principal id is accepted verbatim.
            assert(knl.open({ owner = "alice" }):owner() == "alice", "owner not carried")
        "#,
        )
        .exec()
        .expect("reserved-owner chunk");
    }

    /// (durable) `knl.open({ store = { sqlite = path } })` writes to a
    /// persisted stream, and `knl.resume` reopens it and re-folds the
    /// record: owner, turn count, the balance the budget ledger implies and
    /// the usage view all come back, and the resumed run carries on from
    /// there.  A `budget` on resume is the owner granting again: recorded,
    /// and added to what was left.
    #[cfg(feature = "sqlite")]
    #[test]
    fn open_and_resume_a_durable_sqlite_session() {
        let lua = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path = path.to_str().expect("utf-8 path");

        lua.load(format!(
            r#"
            local path = "{path}"
            local s = knl.open({{ store = {{ sqlite = path }}, owner = "durable-user",
                                  budget = {{ amount = 100, tag = "tokens" }} }})
            s:reserve(30)
            s:append({{ kind = "model_response",
                        content = {{ {{ type = "text", text = "a" }} }},
                        usage = {{ input_tokens = 30 }} }})
            s:append({{ kind = "msg_user", content = "more" }})
            s:reserve(15)
            s:append({{ kind = "model_response",
                        content = {{ {{ type = "text", text = "b" }} }},
                        usage = {{ input_tokens = 20 }} }})
            s:spend(5)  -- the second call overran its estimate
            assert(s:turns() == 2, "open turns: " .. tostring(s:turns()))
            assert(s:remaining() == 50, "open remaining: " .. tostring(s:remaining()))
            local id = s:id()
            s:close()

            -- Reopen the same stream and continue where it left off.  No new
            -- grant: the balance is what the ledger says was left.
            local r = knl.resume({{ store = {{ sqlite = path }}, session = id }})
            assert(r:owner() == "durable-user", "resumed owner: " .. tostring(r:owner()))
            assert(r:turns() == 2, "resumed turns: " .. tostring(r:turns()))
            assert(r:remaining() == 50, "resumed remaining: " .. tostring(r:remaining()))
            assert(r:id() == id, "resumed id is the stream it reopened")
            local u = r:view("usage")
            assert(u.model_calls == 2, "resumed usage model_calls: " .. tostring(u.model_calls))
            assert(u.input_tokens == 50, "resumed usage input_tokens: " .. tostring(u.input_tokens))

            -- The grant's words came back too: a refusal still names it.
            local ok, tag = r:reserve(1000)
            assert(ok == false and tag == "tokens", "refused tag: " .. tostring(tag))

            -- Numbering and the ledger continue on the resumed session.
            r:reserve(5)
            r:append({{ kind = "model_response",
                        content = {{ {{ type = "text", text = "c" }} }},
                        usage = {{ input_tokens = 5 }} }})
            assert(r:turns() == 3, "continued turns: " .. tostring(r:turns()))
            assert(r:remaining() == 45, "continued remaining: " .. tostring(r:remaining()))
            r:close()

            -- Granting again on resume adds to what is left, and is recorded.
            local g = knl.resume({{ store = {{ sqlite = path }}, session = id,
                                    budget = {{ amount = 100, tag = "tokens",
                                                desc = "a second run" }} }})
            assert(g:remaining() == 145, "re-granted remaining: " .. tostring(g:remaining()))
            local evs = g:events()
            local last = evs[#evs]
            assert(last.kind == "budget_granted", "last event: " .. tostring(last.kind))
            assert(last.amount == 100 and last.desc == "a second run")
        "#
        ))
        .exec()
        .expect("durable open/resume chunk");
    }

    /// (owner namespace) resume holds the same reserved-principal line as
    /// open: a stream the host opened as SYSTEM cannot be reopened from
    /// Lua, or an untrusted caller could write into the reserved namespace
    /// through the resume side door.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_rejects_a_reserved_system_owned_stream() {
        let lua = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        // The host side (Rust) legitimately opens a SYSTEM-owned stream.
        let stream = "system-stream".to_string();
        let store = crate::knl::SqliteEventStore::open(&path, stream.clone()).expect("open store");
        let state =
            crate::knl::Session::open_on(crate::knl::SYSTEM.to_string(), None, Box::new(store))
                .expect("open system session");
        drop(state);

        // Lua resuming it is refused, exactly as claiming SYSTEM at open is.
        let msg = expect_err(
            &lua,
            &format!(
                r#"knl.resume({{ store = {{ sqlite = "{path_str}" }}, session = "{stream}" }})"#
            ),
        );
        assert!(
            msg.contains("reserved"),
            "must name the reserved owner: {msg}"
        );
    }

    /// (attribution) resume needs a sqlite store and a session id; each
    /// missing piece is a `knl: resume:` error.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_requires_a_sqlite_store_and_a_session_id() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.resume()"#);
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");

        let msg = expect_err(&lua, r#"knl.resume({ session = "s1" })"#);
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");
        assert!(msg.contains("sqlite"), "{msg}");

        let msg = expect_err(&lua, r#"knl.resume({ store = { sqlite = "/tmp/x.db" } })"#);
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");
        assert!(msg.contains("session is required"), "{msg}");
    }

    // -- run scope: `<close>` and the drop backstop ------------------------
    //
    // Every one of these reads the boundary back out of a *reopened* SQLite
    // stream rather than off the handle that wrote it: the question is
    // whether the record landed, and only the durable log answers that.

    /// The persisted events of `stream`, read through a fresh connection.
    #[cfg(feature = "sqlite")]
    fn persisted(path: &std::path::Path, stream: &str) -> Vec<Value> {
        use crate::knl::EventStore;

        let store = crate::knl::SqliteEventStore::open(path, stream).expect("reopen the stream");
        store.read(0, usize::MAX).expect("read the stream")
    }

    /// Run `chunk` in a fresh VM and return the session id it yields.
    ///
    /// The VM is dropped before the caller reads the stream, so anything the
    /// collector still owed has been paid by the time the log is inspected.
    #[cfg(feature = "sqlite")]
    fn stream_id_from(chunk: String) -> String {
        let lua = vm();
        lua.load(chunk).eval::<String>().expect("run scope chunk")
    }

    /// (I6) A `<close>` scope that ends cleanly records the run's boundary
    /// with `scope_exit`: the shell no longer has to remember to close.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_close_scope_records_the_boundary_on_the_way_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        let id = stream_id_from(format!(
            r#"
            local id
            do
                local s <close> = knl.open({{ store = {{ sqlite = "{path_str}" }}, owner = "t" }})
                id = s:id()
                s:append({{ kind = "note" }})
                assert(s:len() == 2, "inside the scope: run_started + note")
            end
            return id
        "#
        ));

        let log = persisted(&path, &id);
        let last = log.last().expect("the stream is not empty");
        assert_eq!(last["kind"], Value::from("run_finished"), "{last}");
        assert_eq!(last["reason"], Value::from("scope_exit"), "{last}");
        assert_eq!(last.get("detail"), None, "a clean exit has nothing to say");
        assert_eq!(log.len(), 3, "run_started + note + run_finished");
    }

    /// (I6) A block that raises closes its session too, with `error` as the
    /// reason and the message as `detail` — so the log says the run ended
    /// badly without the reason vocabulary growing a member per failure.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_close_scope_that_raises_records_the_error_and_its_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        let id = stream_id_from(format!(
            r#"
            local id
            local ok, msg = pcall(function()
                local s <close> = knl.open({{ store = {{ sqlite = "{path_str}" }}, owner = "t" }})
                id = s:id()
                error("boom")
            end)
            assert(not ok, "the block was supposed to fail")
            assert(tostring(msg):find("boom"), "the error is still the caller's: " .. tostring(msg))
            return id
        "#
        ));

        let log = persisted(&path, &id);
        let last = log.last().expect("the stream is not empty");
        assert_eq!(last["kind"], Value::from("run_finished"), "{last}");
        assert_eq!(last["reason"], Value::from("error"), "{last}");
        let detail = last["detail"].as_str().expect("detail text");
        assert!(detail.contains("boom"), "detail: {detail}");
    }

    /// (I6) An explicit `close` wins: the scope exit that follows it is a
    /// no-op, so the reason in the log is the caller's and there is exactly
    /// one boundary.
    #[cfg(feature = "sqlite")]
    #[test]
    fn an_explicit_close_wins_over_the_scope_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        let id = stream_id_from(format!(
            r#"
            local id
            do
                local s <close> = knl.open({{ store = {{ sqlite = "{path_str}" }}, owner = "t" }})
                id = s:id()
                s:close("done")
            end
            return id
        "#
        ));

        let log = persisted(&path, &id);
        let finished: Vec<&Value> = log.iter().filter(|e| e["kind"] == "run_finished").collect();
        assert_eq!(finished.len(), 1, "exactly one boundary: {log:?}");
        assert_eq!(finished[0]["reason"], Value::from("done"));
    }

    /// (I6) The backstop: a handle that goes out of scope with no `<close>`
    /// and no explicit close still records the boundary when the collector
    /// reclaims it.  A run that ends by being forgotten is still an ended
    /// run, and a reader of the stream must not see it as open forever.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_collected_handle_records_the_boundary_as_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        let id = stream_id_from(format!(
            r#"
            -- Opened inside a function so the handle is unreachable the
            -- moment it returns: nothing holds the userdata but the
            -- collector.
            local function run()
                local s = knl.open({{ store = {{ sqlite = "{path_str}" }}, owner = "t" }})
                s:append({{ kind = "note" }})
                return s:id()
            end
            local id = run()
            collectgarbage("collect")
            collectgarbage("collect")
            return id
        "#
        ));

        let log = persisted(&path, &id);
        let last = log.last().expect("the stream is not empty");
        assert_eq!(
            last["kind"],
            Value::from("run_finished"),
            "the collector left the run open: {log:?}"
        );
        assert_eq!(last["reason"], Value::from("dropped"), "{last}");
    }
}
