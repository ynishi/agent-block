//! `knl.*` — Lua surface of the kernel syscall layer.
//!
//! This module is an adapter, nothing more.  The domain rules live in
//! [`crate::knl`] (pure Rust, unit-tested without a VM); here we only:
//!
//! 1. define the `Session` userdata and bind its methods,
//! 2. convert Lua tables ⇄ `serde_json::Value`,
//! 3. attribute failures as `knl: <method>: <kind>: <reason>`.
//!
//! # Failures carry their class
//!
//! A failure is raised as a message, because mlua raises every error a Rust
//! callback returns as its own userdata and offers no way to make a Lua table
//! *be* the raised value.  So the message is given a shape instead: the first
//! three fields are a closed vocabulary — the prefix, the method, and the
//! class ([`knl::KnlError::KINDS`]) — and only the fourth is prose.
//! `knl.error(e)` reads it back as `{ kind, method, retryable, message }`,
//! and `knl.api().errors` publishes the class list so the shell's own
//! declaration of it can be checked rather than trusted.  A caller that only
//! wants to print keeps working: the table renders as the message it came
//! from, and the message still contains what it always did.
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
//! - **An event is envelope + meta + data.**  What you append is
//!   `{ kind = …, beat? = …, meta? = { … }, data? = { … } }` and nothing
//!   else: a top-level key outside that set is refused, because a kind's own
//!   fields belong under `data`.  `meta` is shallow — string, number or
//!   boolean values — so a reader can group or filter on it without knowing
//!   the kind; `data` is yours, at any depth, and the shape under it is
//!   declared where the kind is written (`knl.shapes`).  The kernel checks
//!   the `data` of its own six kinds (`session_*` / `budget_*`) and of no
//!   others.  Both come back the way they went in, `data` defaulting to an
//!   empty table when you write none.
//! - **I1 append-only.**  There is deliberately no `update`, `delete` or
//!   `replace`.  `events()` / `view()` hand back freshly built tables, so
//!   a caller that mutates a returned value cannot reach recorded state.
//!   `seq` and `epoch_ms` are assigned by the kernel and overwrite any
//!   caller-supplied field of the same name.  Nothing else is added: a
//!   `beat` the caller declared is stored exactly as given.
//! - **A session has a scope.**  The two are different things sharing one
//!   lifetime: the session is the stream (`s:id()`), the scope is the
//!   authority it is written under — a kernel-issued scope id (`s:scope_id()`)
//!   and the principal it belongs to (`s:owner()`, a real id or the reserved
//!   "anon" / "system").  Neither id is a caller's to choose, and they are
//!   not each other: `s:id()` names the stream a `knl.resume` reopens.  The
//!   scope id is recorded on `session_opened` and on every `budget_*` event,
//!   so the boundary is readable — and unforgeable — from the log alone.
//!   There is no per-event author: a session holds only its own events, so
//!   an accounting of what a run consumed keys on the `kind` alone — every
//!   `llm_response` in the log is a call this run made.
//! - **Beats are yours.**  `knl.new_beat_id()` mints a time-ordered id; you
//!   stamp it on the `llm_response`, `tool_call` and `tool_result` events
//!   that belong to one beat.  The kernel does not number beats, does not
//!   require the field, and asks only that a `beat` you do write is a
//!   string.
//! - **I3 budget monotonicity.**  The budget is a quota an owner grants
//!   the session, not a ledger of what it used.  `reserve(n)` asks *before*
//!   spending — it deducts and returns `true`, or refuses with `false,
//!   tag` and leaves the balance exactly as it was — and `spend(n)`
//!   settles afterwards, returning nothing: the settlement landing *is* the
//!   answer, and `remaining()` is the separate question of what is left.
//!   Both take non-negative whole amounts and the balance can only decrease;
//!   there is no API to raise or reset it, and no `append` moves it.  What a
//!   session actually consumed is read off the responses it recorded
//!   (`knl.views.usage`, a query view in Lua), independent of the balance.
//! - **I6 session lifecycle.**  All state lives inside the userdata — no
//!   module-level statics, no Lua globals — so two sessions are fully
//!   independent.  There is no "run" inside a session: `knl.open()` records
//!   `session_opened` and the grant, so the log says what was allowed, and
//!   `close` records `session_closed`.  Both events are the kernel's alone —
//!   appending either by hand is an error — so the lifecycle in the log is
//!   the lifecycle that happened.  Three paths reach
//!   the closing boundary and the log never loses it: `close(reason?)` said
//!   explicitly; a `local s <close> = knl.open{...}` scope ending, which
//!   records `scope_exit` — or `error` with the message in `detail` when the
//!   block raised; and the drop backstop, `dropped`, for a handle nobody
//!   closed.  Whichever runs first wins and the rest are no-ops, because
//!   `close` is idempotent — so an explicit reason is never overwritten by
//!   the scope or the collector that follows it.
//! - **Closed is the handle's, not the stream's.**  `closed` is a flag on
//!   *this* userdata: after it, this handle's `append` / `reserve` / `spend`
//!   are errors while its reads keep working.  The log turns nothing away.
//!   A write from another handle that never saw the ending is recorded after
//!   the `session_closed`, because that is what happened and it is exactly
//!   what an audit is reading for; two handles that both close leave two
//!   endings, not one.  One reader consults `session_closed` at all —
//!   `knl.resume`, which refuses a stream whose ending is already in the log,
//!   because a session is disposable.
//! - **K2 model call.**  There is no composite call and the session keeps
//!   no backend of its own.  The driver reserves what it estimates the
//!   call will cost, calls the backend itself, appends the `llm_response`,
//!   and settles the difference with `spend`.
//!
//! ```lua
//! local s = knl.open({
//!     owner  = "user-42",              -- default: the reserved "anon"
//!     budget = { amount = 10000, tag = "tokens" },
//! })
//! s:append({ kind = "msg_user", data = { content = "hi" } })
//! -- Drive the beat yourself: name it, ask for the estimate first, call
//! -- the backend, append the response, then settle what it really cost.
//! local beat = knl.new_beat_id()
//! local ok, tag = s:reserve(est)
//! if not ok then return { budget_stopped = true, tag = tag } end
//! s:append({ kind = "llm_response", beat = beat,
//!            data = { content = blocks, usage = u } })
//! s:spend(math.max(actual - est, 0))     -- the settlement
//! local events = s:events(from)          -- the record, from `from` on
//! local tail   = s:view("tail", { n = 5 })  -- the last events, verbatim
//! s:close("done")
//! ```
//!
//! Two named read faces and no more: the events, and the last of the record.
//! Turning events into a request for a provider — which role a kind takes,
//! whether a system message goes in front, where to cut the history off — is
//! the shell's policy, so it is written in Lua over `events(from)` rather
//! than named as a view here.  The token account went the same way: it reads
//! the `usage` the adapter recorded on each `llm_response`, which is the
//! shell's vocabulary, so it is a query view in Lua (`knl.views.usage`) and
//! not a name the kernel answers to.
//!
//! The third face is not a name but a language: `s:query(sql, params?,
//! opts?)` reads the log with one `SELECT` / `WITH` over the table the events
//! live in, whose columns `knl.api().schema` publishes.  That is what keeps
//! the list of names short — a fold the kernel has no opinion about is a
//! query, not a name it had to be taught.  `$stream` binds to this session
//! and `$sessions` to the set in `opts.sessions`, so reading across a tree of
//! sessions is one statement.  Values are bound, never pasted; the connection
//! it runs on cannot write; and it returns `rows, truncated`, so a page can
//! be told from a complete answer.
//!
//! Storage backend.  `knl.open` takes an optional `store`: absent or `"mem"`
//! is an in-memory SQLite database that lives as long as the session does,
//! while `{ sqlite = "<path>" }` is a durable, per-session stream in a file.
//! One backend, two kinds of database — the log is a table either way, which
//! is what `s:query` reads.  `knl.resume({ store = …, session = "<id>",
//! budget? })` reopens a stream and re-folds it, so a resumed session's
//! accounting continues from the recorded state — it behaves exactly like a
//! fresh session, only pre-loaded.  A file survives the process; an in-memory
//! database does not, so resuming one is only possible while another handle
//! still holds it open.
//!
//! Scope: effect execution and the Lua-side projection seam are separate
//! steps of the kernel/shell base design.

use std::cell::RefCell;

use mlua::prelude::*;
use serde_json::{Map, Value};

use super::{json_to_lua, lua_to_json};
use crate::knl;

/// Every method the session userdata answers to, with the contract it holds
/// in one line.
///
/// The declared surface: what Lua can reach on a session is this table and
/// nothing else, and a test below reflects over a live userdata to hold that
/// (a method registered without an entry here fails it).  `knl.api()` hands
/// the same list to Lua, so a caller can ask what a session offers instead of
/// guessing.
/// Each doc names the classes that method can raise (`knl.error(e).kind`,
/// one of `knl.api().errors`), so a caller reads what it has to handle from
/// the same table it reads the signature from.  `busy` / `storage` /
/// `corruption` are the durable backend's and never occur on the in-memory
/// one; a method that only reads its own value raises nothing at all.
pub const SESSION_API: &[(&str, &str)] = &[
    ("id", "id() -> string — the stream this session writes"),
    (
        "scope_id",
        "scope_id() -> string — the authority the stream is written under",
    ),
    (
        "owner",
        "owner() -> string — the principal the scope belongs to (or \"anon\" / \"system\")",
    ),
    (
        "append",
        "append(event) -> seq — record a fact: { kind, beat?, meta? (shallow), data? }; a key \
         outside that envelope, a kernel-only kind and a nested meta are refused, and the budget \
         does not move [raises: validation, closed, busy, storage]",
    ),
    (
        "events",
        "events(from?) -> array — the record from `from` on, as fresh tables in the shape it was \
         written in (kind / beat / meta / data, plus the kernel's stamps) \
         [raises: busy, storage, corruption]",
    ),
    (
        "len",
        "len() -> integer — how many events are recorded [raises: busy, storage]",
    ),
    (
        "view",
        "view(name, opts?) -> table — the one named fold: \"tail\" { n }; anything else is a \
         query [raises: validation, busy, storage, corruption]",
    ),
    (
        "query",
        "query(sql, params?, opts?) -> rows, truncated — read the log with one SELECT / WITH; \
         $stream is this session, $sessions is opts.sessions (default { this session }) \
         [raises: validation, busy, storage, corruption, timeout]",
    ),
    (
        "reserve",
        "reserve(n) -> true | false, tag — refuse if remaining < n, atomic, both answers recorded \
         [raises: validation, closed, busy, storage, corruption]",
    ),
    (
        "spend",
        "spend(n) -> nil — settle after the fact; the write is the answer, read the balance \
         with remaining() [raises: validation, closed, busy, storage]",
    ),
    (
        "remaining",
        "remaining() -> integer | nil — the balance, nil without a budget; a store that cannot be \
         read raises rather than reporting a stale one [raises: busy, storage, corruption]",
    ),
    (
        "exhausted",
        "exhausted() -> boolean — whether the budget is used up (false without one) \
         [raises: busy, storage, corruption]",
    ),
    (
        "close",
        "close(reason?, detail?) -> nil — record session_closed and end the session; idempotent \
         [raises: validation, busy, storage]",
    ),
    (
        "__close",
        "__close(err) — the <close> scope boundary: scope_exit, or error with the message as detail \
         [raises: busy, storage — only on a clean exit; an unwinding one is logged]",
    ),
];

/// Every function the `knl` global carries, with its one-line contract.
///
/// The module half of the declared surface, held by the same test, and
/// annotated with the error classes each can raise like [`SESSION_API`].
pub const MODULE_API: &[(&str, &str)] = &[
    (
        "open",
        "open(opts?) -> session — owner? / budget? / store? (\"mem\" for an in-memory database, \
         or { sqlite = path }) [raises: validation, busy, storage]",
    ),
    (
        "resume",
        "resume(opts) -> session — reopen a stream and re-fold it; a closed session is not \
         resumable [raises: validation, closed, busy, storage, corruption]",
    ),
    (
        "new_beat_id",
        "new_beat_id() -> string — mint a time-ordered beat id for the caller to stamp",
    ),
    (
        "error",
        "error(err) -> { kind, method, retryable, message } — read a raised failure as a table; \
         an unrecognised one comes back with kind = nil and the whole text as message",
    ),
    (
        "api",
        "api() -> { session = …, module = …, errors = { kind }, schema = { table, columns } } — \
         the declared surface, and the columns a query may name",
    ),
];

/// K5 session: the only handle the Lua side has on kernel state.
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

    /// Open a session for `owner` with an optional budget grant, on the
    /// in-memory store.
    fn new(owner: String, grant: Option<knl::BudgetGrant>) -> LuaResult<Self> {
        let state = knl::Session::new(owner, grant).map_err(|e| knl_err("open", &e))?;
        Ok(Self::from_state(state))
    }
}

/// The backstop under `close` and `<close>`: a handle nobody ended still
/// records the session's boundary, here, where the value dies.
///
/// A dropped handle is the one close path with no caller left to tell, so
/// it cannot fail loudly the way the other two do: a failed append is a
/// `warn!` and nothing else.  Panicking in `drop` would abort the process
/// (a Lua collection cycle is not a place to unwind from), and a session
/// already past its last reader is not worth that.  What the boundary
/// costs is one line in the log; what it buys is a resumed or audited
/// stream that is not silently open forever.
impl Drop for Session {
    fn drop(&mut self) {
        // `try_borrow_mut`, not `borrow_mut`: a panic unwinding out of a
        // method leaves the borrow live, and this runs during that unwind.
        let Ok(mut state) = self.state.try_borrow_mut() else {
            tracing::warn!("knl: session dropped while borrowed; session_closed was not recorded");
            return;
        };
        if state.is_closed() {
            return;
        }
        if let Err(e) = state.close(Some(knl::CLOSE_REASON_DROPPED)) {
            tracing::warn!(
                session = %state.id(),
                error = %e,
                "knl: dropped session could not record session_closed"
            );
        }
    }
}

/// The longest `detail` a close records, in characters.
///
/// A `session_closed` says why a session ended, and an error message can be
/// a whole traceback; the cap keeps one bad turn from putting a page into
/// the log.  Counted in `chars` so the cut never lands inside one.
const DETAIL_MAX_CHARS: usize = 200;

/// `text` cut to [`DETAIL_MAX_CHARS`], with an ellipsis when it was cut.
///
/// One rule for both close paths: what `<close>` records off a raised error
/// and what a caller passes to `close(reason, detail)` are capped the same
/// way, so the log cannot grow a page-long entry from either side.
fn truncated(text: &str) -> String {
    if text.chars().count() <= DETAIL_MAX_CHARS {
        return text.to_string();
    }
    text.chars().take(DETAIL_MAX_CHARS).collect::<String>() + "..."
}

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
    truncated(&text)
}

/// An optional string argument of `close`: absent, or a string.
///
/// `field` names it (`reason` / `detail`) so the refusal says which of the
/// two was wrong.
fn close_text(field: &str, value: LuaValue) -> LuaResult<Option<String>> {
    match value {
        LuaValue::Nil => Ok(None),
        LuaValue::String(text) => Ok(Some(text.to_str()?.to_string())),
        other => Err(err(
            "close",
            format!("{field} must be a string, got {}", other.type_name()),
        )),
    }
}

/// The `knl: <method>: <kind>: <reason>` attribution, as text.
///
/// Four fields in a fixed order, and the first three are a closed
/// vocabulary: the prefix, the method the caller invoked, and the class of
/// the failure ([`knl::KnlError::KINDS`]).  Only the fourth is prose.  That
/// is what lets [`error_table`] hand the same four fields back as a table
/// without the Lua side matching on a sentence that is meant to change.
fn attributed(method: &str, kind: &str, reason: impl std::fmt::Display) -> String {
    format!("knl: {method}: {kind}: {reason}")
}

/// Build a `knl:`-attributed error of `kind` for `method`.
fn err_of(method: &str, kind: &str, reason: impl std::fmt::Display) -> LuaError {
    LuaError::external(attributed(method, kind, reason))
}

/// The bridge refusing what it was handed: a `validation` failure.
///
/// Every refusal raised on this side of the boundary — a non-table event, a
/// misspelt budget field, an amount that is not a whole number — is the
/// caller's arguments not holding up, which is the same class the kernel
/// gives its own validator's refusals.  So the shell sees one vocabulary,
/// whether the check ran in Rust's kernel or in its adapter.
fn err(method: &str, reason: impl std::fmt::Display) -> LuaError {
    err_of(method, knl::KnlError::VALIDATION, reason)
}

/// A kernel failure, carrying the kernel's own classification outwards.
///
/// The bridge does not re-decide what went wrong: [`knl::KnlError::kind`]
/// already said, and this only renders it.
fn knl_err(method: &str, error: &knl::KnlError) -> LuaError {
    err_of(method, error.kind(), error.reason())
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

        // s:scope_id() -> string
        //
        // The kernel-issued id of the scope this session is written under,
        // as recorded on `session_opened` and on every `budget_*` event.
        // Not `s:id()`: that names the stream, this names the authority the
        // stream is written under, and neither is a caller's to choose.
        methods.add_method("scope_id", |_, this, ()| {
            Ok(this.state.borrow().scope_id().to_string())
        });

        // s:owner() -> string
        //
        // The principal the scope belongs to (a real id, or the reserved
        // "anon" / "system").  Total — never nil.
        methods.add_method("owner", |_, this, ()| {
            Ok(this.state.borrow().owner().to_string())
        });

        // s:append(event) -> seq
        //
        // K1: the only way to add to the history, and there is no way to
        // change what is already in it.  The event is the envelope
        // (`kind` / `beat?` / `meta?` / `data?`) and nothing beside it; what
        // is under `data` is recorded as written, `beat` included, and the
        // kernel adds `seq` / `epoch_ms` and an empty `data` when there was
        // none.  No append touches the budget — that is `reserve` before the
        // call and `spend` after it — and the two `session_*` kinds are
        // refused here, since only `knl.open` / `close` write those.
        methods.add_method("append", |lua, this, event: LuaValue| {
            let obj = table_to_object(lua, "append", "event", event)?;
            this.state
                .borrow_mut()
                .append(obj)
                .map_err(|e| knl_err("append", &e))
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
                .map_err(|e| knl_err("events", &e))?;
            // The events come out of the kernel as `Current` — the proof that
            // they were read through the upcaster seam — and that proof stops
            // at this boundary: what Lua gets is a table, so the objects are
            // taken back out here, at the one place they leave the kernel.
            let selected: Vec<Value> = selected
                .into_iter()
                .map(|event| Value::Object(event.into_inner()))
                .collect();
            // The borrow is released above: json_to_lua re-enters Lua.
            json_to_lua(lua, Value::Array(selected))
        });

        // s:len() -> number of recorded events
        methods.add_method("len", |_, this, ()| {
            let n = this.state.borrow().len().map_err(|e| knl_err("len", &e))?;
            Ok(n as u64)
        });

        // s:view(name, opts?) -> projection (fresh table each call)
        //
        // `tail` (`opts.n` events from the end), and that is the whole
        // vocabulary: an unknown name is an error, because a projection the
        // kernel does not name is the shell's to build — from
        // `events(from)`, or as a query view over the published schema.
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
                .map_err(|e| knl_err("view", &e))?;
            // The borrow is released above: json_to_lua re-enters Lua.
            json_to_lua(lua, value)
        });

        // s:query(sql, params?, opts?) -> rows, truncated
        //
        // The log read with SQL.  `view` names the folds whose consumer is
        // the kernel's own; everything else — beats grouped, tool calls
        // paired with their results, a ledger — is a SELECT over the table
        // the events live in, whose columns `knl.api().schema` publishes.
        //
        // What the kernel keeps around it: one statement and it reads, a
        // connection that cannot write, values bound rather than pasted, a
        // deadline, a row cap.  The second return says whether the cap cut
        // anything off, so a caller can tell a complete answer from a page.
        methods.add_method(
            "query",
            |lua, this, (sql, params, opts): (LuaValue, LuaValue, LuaValue)| {
                let LuaValue::String(sql) = sql else {
                    return Err(err(
                        "query",
                        format!("sql must be a string, got {}", sql.type_name()),
                    ));
                };
                let sql = sql.to_str()?.to_string();
                // Both conversions happen before the session is borrowed:
                // walking a Lua table can re-enter Lua.
                let params = query_params(lua, params)?;
                let opts = query_opts(lua, opts)?;

                let found = this
                    .state
                    .borrow()
                    .query(&sql, params, &opts)
                    .map_err(|e| knl_err("query", &e))?;
                let rows: Vec<Value> = found.rows.into_iter().map(Value::Object).collect();
                // The borrow is released above: json_to_lua re-enters Lua.
                let rows = json_to_lua(lua, Value::Array(rows))?;
                Ok((rows, found.truncated))
            },
        );

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
            let granted = state.reserve(amount).map_err(|e| knl_err("reserve", &e))?;
            // The tag rides along only on a refusal: it answers "which
            // budget stopped you", which is a question only then.
            let tag = if granted {
                None
            } else {
                state.grant().and_then(|grant| grant.tag.clone())
            };
            Ok((granted, tag))
        });

        // s:spend(n) — the settlement after a reservation.
        //
        // K4.  Non-negative amounts only, and the balance never rises.
        //
        // It returns nothing.  It used to hand back the balance it read
        // afterwards, which made a settlement that landed and then failed its
        // read-back indistinguishable from one that never landed — the caller
        // saw an error either way and could not tell whether the `budget_spent`
        // was in the log.  Two questions, two calls: this one raises only if
        // the write itself failed, and `s:remaining()` answers the other.
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
            this.state
                .borrow_mut()
                .spend(amount)
                .map_err(|e| knl_err("spend", &e))
        });

        // s:remaining() -> number or nil (no budget)
        //
        // Raises when the ledger cannot be read: a store that is down has no
        // balance to report, and both values this could otherwise return —
        // a stale number, or the nil that means "no budget here" — read as
        // facts a run would carry on spending against.
        methods.add_method("remaining", |_, this, ()| {
            this.state
                .borrow()
                .remaining()
                .map_err(|e| knl_err("remaining", &e))
        });

        // s:exhausted() -> boolean (always false without a budget)
        //
        // Raises for the same reason `remaining` does: a `false` that meant
        // "the store could not be read" is the one answer a run must never
        // be handed, because it reads as "carry on".
        methods.add_method("exhausted", |_, this, ()| {
            this.state
                .borrow()
                .exhausted()
                .map_err(|e| knl_err("exhausted", &e))
        });

        // s:close(reason?, detail?) — records `session_closed` and ends the
        // session.  Idempotent.
        //
        // The reason says *which kind of ending* this was and stays a short
        // vocabulary a reader can fold on; the optional `detail` is the
        // sentence only this close can tell — the message of the error a
        // caller's own bracket caught, say.  Keeping them apart is what stops
        // every distinct error message from becoming its own reason, and it
        // is the same split the `<close>` path records.  `detail` is truncated
        // exactly as that path truncates it.
        methods.add_method(
            "close",
            |_, this, (reason, detail): (LuaValue, LuaValue)| {
                let reason = close_text("reason", reason)?;
                let detail = close_text("detail", detail)?.map(|text| truncated(&text));
                // A close whose `session_closed` append fails (a database
                // contended past its retries, a store that is gone) surfaces
                // here: the session stays open and the caller knows the
                // boundary was not recorded, instead of a silent closed=true
                // with no record.
                this.state
                    .borrow_mut()
                    .close_with(reason.as_deref(), detail.as_deref())
                    .map_err(|e| knl_err("close", &e))?;
                Ok(())
            },
        );

        // __close(self, err) — the Lua 5.4 to-be-closed variable:
        //
        //     do
        //         local s <close> = knl.open({ owner = "u" })
        //         ...
        //     end   -- the session's boundary is recorded here
        //
        // The reason says how the scope ended, not what went wrong: a clean
        // exit is "scope_exit", an unwinding one "error", with the message
        // in `detail`.  Folding the message into the reason would make every
        // distinct failure its own reason and the vocabulary unreadable.
        //
        // An explicit `close` earlier in the block already ended the session,
        // and this is a no-op then: the caller's reason is the one in the log.
        //
        // What a failed append does depends on whether there is already an
        // error on its way out:
        //
        // - clean exit (`err` is nil): raise, exactly as `close` does, since
        //   a close that reports success with no `session_closed` recorded
        //   is the one outcome the boundary exists to rule out;
        // - unwinding (`err` is non-nil): do *not* raise.  Lua would replace
        //   the body's error with this one, and the body's error is what the
        //   caller is trying to diagnose — a bookkeeping failure must not
        //   overwrite the failure it is bookkeeping for.  It goes to the log
        //   as a `warn!` and the original error propagates unchanged.
        methods.add_meta_method(LuaMetaMethod::Close, |_, this, error: LuaValue| {
            if this.state.borrow().is_closed() {
                return Ok(());
            }
            // Computed before the borrow: nothing about the error value is
            // read while the session is held.
            let unwinding = !matches!(error, LuaValue::Nil);
            let (reason, detail) = match error {
                LuaValue::Nil => (knl::CLOSE_REASON_SCOPE_EXIT, None),
                error => (knl::CLOSE_REASON_ERROR, Some(error_detail(&error))),
            };
            let outcome = this
                .state
                .borrow_mut()
                .close_with(Some(reason), detail.as_deref());
            match outcome {
                Ok(()) => Ok(()),
                Err(e) if unwinding => {
                    tracing::warn!(
                        session = %this.state.borrow().id(),
                        error = %e,
                        "knl: session_closed was not recorded; \
                         the block's own error is propagating instead"
                    );
                    Ok(())
                }
                Err(e) => Err(knl_err("close", &e)),
            }
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

/// Read `opts.budget` into the session's grant: `{ amount, tag?, desc? }`.
///
/// `amount` is the quota and the only field the kernel interprets; `tag`
/// names its unit and `desc` records what was allowed and why, both of
/// which ride onto `budget_granted` verbatim.  An unknown field is an error
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
            // reserved principal on `session_opened`.  Compared against the consts,
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

/// The fields a `query` opts table may carry.  Anything else is a typo, and
/// a misspelt `limit` or `timeout_ms` that reads as "no cap" / "no deadline"
/// is exactly what those two exist to prevent.
const QUERY_OPT_FIELDS: [&str; 3] = ["sessions", "timeout_ms", "limit"];

/// Read the `params` argument of `s:query`.
///
/// A list is the values for the `?` parameters, in order; a table with names
/// is the values for `:name` / `@name` / `$name`.  An absent or empty table
/// is neither: the statement is expected to have no parameters of its own.
///
/// Converted before the session is borrowed — walking a Lua table can call
/// back into Lua.
fn query_params(lua: &Lua, params: LuaValue) -> LuaResult<knl::QueryParams> {
    match params {
        LuaValue::Nil => Ok(knl::QueryParams::None),
        table @ LuaValue::Table(_) => match lua_to_json(lua, table).map_err(|e| err("query", e))? {
            Value::Array(values) => Ok(knl::QueryParams::Positional(values)),
            Value::Object(named) if named.is_empty() => Ok(knl::QueryParams::None),
            Value::Object(named) => Ok(knl::QueryParams::Named(named)),
            _ => Err(err("query", "params must be a table")),
        },
        other => Err(err(
            "query",
            format!(
                "params must be a list or a table of names, got {}",
                other.type_name()
            ),
        )),
    }
}

/// Read the `opts` argument of `s:query`: `{ sessions?, timeout_ms?, limit? }`.
fn query_opts(lua: &Lua, opts: LuaValue) -> LuaResult<knl::QueryOpts> {
    let opts = match opts {
        LuaValue::Nil => return Ok(knl::QueryOpts::default()),
        LuaValue::Table(table) => table,
        other => {
            return Err(err(
                "query",
                format!("opts must be a table, got {}", other.type_name()),
            ));
        }
    };

    for pair in opts.clone().pairs::<LuaValue, LuaValue>() {
        let (key, _) = pair?;
        let LuaValue::String(name) = &key else {
            return Err(err(
                "query",
                format!("opts fields must be named, got a {}", key.type_name()),
            ));
        };
        let name = name.to_str()?.to_string();
        if !QUERY_OPT_FIELDS.contains(&name.as_str()) {
            return Err(err(
                "query",
                format!("unknown query option {name:?} (expected sessions / timeout_ms / limit)"),
            ));
        }
    }

    let sessions = match opts.get::<LuaValue>("sessions")? {
        LuaValue::Nil => None,
        table @ LuaValue::Table(_) => {
            match lua_to_json(lua, table).map_err(|e| err("query", e))? {
                Value::Array(ids) => Some(
                    ids.into_iter()
                        .map(|id| match id {
                            Value::String(id) => Ok(id),
                            other => Err(err(
                                "query",
                                format!("opts.sessions must be a list of session ids, got {other}"),
                            )),
                        })
                        .collect::<LuaResult<Vec<String>>>()?,
                ),
                // An empty table is an empty set, which the kernel refuses
                // with its own words rather than being read as "all of them".
                Value::Object(named) if named.is_empty() => Some(Vec::new()),
                _ => {
                    return Err(err("query", "opts.sessions must be a list of session ids"));
                }
            }
        }
        other => {
            return Err(err(
                "query",
                format!(
                    "opts.sessions must be a list of session ids, got {}",
                    other.type_name()
                ),
            ));
        }
    };

    let timeout_ms = match opts.get::<LuaValue>("timeout_ms")? {
        LuaValue::Nil => knl::DEFAULT_TIMEOUT_MS,
        value => as_whole_non_negative(&value)
            .map(|n| n as u64)
            .ok_or_else(|| {
                err(
                    "query",
                    format!(
                        "opts.timeout_ms must be a non-negative whole number, got {}",
                        lua_value_for_msg(&value)
                    ),
                )
            })?,
    };
    let limit = match opts.get::<LuaValue>("limit")? {
        LuaValue::Nil => knl::DEFAULT_LIMIT,
        value => as_whole_non_negative(&value)
            .map(|n| n as usize)
            .ok_or_else(|| {
                err(
                    "query",
                    format!(
                        "opts.limit must be a non-negative whole number, got {}",
                        lua_value_for_msg(&value)
                    ),
                )
            })?,
    };

    Ok(knl::QueryOpts {
        sessions,
        timeout_ms,
        limit,
    })
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
fn open_sqlite(owner: String, grant: Option<knl::BudgetGrant>, path: &str) -> LuaResult<Session> {
    let stream = uuid::Uuid::new_v4().to_string();
    let store = knl::SqliteEventStore::open(std::path::Path::new(path), stream.clone())
        .map_err(|e| knl_err("open", &e))?;
    let mut state =
        knl::Session::open_on(owner, grant, Box::new(store)).map_err(|e| knl_err("open", &e))?;
    state.adopt_id(stream);
    Ok(Session::from_state(state))
}

/// Reopen the stream `session_id` and resume it.
///
/// `store` is the backend the stream lives in — a file, or the in-memory
/// database of that name while some handle still holds it open.
/// `Session::resume` re-folds the log; the reopened stream's id is adopted so
/// `s:id()` matches the stream the caller named.
fn resume_on(
    grant: Option<knl::BudgetGrant>,
    store: knl::SqliteEventStore,
    session_id: String,
) -> LuaResult<Session> {
    // Resumed with no grant, so nothing has been written yet when the check
    // below runs: a refused resume must leave the stream exactly as it found
    // it, and a `budget_granted` recorded before the refusal would be the
    // caller writing into a stream it was not allowed to touch.
    let mut state =
        knl::Session::resume(None, Box::new(store)).map_err(|e| knl_err("resume", &e))?;
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
    // The stream passed: now the owner's fresh grant is recorded, adding to
    // what the ledger already carried.
    if let Some(grant) = grant {
        state.grant_more(grant).map_err(|e| knl_err("resume", &e))?;
    }
    state.adopt_id(session_id);
    Ok(Session::from_state(state))
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
/// one `knl.open` returns, only pre-loaded with the balance folded from the
/// ledger.
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
    let store = parse_store("resume", Some(&opts))?;
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
    let store = match store {
        StoreSpec::Sqlite(path) => {
            knl::SqliteEventStore::open(std::path::Path::new(&path), session_id.clone())
        }
        // An in-memory stream is reopenable too, for as long as it exists:
        // the database is named after the stream, so a second handle on a
        // live one finds the same log.  It cannot outlive the process, and it
        // does not pretend to — a name nobody is holding open resumes as an
        // empty stream, which is refused for having no session in it.
        StoreSpec::Mem => knl::SqliteEventStore::open_memory(session_id.clone()),
    }
    .map_err(|e| knl_err("resume", &e))?;
    let state = resume_on(grant, store, session_id)?;
    lua.create_userdata(state)
}

/// Take an attributed message apart into `{ kind, method, retryable,
/// message }` — the body of `knl.error`.
///
/// # Why a function and not the raised value
///
/// The error a caller wants is a table.  It cannot be one: mlua raises every
/// failure a Rust callback returns as its own `WrappedFailure` userdata
/// ([`LuaError`] has no variant that carries a Lua value), so a bridge method
/// has no way to make a table *be* the raised object.  What it can do is
/// raise a message with a shape — [`attributed`] fixes the first three fields
/// as a closed vocabulary — and hand the shell a reader for it.  So
/// `knl.error(err)` is that reader: `pcall`, pass what was caught, and get
/// the table.
///
/// The argument is anything the raise handed over: the userdata, or a string
/// somebody already rendered.  Either way it is read as text.
///
/// An unrecognised message is not an error.  A raise that did not come from
/// this bridge (a Lua-side `error("...")`, a message from another module) is
/// reported as it is — `method = nil`, `kind = nil`, `retryable = false`, and
/// the whole text as `message` — because a reader that raised on unfamiliar
/// input would turn every unrelated failure into a second one, inside the
/// handler that was trying to describe the first.
///
/// The returned table carries a `__tostring` that gives the original message
/// back, so `tostring(knl.error(e))` is `tostring(e)` and a caller that only
/// wants to print or `find` in it does not have to know which it is holding.
fn error_table(lua: &Lua, raised: LuaValue) -> LuaResult<LuaTable> {
    let text = match &raised {
        LuaValue::String(text) => text.to_str()?.to_string(),
        // Anything else is rendered by Lua's own rules: the raised value is
        // a userdata whose `__tostring` is the message.
        other => other.to_string()?,
    };

    let out = lua.create_table()?;
    out.set("message", text.clone())?;
    out.set("retryable", false)?;

    // `knl: <method>: <kind>: <message>` — read off the line that carries
    // it, since a raise that crossed a callback boundary arrives with a
    // traceback on the lines after it.  Split on the first two separators
    // only: the message is whatever is left, colons and all.
    let attributed = text
        .lines()
        .find_map(|line| line.split_once("knl: ").map(|(_, rest)| rest));
    if let Some((method, rest)) = attributed.and_then(|rest| rest.split_once(": ")) {
        if let Some((kind, message)) = rest.split_once(": ") {
            // Only a kind the kernel actually publishes is taken as one, so
            // a message that merely looks like the shape is left as prose.
            if knl::KnlError::KINDS.contains(&kind) {
                out.set("method", method)?;
                out.set("kind", kind)?;
                out.set("retryable", knl::KnlError::kind_is_retryable(kind))?;
                out.set("message", message)?;
            }
        }
    }

    // The table renders as the message it was read from, so it can stand in
    // for the raised value wherever one was being printed or searched.
    let meta = lua.create_table()?;
    meta.set(
        "__tostring",
        lua.create_function(move |_, _: LuaValue| Ok(text.clone()))?,
    )?;
    out.set_metatable(Some(meta))?;
    Ok(out)
}

/// Mint a beat id — the body of `knl.new_beat_id`.
///
/// A UUID v7: random, but with its timestamp in the leading bits, so beat
/// ids of one session sort in the order they were minted.  That ordering is
/// the only property the id carries; the kernel treats it as opaque.
///
/// Session-free on purpose, like a sequence generator: a beat is declared by
/// the layer that drives the loop, and asking the kernel for one would put
/// the numbering back where this round took it out of.
fn new_beat_id(_: &Lua, _: ()) -> LuaResult<String> {
    Ok(uuid::Uuid::now_v7().to_string())
}

/// The declared surface as a Lua table — the body of `knl.api()`.
///
/// `{ session = { { name = …, doc = … }, … }, module = { … }, errors = { … },
/// schema = { table = …, columns = { { name, type, pk }, … } } }`, built from
/// [`SESSION_API`], [`MODULE_API`], [`knl::KnlError::KINDS`] and the events
/// table itself, so a caller reads what the kernel offers from the same
/// tables the reflection test holds the registration to.
///
/// `errors` is the closed list of classes `knl.error(e).kind` can report.  It
/// is published for the same reason the two method lists are: the shell keeps
/// its own declaration of the vocabulary, and a declaration nobody can check
/// is one that drifts.
fn api(lua: &Lua, _: ()) -> LuaResult<LuaTable> {
    /// One `{ name, doc }` list, in declaration order.
    fn listed(lua: &Lua, entries: &[(&str, &str)]) -> LuaResult<LuaTable> {
        let list = lua.create_table()?;
        for (index, (name, doc)) in entries.iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("name", *name)?;
            entry.set("doc", *doc)?;
            list.set(index + 1, entry)?;
        }
        Ok(list)
    }

    let out = lua.create_table()?;
    out.set("session", listed(lua, SESSION_API)?)?;
    out.set("module", listed(lua, MODULE_API)?)?;

    let errors = lua.create_table()?;
    for (index, kind) in knl::KnlError::KINDS.iter().enumerate() {
        errors.set(index + 1, *kind)?;
    }
    out.set("errors", errors)?;

    // The read contract: the table a query names and the columns it has.
    // Read off SQLite itself (`PRAGMA table_info`) rather than written out
    // here, so the published schema is the schema — a caller's SQL is written
    // against columns that exist, and the shell's own declaration of them can
    // be checked instead of trusted.
    let schema = lua.create_table()?;
    schema.set("table", knl::EVENTS_TABLE)?;
    let columns = lua.create_table()?;
    for (index, column) in knl::events_schema()
        .map_err(|e| knl_err("api", &e))?
        .into_iter()
        .enumerate()
    {
        let entry = lua.create_table()?;
        entry.set("name", column.name)?;
        entry.set("type", column.declared_type)?;
        entry.set("pk", column.pk)?;
        columns.set(index + 1, entry)?;
    }
    schema.set("columns", columns)?;
    out.set("schema", schema)?;
    Ok(out)
}

/// Register the `knl` global.  No [`crate::host::HostContext`] is needed —
/// this layer keeps all state inside the session userdata.
///
/// The functions are exactly [`MODULE_API`]: `knl.open(opts?)` is the
/// constructor (owner- and store-aware), `knl.resume(opts)` reopens a
/// persisted SQLite session, `knl.new_beat_id()` mints a beat id for the
/// caller to stamp on events, `knl.error(e)` reads a raised failure back as
/// a table, and `knl.api()` reports the declared surface.
/// Each is bound by hand — a `create_function` needs its own signature — and
/// a test below checks the set of bound names against the table.
pub fn register(lua: &Lua) -> LuaResult<()> {
    let knl_tbl = lua.create_table()?;

    // knl.open(opts?) -> Session userdata
    knl_tbl.set("open", lua.create_function(open_session)?)?;

    // knl.resume(opts) -> Session userdata (durable stream re-folded)
    knl_tbl.set("resume", lua.create_function(resume_session)?)?;

    // knl.new_beat_id() -> string (time-ordered, session-free)
    knl_tbl.set("new_beat_id", lua.create_function(new_beat_id)?)?;

    // knl.error(err) -> { kind, method, retryable, message }
    knl_tbl.set("error", lua.create_function(error_table)?)?;

    // knl.api() -> the declared surface, session methods and module functions
    knl_tbl.set("api", lua.create_function(api)?)?;

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

        -- The classified failure of a call that is supposed to fail, plus
        -- the raised value itself for the tests that check how it reads.
        function failure(fn, ...)
            local ok, raised = pcall(fn, ...)
            assert(not ok, "the call was supposed to fail")
            return knl.error(raised), raised
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
    /// `session_opened`.
    #[test]
    fn append_assigns_monotonic_seq_and_len_tracks() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open()
            assert(s:len() == 1, "a fresh session holds session_opened")
            local a = s:append({ kind = "user_msg", data = { text = "hi" } })
            local b = s:append({ kind = "note", data = { name = "sh" } })
            assert(a == 2, "first caller seq: " .. tostring(a))
            assert(b == 3, "second caller seq: " .. tostring(b))
            assert(s:len() == 3, "len: " .. tostring(s:len()))

            local evs = s:events()
            assert(#evs == 3, "events len: " .. tostring(#evs))
            assert(evs[1].kind == "session_opened")
            assert(evs[2].kind == "user_msg")
            assert(evs[2].data.text == "hi")
            assert(evs[2].seq == 2)
            assert(type(evs[2].epoch_ms) == "number", "epoch_ms must be a number")
            assert(evs[3].kind == "note")
            assert(evs[3].seq == 3)

            -- The envelope is closed: a kind's own field at the top level is
            -- refused, with the place it belongs in the message.
            local err = failure(function() s:append({ kind = "note", text = "hi" }) end)
            assert(err.kind == "validation", "kind: " .. tostring(err.kind))
            assert(err.message:find("under data"), "message: " .. err.message)
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
            s:append({ kind = "user_msg", meta = { tag = "a" },
                       data = { text = "hi", blocks = { { type = "text" } } } })

            local evs = s:events()
            evs[2].kind = "TAMPERED"
            evs[2].data.text = nil
            evs[2].data.extra = "injected"
            evs[2].meta.tag = "b"
            evs[2].data.blocks[1].type = "tampered"
            table.insert(evs, { kind = "ghost" })

            local again = s:events()
            assert(#again == 2, "history length changed: " .. tostring(#again))
            assert(again[2].kind == "user_msg", "kind changed: " .. tostring(again[2].kind))
            assert(again[2].data.text == "hi", "data changed")
            assert(again[2].data.extra == nil, "field injected into history")
            assert(again[2].meta.tag == "a", "meta changed")
            assert(again[2].data.blocks[1].type == "text", "nested table changed")
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
            local s = knl.open({ budget = { amount = 100, tag = "beats" } })
            s:spend(-1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("non-negative"), "{msg}");

        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 100, tag = "beats" } })
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
    /// up.  `spend` itself answers nothing: the balance is `remaining()`.
    #[test]
    fn spend_is_monotonic_and_flips_exhausted() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 1000, tag = "beats" } })
            assert(s:remaining() == 1000)
            assert(s:exhausted() == false)

            local prev = s:remaining()
            for _, n in ipairs({ 120, 0, 300, 80 }) do
                assert(s:spend(n) == nil, "spend answers with the write, not a number")
                local r = s:remaining()
                assert(r <= prev, "remaining rose: " .. tostring(prev) .. " -> " .. tostring(r))
                prev = r
            end
            assert(s:remaining() == 500, "remaining: " .. tostring(s:remaining()))
            assert(s:exhausted() == false)

            -- Overspending floors at zero and never goes negative.
            s:spend(9999)
            assert(s:remaining() == 0, "floor: " .. tostring(s:remaining()))
            assert(s:exhausted() == true, "exhausted must flip after overspending")
            s:spend(1)
            assert(s:remaining() == 0, "spending past zero stays at zero")
        "#,
        )
        .exec()
        .expect("budget monotonicity chunk");
    }

    /// (I3) Without a budget, `remaining()` is nil, `spend` records nothing
    /// and the session is never exhausted.
    #[test]
    fn session_without_budget_reports_nil() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open()
            assert(s:remaining() == nil, "remaining must be nil without a budget")
            assert(s:spend(50) == nil, "spend answers nothing")
            assert(s:len() == 1, "a settlement without a budget records nothing")
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
            assert(granted.data.amount == 42 and granted.data.tag == "tokens")
            assert(granted.data.desc == "one nightly run",
                   "desc: " .. tostring(granted.data.desc))

            local bare = knl.open({ budget = { amount = 7 } })
            local g2 = bare:events()[2].data
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
            local a = knl.open({ budget = { amount = 100, tag = "beats" } })
            local b = knl.open({ budget = { amount = 100, tag = "beats" } })

            assert(type(a:id()) == "string" and #a:id() > 0, "id must be a non-empty string")
            assert(a:id() ~= b:id(), "session ids must be unique")

            a:append({ kind = "only_in_a" })
            a:spend(60)

            -- a: session_opened, budget_granted, only_in_a, budget_spent.
            -- b: session_opened, budget_granted.
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
            local s = knl.open({ budget = { amount = 10, tag = "beats" } })
            s:close()
            s:spend(1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        // A closed session cannot be granted more either.
        let msg = expect_err(
            &lua,
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "beats" } })
            s:close()
            s:reserve(1)
        "#,
        );
        assert!(msg.contains("knl: reserve:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "beats" } })
            s:append({ kind = "before_close" })
            s:spend(4)
            s:close()
            s:close() -- idempotent

            -- Reads still work after the session ends.
            assert(s:len() == 5,
                   "session_opened + budget_granted + before_close + budget_spent + session_closed")
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
            assert(s:len() == 1, "a second VM starts with only its own session_opened")
            assert(s:events()[1].kind == "session_opened")
        "#,
            )
            .exec()
            .expect("vm b chunk");
    }

    /// The kernel brackets the session: `session_opened` on open,
    /// `session_closed` on close, with the caller's reason (or the default).
    #[test]
    fn session_boundaries_are_recorded_by_the_kernel() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open()
            local opened = s:events()[1]
            assert(opened.kind == "session_opened", "kind: " .. tostring(opened.kind))
            assert(opened.seq == 1)

            s:close("budget_exhausted")
            local evs = s:events()
            assert(#evs == 2, "close must record session_closed")
            assert(evs[2].kind == "session_closed")
            assert(evs[2].data.reason == "budget_exhausted")

            s:close("ignored")
            assert(s:len() == 2, "close is idempotent")

            -- Without a reason the kernel records its default.
            local d = knl.open()
            d:close()
            assert(d:events()[2].data.reason == "closed", "default reason")
        "#,
        )
        .exec()
        .expect("session boundary chunk");

        let msg = expect_err(&lua, r#"knl.open():close({ not_a = "string" })"#);
        assert!(msg.contains("knl: close:"), "missing attribution: {msg}");
        assert!(msg.contains("reason must be a string"), "{msg}");
    }

    /// The kernel checks the *envelope* of every event — the closed set of
    /// top-level keys, a string beat, a shallow meta, a table data — and the
    /// shape of a kind's own `data` is the writer's business, not its.
    #[test]
    fn the_envelope_is_validated_and_a_kinds_own_data_is_not() {
        let lua = vm();

        // A kind's own field at the top level: refused, and the message says
        // where it goes.
        let msg = expect_err(
            &lua,
            r#"knl.open():append({ kind = "msg_user", content = "hi" })"#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("content"), "{msg}");
        assert!(msg.contains("under data"), "{msg}");

        // `meta` is shallow: nesting belongs under `data`.
        let msg = expect_err(
            &lua,
            r#"knl.open():append({ kind = "note", meta = { deep = { a = 1 } } })"#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("meta is shallow"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open():append({ kind = "note", data = 7 })"#);
        assert!(msg.contains("data must be a table"), "{msg}");

        lua.load(
            r#"
            local s = knl.open()
            pcall(function() s:append({ kind = "note", text = "hi" }) end)
            assert(s:len() == 1, "a rejected event was recorded")

            -- The kinds of a turn are the Lua kernel's, shape and all: the
            -- Rust side takes whatever `data` says, at any depth.
            local beat = knl.new_beat_id()
            s:append({ kind = "msg_user", data = { content = "hi" } })
            s:append({ kind = "tool_call", beat = beat,
                       data = { call_id = "c1", name = "sh", args = { cmd = "ls" } } })
            s:append({ kind = "tool_result", beat = beat,
                       data = { call_id = "c1", ok = false, result = "boom" } })
            -- …including an empty one.
            s:append({ kind = "tool_call" })
            assert(s:len() == 5)
            assert(s:events()[3].beat == beat, "the declared beat is recorded")
            assert(s:events()[5].beat == nil, "an undeclared beat stays absent")
            assert(s:events()[3].data.args.cmd == "ls", "data comes back at any depth")
            assert(next(s:events()[5].data) == nil, "an absent data reads as empty")

            -- meta takes scalars, and comes back as it was written.
            s:append({ kind = "note", meta = { label = "a", attempt = 2, retried = true } })
            local m = s:events()[6].meta
            assert(m.label == "a" and m.attempt == 2 and m.retried == true,
                   "meta round-trips")

            -- A numbered beat is refused, on any kind.
            local ok = pcall(function() s:append({ kind = "note", beat = 1 }) end)
            assert(not ok, "a numeric beat was accepted")
        "#,
        )
        .exec()
        .expect("envelope chunk");
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
            local s = knl.open({ budget = { amount = 10, tag = "beats" } })
            s:append({ kind = "budget_reserved", data = { amount = 5 } })
        "#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("kernel only"), "{msg}");
        assert!(msg.contains("budget_reserved"), "{msg}");

        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "beats" } })
            for _, ev in ipairs({
                { kind = "budget_granted", data = { amount = 1000000 } },
                { kind = "budget_reserved", data = { amount = 5 } },
                { kind = "budget_refused", data = { amount = 5, remaining = 0 } },
                { kind = "budget_spent", data = { amount = 5 } },
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
            assert(evs[3].kind == "budget_reserved" and evs[3].data.amount == 4,
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
            local s = knl.open({ budget = { amount = 100, tag = "beats" } })

            local ok, tag = s:reserve(30)
            assert(ok == true, "a covered reservation must be granted")
            assert(tag == nil, "a granted reservation names no budget")
            assert(s:remaining() == 70, "remaining: " .. tostring(s:remaining()))

            local ok2, tag2 = s:reserve(1000)
            assert(ok2 == false, "an uncovered reservation must be refused")
            assert(tag2 == "beats", "a refusal must name the budget: " .. tostring(tag2))
            assert(s:remaining() == 70, "a refusal must not deduct")
            assert(s:exhausted() == false, "a refusal must not exhaust")

            -- Both answers are in the log, with what was asked for.
            local evs = s:events()
            assert(evs[3].kind == "budget_reserved" and evs[3].data.amount == 30)
            assert(evs[3].data.tag == "beats")
            assert(evs[4].kind == "budget_refused" and evs[4].data.amount == 1000)
            assert(evs[4].data.remaining == 70, "the refusal records what there was")

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
        // recorded: a session with no quota keeps no ledger.
        lua.load(
            r#"
            local s = knl.open()
            local ok, tag = s:reserve(999999)
            assert(ok == true and tag == nil, "no budget must grant everything")
            assert(s:len() == 1, "a session with no quota recorded a ledger event")
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
                        balance = (balance or 0) + ev.data.amount
                    elseif ev.kind == "budget_reserved" or ev.kind == "budget_spent" then
                        balance = math.max(0, balance - ev.data.amount)
                    end
                end
                return balance
            end

            local s = knl.open({ budget = { amount = 500, tag = "beats" } })
            assert(folded(s) == s:remaining())

            s:reserve(120)
            s:append({ kind = "llm_response",
                       data = { content = { { type = "text", text = "hi" } },
                                usage = { input_tokens = 100, output_tokens = 50 } } })
            s:spend(30)          -- the call overran its estimate
            s:reserve(10000)     -- refused, and moves nothing
            s:spend(0)

            assert(s:remaining() == 350, "remaining: " .. tostring(s:remaining()))
            assert(folded(s) == s:remaining(), "the fold and the counter disagree")

            -- What the call consumed is the other, independent reading, and
            -- it is in the log rather than in the ledger: the counts sit on
            -- the response, for a query view to sum.
            local r = s:events()[4]  -- opened, granted, reserved, the response
            assert(r.kind == "llm_response", "kind: " .. tostring(r.kind))
            assert(r.data.usage.input_tokens == 100 and r.data.usage.output_tokens == 50)
        "#,
        )
        .exec()
        .expect("fold chunk");
    }

    /// The session's own boundaries are the kernel's: Lua can read them but
    /// not write them.  Hand-appending either would be claiming an opening
    /// the stream never had, or an ending it never reached, so both are
    /// refused and the session stays open.
    #[test]
    fn lua_cannot_append_the_session_boundary_kinds_by_hand() {
        let lua = vm();

        let msg = expect_err(
            &lua,
            r#"
            local s = knl.open()
            s:append({ kind = "session_closed", data = { reason = "carried over" } })
        "#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("kernel only"), "{msg}");
        assert!(msg.contains("session_closed"), "{msg}");

        lua.load(
            r#"
            local s = knl.open({ budget = { amount = 100, tag = "beats" } })
            for _, ev in ipairs({
                { kind = "session_opened", data = { scope_id = "s", owner = "me" } },
                { kind = "session_closed", data = { reason = "carried over" } },
            }) do
                local ok = pcall(function() s:append(ev) end)
                assert(not ok, "a caller wrote " .. ev.kind)
            end

            -- Still open, and nothing was recorded.
            assert(s:len() == 2, "a rejected boundary was recorded: " .. tostring(s:len()))
            assert(s:append({ kind = "note" }) == 3, "the refusal ended the session")
            s:spend(10)
            assert(s:remaining() == 90)

            -- Only close writes the boundary, and it writes exactly one.
            s:close("done")
            assert(kinds_of(s) ==
                   "session_opened,budget_granted,note,budget_spent,session_closed",
                   "recorded: " .. kinds_of(s))
            local evs = s:events()
            assert(evs[5].data.reason == "done",
                   "reason: " .. tostring(evs[5].data.reason))

            local ok = pcall(function() s:append({ kind = "note" }) end)
            assert(not ok, "a closed session took a write")
        "#,
        )
        .exec()
        .expect("session boundary kind chunk");
    }

    /// `knl.new_beat_id()` mints the beat id the shell stamps on its events:
    /// a fresh non-empty string every call, needing no session, and ordered
    /// by the time it was minted (UUID v7) so a stream's beats sort the way
    /// they happened.
    #[test]
    fn new_beat_id_mints_distinct_time_ordered_ids() {
        let lua = vm();
        lua.load(
            r#"
            local a = knl.new_beat_id()
            local b = knl.new_beat_id()
            assert(type(a) == "string" and #a > 0, "a beat id must be a non-empty string")
            assert(a ~= b, "two beats must be two ids")
            assert(a < b, "beat ids must sort in the order they were minted: " .. a .. " " .. b)

            -- Version 7: the 13th hex digit of a UUID is the version nibble.
            assert(a:sub(15, 15) == "7", "not a v7 uuid: " .. a)

            -- It is a module function, not a session method: no session is
            -- needed to name a beat.
            local s = knl.open()
            assert(s.new_beat_id == nil, "the beat id is not the session's to mint")

            -- And it is what the kernel accepts as a beat.
            s:append({ kind = "llm_response", beat = a,
                       data = { content = { { type = "text", text = "ok" } },
                                usage = { input_tokens = 1 } } })
            assert(s:events()[2].beat == a, "the minted beat is recorded verbatim")
        "#,
        )
        .exec()
        .expect("new_beat_id chunk");
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

        // The token account is one of the names the kernel does not have:
        // it reads the `data` of an `llm_response`, so it is a query view in
        // Lua (`knl.views.usage`) over the published schema.
        let msg = expect_err(&lua, r#"knl.open():view("usage")"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains(r#"unknown view "usage""#), "{msg}");

        let msg = expect_err(&lua, r#"knl.open():view(42)"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("name must be a string"), "{msg}");

        let msg = expect_err(&lua, r#"knl.open():view("tail", "n=2")"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("opts must be a table"), "{msg}");
    }

    /// (I1) A view is a fresh table every call: mutating it cannot reach
    /// the history.
    #[test]
    fn view_returns_a_fresh_table_each_call() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open()
            s:append({ kind = "msg_user", data = { content = "hi" } })

            local t = s:view("tail", { n = 1 })
            t[1].kind = "TAMPERED"
            t[1].data.content = nil
            table.insert(t, { kind = "ghost" })

            local again = s:view("tail", { n = 1 })
            assert(#again == 1, "tail length changed: " .. tostring(#again))
            assert(again[1].kind == "msg_user", "kind changed: " .. tostring(again[1].kind))
            assert(again[1].data.content == "hi", "content changed")

            -- …and the history itself is untouched by any of it.
            assert(s:len() == 2, "len: " .. tostring(s:len()))
            assert(s:events()[2].kind == "msg_user", "the record was reachable")
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
            local s = knl.open({ store = "mem", owner = "x", budget = { amount = 10, tag = "beats" } })
            assert(s:len() == 2, "mem store opens like the default: session_opened + the grant")
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

    /// (scope) A session has a scope: `s:scope_id()` is a real kernel-issued
    /// string, it is not the session id, and it is what `session_opened` and
    /// every `budget_*` event were written under.  Two runs are two scopes.
    #[test]
    fn a_session_reports_its_scope_id_and_records_it_on_the_log() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ owner = "alice", budget = { amount = 100, tag = "beats" } })
            local scope = s:scope_id()
            assert(type(scope) == "string" and #scope > 0, "scope_id must be a non-empty string")
            assert(scope ~= s:id(), "the scope names the authority, the id names the stream")

            s:reserve(30)     -- budget_reserved
            s:spend(10)       -- budget_spent
            s:reserve(10000)  -- budget_refused

            local seen = 0
            for _, e in ipairs(s:events()) do
                if e.kind == "session_opened" then
                    assert(e.data.scope_id == scope,
                           "session_opened scope_id: " .. tostring(e.data.scope_id))
                    assert(e.data.owner == "alice", "the owner rides beside it")
                    seen = seen + 1
                elseif e.kind:sub(1, 7) == "budget_" then
                    assert(e.data.scope_id == scope,
                           e.kind .. " scope_id: " .. tostring(e.data.scope_id))
                    seen = seen + 1
                end
            end
            assert(seen == 5,
                   "session_opened + granted + reserved + spent + refused: " .. tostring(seen))

            -- A caller's own event carries no scope id: the field is on the
            -- kinds only the kernel writes.
            s:append({ kind = "note" })
            local evs = s:events()
            assert(evs[#evs].data.scope_id == nil,
                   "a caller's event must not carry a scope id")

            assert(knl.open({ owner = "bob" }):scope_id() ~= scope,
                   "two runs must be two scopes")
        "#,
        )
        .exec()
        .expect("scope id chunk");
    }

    /// (scope, durable) The scope outlives the process: a reopened stream
    /// resumes under the id its `session_opened` recorded — not a fresh one —
    /// and the ledger it goes on writing names that same scope.
    #[test]
    fn a_resumed_session_keeps_the_scope_id_the_log_recorded() {
        let lua = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path = path.to_str().expect("utf-8 path");

        lua.load(format!(
            r#"
            local path = "{path}"
            local s = knl.open({{ store = {{ sqlite = path }}, owner = "scoped-user",
                                  budget = {{ amount = 100, tag = "beats" }} }})
            local id, scope = s:id(), s:scope_id()
            assert(scope ~= id, "the scope id is not the stream id")
            s:reserve(20)

            -- Resumed while the stream is still open: a session is disposable,
            -- so a closed one is never reopened.
            local r = knl.resume({{ store = {{ sqlite = path }}, session = id }})
            assert(r:id() == id, "resumed id is the stream it reopened")
            assert(r:scope_id() == scope, "resumed scope: " .. tostring(r:scope_id()))
            assert(r:owner() == "scoped-user", "resumed owner: " .. tostring(r:owner()))

            r:reserve(5)
            local evs = r:events()
            local last = evs[#evs]
            assert(last.kind == "budget_reserved", "last kind: " .. tostring(last.kind))
            assert(last.data.scope_id == scope,
                   "continued scope_id: " .. tostring(last.data.scope_id))
        "#
        ))
        .exec()
        .expect("durable scope chunk");
    }

    /// (durable) `knl.open({ store = { sqlite = path } })` writes to a
    /// persisted stream, and `knl.resume` reopens it and re-folds the
    /// record: the owner, the balance the budget ledger implies and the
    /// recorded events all come back, and the resumed session carries on
    /// from there.  A `budget` on resume is the owner granting again:
    /// recorded, and added to what was left.
    #[test]
    fn open_and_resume_a_durable_sqlite_session() {
        let lua = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path = path.to_str().expect("utf-8 path");

        lua.load(format!(
            r#"
            local path = "{path}"
            -- The responses in a stream, for the counts a query view sums.
            local function responses(s)
                local out = {{}}
                for _, ev in ipairs(s:events()) do
                    if ev.kind == "llm_response" then out[#out + 1] = ev end
                end
                return out
            end

            local s = knl.open({{ store = {{ sqlite = path }}, owner = "durable-user",
                                  budget = {{ amount = 100, tag = "beats" }} }})
            s:reserve(30)
            s:append({{ kind = "llm_response",
                        data = {{ content = {{ {{ type = "text", text = "a" }} }},
                                  usage = {{ input_tokens = 30 }} }} }})
            s:append({{ kind = "msg_user", data = {{ content = "more" }} }})
            s:reserve(15)
            s:append({{ kind = "llm_response",
                        data = {{ content = {{ {{ type = "text", text = "b" }} }},
                                  usage = {{ input_tokens = 20 }} }} }})
            s:spend(5)  -- the second call overran its estimate
            assert(s:remaining() == 50, "open remaining: " .. tostring(s:remaining()))
            local id = s:id()

            -- Reopen the same stream and continue where it left off.  No new
            -- grant: the balance is what the ledger says was left.  The stream
            -- is still open: a session is disposable, so a closed one is not
            -- reopened.
            local r = knl.resume({{ store = {{ sqlite = path }}, session = id }})
            assert(r:owner() == "durable-user", "resumed owner: " .. tostring(r:owner()))
            assert(r:remaining() == 50, "resumed remaining: " .. tostring(r:remaining()))
            assert(r:id() == id, "resumed id is the stream it reopened")
            -- The record came back whole: the counts are on the responses,
            -- where a query view reads them.
            local rs = responses(r)
            assert(#rs == 2, "resumed responses: " .. tostring(#rs))
            assert(rs[1].data.usage.input_tokens == 30
                       and rs[2].data.usage.input_tokens == 20,
                   "the counts came back with the record")

            -- The grant's words came back too: a refusal still names it.
            local ok, tag = r:reserve(1000)
            assert(ok == false and tag == "beats", "refused tag: " .. tostring(tag))

            -- The record and the ledger continue on the resumed session.
            r:reserve(5)
            r:append({{ kind = "llm_response", beat = knl.new_beat_id(),
                        data = {{ content = {{ {{ type = "text", text = "c" }} }},
                                  usage = {{ input_tokens = 5 }} }} }})
            assert(#responses(r) == 3, "continued responses: " .. tostring(#responses(r)))
            assert(r:remaining() == 45, "continued remaining: " .. tostring(r:remaining()))

            -- Granting again on resume adds to what is left, and is recorded.
            local g = knl.resume({{ store = {{ sqlite = path }}, session = id,
                                    budget = {{ amount = 100, tag = "beats",
                                                desc = "a second grant" }} }})
            assert(g:remaining() == 145, "re-granted remaining: " .. tostring(g:remaining()))
            local evs = g:events()
            local last = evs[#evs]
            assert(last.kind == "budget_granted", "last event: " .. tostring(last.kind))
            assert(last.data.amount == 100 and last.data.desc == "a second grant")
        "#
        ))
        .exec()
        .expect("durable open/resume chunk");
    }

    /// (owner namespace) resume holds the same reserved-principal line as
    /// open: a stream the host opened as SYSTEM cannot be reopened from
    /// Lua, or an untrusted caller could write into the reserved namespace
    /// through the resume side door.
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

    /// (attribution) resume needs a session id, and a stream that is one:
    /// each missing piece is a `knl: resume:` error.
    #[test]
    fn resume_requires_a_session_id_and_a_stream_that_holds_a_session() {
        let lua = vm();

        let msg = expect_err(&lua, r#"knl.resume()"#);
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");

        let msg = expect_err(&lua, r#"knl.resume({ store = { sqlite = "/tmp/x.db" } })"#);
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");
        assert!(msg.contains("session is required"), "{msg}");

        // A name nobody is holding open is an empty stream, not a session:
        // an in-memory database exists only while a handle does, so resuming
        // one that has gone is refused for having no opening in it — the same
        // answer a fresh file gives.
        let msg = expect_err(&lua, r#"knl.resume({ session = "never-opened" })"#);
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");
        assert!(msg.contains("no session to resume"), "{msg}");
    }

    /// (mem) An in-memory session is a session: it is resumable while it is
    /// alive, by the id it reports, and the resumed handle reads the same log
    /// and continues the same ledger.  What it cannot do is outlive the
    /// process, and nothing here pretends otherwise.
    #[test]
    fn an_in_memory_stream_is_resumable_while_it_is_open() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ owner = "mem-user", budget = { amount = 100, tag = "beats" } })
            local id = s:id()
            s:reserve(30)
            s:append({ kind = "note", data = { text = "in memory" } })

            -- The writer is still alive, so the database is still there.
            local r = knl.resume({ store = "mem", session = id })
            assert(r:id() == id, "resumed id: " .. tostring(r:id()))
            assert(r:owner() == "mem-user", "resumed owner: " .. tostring(r:owner()))
            assert(r:remaining() == 70, "resumed remaining: " .. tostring(r:remaining()))
            assert(r:len() == 4, "session_opened + granted + reserved + note")

            -- An absent store means the same thing on resume as it does on
            -- open: the in-memory database.
            local r2 = knl.resume({ session = id })
            assert(r2:remaining() == 70, "resumed again: " .. tostring(r2:remaining()))

            -- And the resumed handle writes into the same log.
            r:spend(20)
            assert(s:remaining() == 50, "the writer sees it: " .. tostring(s:remaining()))
        "#,
        )
        .exec()
        .expect("in-memory resume chunk");
    }

    // -- session lifecycle: `<close>` and the drop backstop ----------------
    //
    // Every one of these reads the boundary back out of a *reopened* SQLite
    // stream rather than off the handle that wrote it: the question is
    // whether the record landed, and only the durable log answers that.

    /// The persisted events of `stream`, read through a fresh connection.
    fn persisted(path: &std::path::Path, stream: &str) -> Vec<Value> {
        use crate::knl::EventStore;

        let store = crate::knl::SqliteEventStore::open(path, stream).expect("reopen the stream");
        store.read(0, usize::MAX).expect("read the stream")
    }

    /// Run `chunk` in a fresh VM and return the session id it yields.
    ///
    /// The VM is dropped before the caller reads the stream, so anything the
    /// collector still owed has been paid by the time the log is inspected.
    fn stream_id_from(chunk: String) -> String {
        let lua = vm();
        lua.load(chunk).eval::<String>().expect("close scope chunk")
    }

    /// (I6) A `<close>` scope that ends cleanly records the session's
    /// boundary with `scope_exit`: the shell no longer has to remember to
    /// close.
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
                assert(s:len() == 2, "inside the scope: session_opened + note")
            end
            return id
        "#
        ));

        let log = persisted(&path, &id);
        let last = log.last().expect("the stream is not empty");
        assert_eq!(last["kind"], Value::from("session_closed"), "{last}");
        assert_eq!(last["data"]["reason"], Value::from("scope_exit"), "{last}");
        assert_eq!(
            last["data"].get("detail"),
            None,
            "a clean exit has nothing to say"
        );
        assert_eq!(log.len(), 3, "session_opened + note + session_closed");
    }

    /// (I6) A block that raises closes its session too, with `error` as the
    /// reason and the message as `detail` — so the log says the session
    /// ended badly without the reason vocabulary growing a member per
    /// failure.
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
        assert_eq!(last["kind"], Value::from("session_closed"), "{last}");
        assert_eq!(last["data"]["reason"], Value::from("error"), "{last}");
        let detail = last["data"]["detail"].as_str().expect("detail text");
        assert!(detail.contains("boom"), "detail: {detail}");
    }

    // -- a store that fails, so a failed close can be driven from Lua -------
    //
    // An append is serialized and lands, so a close can no longer be made to
    // fail by racing another handle.  What is left is a backend that reports
    // a failure, which is a real thing a durable store does (a database gone,
    // or contended past its retries).  The store below is the smallest honest
    // stand-in: it fails the append at a chosen position and no other.

    /// A [`knl::EventStore`] that fails its `nth` append and serves the rest
    /// from an in-memory log the test keeps a handle on.
    struct FlakyStore {
        /// The real log, shared with the test so it can be read after the
        /// session that owned it is gone.
        inner: std::rc::Rc<RefCell<knl::MemEventStore>>,
        /// Which append (1-based) fails; `0` fails none.
        fails_on: usize,
        /// How many appends have been attempted.
        attempts: std::cell::Cell<usize>,
    }

    impl FlakyStore {
        /// A store whose `fails_on`-th append reports a failure, plus the
        /// handle on the log it writes to.
        fn new(fails_on: usize) -> (Self, std::rc::Rc<RefCell<knl::MemEventStore>>) {
            let inner = std::rc::Rc::new(RefCell::new(knl::MemEventStore::new()));
            let store = Self {
                inner: std::rc::Rc::clone(&inner),
                fails_on,
                attempts: std::cell::Cell::new(0),
            };
            (store, inner)
        }

        /// Whether this attempt is the one that fails.
        fn fails_now(&self) -> bool {
            self.attempts.set(self.attempts.get() + 1);
            self.attempts.get() == self.fails_on
        }
    }

    impl knl::EventStore for FlakyStore {
        fn append(&mut self, event: Map<String, Value>) -> knl::KnlResult<knl::Committed> {
            if self.fails_now() {
                return Err(knl::KnlError::Storage("the store is down".to_string()));
            }
            self.inner.borrow_mut().append(event)
        }

        /// A batch is *one* write, as it is on the durable backend: it counts
        /// as one attempt, and when that attempt is the failing one nothing
        /// in the batch is recorded.  A stand-in that let half a batch land
        /// would be modelling a store the SPI does not allow.
        fn append_many(
            &mut self,
            events: Vec<Map<String, Value>>,
        ) -> knl::KnlResult<Vec<knl::Committed>> {
            if self.fails_now() {
                return Err(knl::KnlError::Storage("the store is down".to_string()));
            }
            let mut inner = self.inner.borrow_mut();
            events
                .into_iter()
                .map(|event| inner.append(event))
                .collect()
        }

        fn append_if(
            &mut self,
            kinds: Option<&[&str]>,
            decide: &mut knl::Decision<'_>,
        ) -> knl::KnlResult<Option<knl::Committed>> {
            if self.fails_now() {
                return Err(knl::KnlError::Storage("the store is down".to_string()));
            }
            self.inner.borrow_mut().append_if(kinds, decide)
        }

        fn read_kinds(
            &self,
            kinds: Option<&[&str]>,
            from_seq: u64,
            limit: usize,
        ) -> knl::KnlResult<Vec<Value>> {
            self.inner.borrow().read_kinds(kinds, from_seq, limit)
        }

        fn head(&self) -> knl::KnlResult<Option<u64>> {
            self.inner.borrow().head()
        }

        fn len(&self) -> knl::KnlResult<usize> {
            self.inner.borrow().len()
        }
    }

    /// The kinds an in-memory log holds, in order.
    fn kinds_in(log: &RefCell<knl::MemEventStore>) -> Vec<String> {
        use crate::knl::EventStore;

        log.borrow()
            .read(0, usize::MAX)
            .expect("read the log")
            .iter()
            .map(|e| e["kind"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// A VM where `open_failing(n)` opens a session on a [`FlakyStore`] whose
    /// `n`-th append fails, and the log it writes to.
    ///
    /// The hook is test-only and lives here rather than in `register`: the
    /// Lua surface a caller sees is [`MODULE_API`] and nothing else.  It
    /// builds the same userdata `knl.open` builds, so `<close>`, the drop
    /// backstop and every method behave exactly as they do in production.
    fn vm_with_a_failing_store(fails_on: usize) -> (Lua, std::rc::Rc<RefCell<knl::MemEventStore>>) {
        let lua = vm();
        let (store, log) = FlakyStore::new(fails_on);
        let store = RefCell::new(Some(store));
        let open_failing = lua
            .create_function(move |lua, ()| {
                let store = store
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| err("open", "the failing store can only be opened once"))?;
                let state = knl::Session::open_on("t".to_string(), None, Box::new(store))
                    .map_err(|e| knl_err("open", &e))?;
                lua.create_userdata(Session::from_state(state))
            })
            .expect("create open_failing");
        lua.globals()
            .set("open_failing", open_failing)
            .expect("register open_failing");
        (lua, log)
    }

    /// (I6) The block's own error wins over a close that could not be
    /// recorded.
    ///
    /// The store fails the `session_closed` append, so `__close` has a real
    /// failure to report while an error is already on its way out of the
    /// block.  Lua would let `__close` replace that error; it must not,
    /// because the bookkeeping failure is not what the caller is trying to
    /// diagnose.  It goes to the tracing log instead.
    #[test]
    fn a_failed_close_does_not_replace_the_error_the_block_raised() {
        // 1: session_opened (no grant, so the close is the second append).
        let (lua, log) = vm_with_a_failing_store(2);

        lua.load(
            r#"
            local kept
            local ok, msg = pcall(function()
                local s <close> = open_failing()
                kept = s
                error("boom")
            end)
            assert(not ok, "the block was supposed to fail")
            assert(tostring(msg):find("boom"),
                   "the close replaced the block's error: " .. tostring(msg))
            assert(not tostring(msg):find("the store is down"),
                   "the close's own failure surfaced instead: " .. tostring(msg))
            -- The session stayed open: the boundary was not recorded, and the
            -- handle says so rather than pretending otherwise.
            assert(kept:len() == 1, "len after the failed close: " .. tostring(kept:len()))
        "#,
        )
        .exec()
        .expect("failing close chunk");

        assert_eq!(
            kinds_in(&log),
            ["session_opened"],
            "the boundary really was not recorded"
        );
    }

    /// (I6) A clean scope exit keeps raising when the boundary cannot be
    /// recorded: there is no body error to preserve, so silence would be a
    /// close reporting success with nothing in the log.
    #[test]
    fn a_failed_close_on_a_clean_scope_exit_still_raises() {
        let (lua, log) = vm_with_a_failing_store(2);

        let msg = expect_err(
            &lua,
            r#"
            do
                local s <close> = open_failing()
            end
        "#,
        );
        assert!(msg.contains("knl: close:"), "missing attribution: {msg}");
        assert!(msg.contains("the store is down"), "{msg}");

        // Nothing but the opening is in the log: the raise is the only thing
        // that says the session ended, which is why it must be raised.
        assert_eq!(kinds_in(&log), ["session_opened"]);
    }

    /// (F4) An open that cannot be recorded leaves the stream *empty*.
    ///
    /// The opening and the grant are one write now (`append_many`), so there
    /// is no window where a reader could see a session that began without the
    /// quota it began under — and nothing to close on the way out either.
    /// This replaces the earlier behaviour, where the two were separate
    /// appends and a failed second one had to be patched over with a
    /// best-effort `session_closed`.
    #[test]
    fn an_open_that_cannot_be_recorded_leaves_the_stream_empty() {
        // The whole opening is one write, so it is the first attempt.
        let (store, log) = FlakyStore::new(1);
        let err = knl::Session::open_on(
            "t".to_string(),
            Some(knl::BudgetGrant::new(100)),
            Box::new(store),
        )
        .expect_err("the open must fail");
        assert_eq!(err.reason(), "the store is down");

        assert!(
            kinds_in(&log).is_empty(),
            "a failed open records nothing at all: {:?}",
            kinds_in(&log)
        );
    }

    /// The other side of it: an open that *does* land records both events, in
    /// order, from the one write.
    #[test]
    fn an_open_records_its_boundary_and_its_grant_together() {
        let (store, log) = FlakyStore::new(0);
        let session = knl::Session::open_on(
            "t".to_string(),
            Some(knl::BudgetGrant::new(100)),
            Box::new(store),
        )
        .expect("the open lands");
        assert_eq!(kinds_in(&log), ["session_opened", "budget_granted"]);
        assert_eq!(session.remaining(), Ok(Some(100)));
    }

    /// `close(reason, detail)` records both: the reason stays the short word
    /// a reader folds on, and the sentence only this close can tell goes to
    /// `detail` — which is what lets a Lua-side bracket record the message of
    /// the error its body raised.  `close(reason)` and `close()` are
    /// unchanged.
    #[test]
    fn close_records_an_optional_detail_beside_the_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        let id = stream_id_from(format!(
            r#"
            local id
            do
                local s = knl.open({{ store = {{ sqlite = "{path_str}" }}, owner = "t" }})
                id = s:id()
                s:close("error", "the body raised: boom")
            end
            return id
        "#
        ));

        let log = persisted(&path, &id);
        let last = log.last().expect("the stream is not empty");
        assert_eq!(last["kind"], Value::from("session_closed"), "{last}");
        assert_eq!(last["data"]["reason"], Value::from("error"), "{last}");
        assert_eq!(
            last["data"]["detail"],
            Value::from("the body raised: boom"),
            "{last}"
        );

        // The one- and no-argument forms still work, and a detail is never
        // invented for them.
        let lua = vm();
        lua.load(
            r#"
            local a = knl.open({ owner = "t" })
            a:close("done")
            local last = a:events()[a:len()].data
            assert(last.reason == "done", "reason: " .. tostring(last.reason))
            assert(last.detail == nil, "a close with no detail must record none")

            local b = knl.open({ owner = "t" })
            b:close()
            local closed = b:events()[b:len()].data
            assert(closed.reason == "closed", "default reason: " .. tostring(closed.reason))
            assert(closed.detail == nil)
        "#,
        )
        .exec()
        .expect("close forms chunk");

        // And a non-string detail is refused, naming which argument it was.
        let msg = expect_err(&lua, r#"knl.open({ owner = "t" }):close("error", 7)"#);
        assert!(msg.contains("knl: close:"), "missing attribution: {msg}");
        assert!(msg.contains("detail must be a string"), "{msg}");
    }

    /// A long `detail` is cut to the cap, exactly as the `<close>` path cuts
    /// the message of a raised error: one bad turn must not put a page into
    /// the log, whichever side records it.
    #[test]
    fn a_long_close_detail_is_truncated() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ owner = "t" })
            s:close("error", string.rep("x", 500))
            local last = s:events()[s:len()].data
            assert(#last.detail == 203, "detail length: " .. tostring(#last.detail))
            assert(last.detail:sub(-3) == "...", "a cut detail says it was cut")
        "#,
        )
        .exec()
        .expect("long detail chunk");
    }

    /// (disposable) A closed stream is not reopened.  The session ended; what
    /// comes after an ending is a new session, and `knl.resume` says so
    /// instead of handing back a handle onto a finished log.
    #[test]
    fn resume_refuses_a_closed_stream() {
        let lua = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        let msg = expect_err(
            &lua,
            &format!(
                r#"
                local s = knl.open({{ store = {{ sqlite = "{path_str}" }}, owner = "t" }})
                local id = s:id()
                s:close("done")
                knl.resume({{ store = {{ sqlite = "{path_str}" }}, session = id }})
            "#
            ),
        );
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");
        assert!(msg.contains("disposable"), "{msg}");
    }

    /// (F3) A resume that is refused writes nothing.  The reserved-owner
    /// check runs before any append, so a caller cannot leave a
    /// `budget_granted` in a stream it was not allowed to reopen.
    #[test]
    fn a_refused_resume_records_no_grant() {
        let lua = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        // The host side legitimately opens a SYSTEM-owned stream.
        let stream = "system-grant-stream".to_string();
        let store = crate::knl::SqliteEventStore::open(&path, stream.clone()).expect("open store");
        let state =
            crate::knl::Session::open_on(crate::knl::SYSTEM.to_string(), None, Box::new(store))
                .expect("open system session");
        drop(state);
        let before = persisted(&path, &stream).len();

        let msg = expect_err(
            &lua,
            &format!(
                r#"knl.resume({{ store = {{ sqlite = "{path_str}" }}, session = "{stream}",
                                 budget = {{ amount = 100, tag = "beats" }} }})"#
            ),
        );
        assert!(msg.contains("reserved"), "{msg}");

        let log = persisted(&path, &stream);
        assert!(
            !log.iter().any(|e| e["kind"] == "budget_granted"),
            "a refused resume wrote its grant anyway: {log:?}"
        );
        assert_eq!(log.len(), before, "a refused resume wrote nothing at all");
    }

    /// The Lua surface is exactly what is declared: the methods a session
    /// answers to are [`SESSION_API`], the functions on the `knl` global are
    /// [`MODULE_API`], and `knl.api()` reports both.  A method registered
    /// without an entry in the table fails here.
    #[test]
    fn the_lua_surface_is_exactly_what_is_declared() {
        let lua = vm();

        // The session's methods, read off the live userdata's metatable.
        let mut declared: Vec<&str> = SESSION_API
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| *name != "__close")
            .collect();
        declared.sort_unstable();

        // Read off the live userdata's metatable.  Lua cannot reach it (mlua
        // protects it with `__metatable`), so the reflection is done from
        // here — it is still the registration itself that is being read, not
        // a second list of names.
        let session: LuaAnyUserData = lua
            .load(r#"return knl.open({ owner = "t" })"#)
            .eval()
            .expect("open a session to reflect over");
        let meta = session.metatable().expect("the session's metatable");
        let index: LuaTable = meta.get("__index").expect("the methods table");
        let mut reflected: Vec<String> = index
            .pairs::<String, LuaValue>()
            .map(|pair| pair.expect("a method entry").0)
            .collect();
        reflected.sort();
        assert_eq!(reflected, declared, "the session surface is SESSION_API");

        // The `<close>` metamethod is on the metatable itself, not in
        // `__index`, and it is declared too.
        assert!(
            SESSION_API.iter().any(|(name, _)| *name == "__close"),
            "the scope boundary belongs to the declared surface"
        );
        assert!(
            !matches!(
                meta.get::<LuaValue>("__close").expect("read __close"),
                LuaValue::Nil
            ),
            "a session must carry the <close> metamethod"
        );

        // The module's functions.
        let mut module: Vec<&str> = MODULE_API.iter().map(|(name, _)| *name).collect();
        module.sort_unstable();
        let mut bound: Vec<String> = lua
            .load(
                r#"
                local names = {}
                for name, value in pairs(knl) do
                    if type(value) == "function" then table.insert(names, name) end
                end
                return names
            "#,
            )
            .eval::<Vec<String>>()
            .expect("reflect over the knl global");
        bound.sort();
        assert_eq!(bound, module, "the module surface is MODULE_API");

        // And `knl.api()` hands the same two lists to Lua, each entry with
        // the name and its one-line contract.
        lua.load(
            r#"
            local api = knl.api()
            assert(#api.session > 0 and #api.module > 0, "api() must list both halves")
            for _, half in ipairs({ api.session, api.module }) do
                for _, entry in ipairs(half) do
                    assert(type(entry.name) == "string" and #entry.name > 0, "an entry needs a name")
                    assert(type(entry.doc) == "string" and #entry.doc > 0, "an entry needs a doc")
                end
            end
            assert(api.session[1].name == "id", "first: " .. tostring(api.session[1].name))
        "#,
        )
        .exec()
        .expect("api() chunk");

        let counted: usize = lua
            .load(r#"local a = knl.api() return #a.session + #a.module"#)
            .eval()
            .expect("count the api entries");
        assert_eq!(counted, SESSION_API.len() + MODULE_API.len());
    }

    /// A raised failure carries its class, and `knl.error` hands it back as
    /// a table: what a caller branches on is a word from a closed list, not
    /// a sentence that is free to be reworded.
    #[test]
    fn a_raised_failure_reports_its_class_through_knl_error() {
        let lua = vm();
        lua.load(
            r#"
            -- A closed handle refusing its own write.  The session is over,
            -- and asking again is not what fixes that.
            local s = knl.open({ owner = "t" })
            s:close()
            local e = failure(function() s:append({ kind = "note" }) end)
            assert(e.kind == "closed", "kind: " .. tostring(e.kind))
            assert(e.method == "append", "method: " .. tostring(e.method))
            assert(e.retryable == false, "a closed session is not a retry")
            assert(e.message == "session is closed", "message: " .. tostring(e.message))

            local t = knl.open({ owner = "t", budget = { amount = 10 } })

            -- A kernel-only kind: the caller asked for something the kernel
            -- will not record from it.
            local k = failure(function() t:append({ kind = "budget_granted", amount = 1 }) end)
            assert(k.kind == "validation", "kind: " .. tostring(k.kind))
            assert(k.method == "append", "method: " .. tostring(k.method))
            assert(k.retryable == false)

            -- A negative reserve, refused before anything moves.
            local n = failure(function() t:reserve(-1) end)
            assert(n.kind == "validation", "kind: " .. tostring(n.kind))
            assert(n.method == "reserve", "method: " .. tostring(n.method))

            -- An unknown view: the kernel's own validator, same class.
            local v = failure(function() t:view("nope") end)
            assert(v.kind == "validation", "kind: " .. tostring(v.kind))
            assert(v.method == "view", "method: " .. tostring(v.method))

            -- A refusal raised on the bridge side, before the kernel is
            -- reached, is the same class: one vocabulary either way.
            local b = failure(function() t:append(7) end)
            assert(b.kind == "validation", "kind: " .. tostring(b.kind))
            assert(b.method == "append", "method: " .. tostring(b.method))
        "#,
        )
        .exec()
        .expect("classified failures chunk");
    }

    /// The class did not cost the message.  A caller that only prints, or
    /// searches the text it caught, reads exactly what it read before — and
    /// the table stands in for the raised value wherever one was.
    #[test]
    fn a_classified_failure_still_reads_as_a_message() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ owner = "t" })
            s:close()
            local e, raised = failure(function() s:append({ kind = "note" }) end)

            local text = tostring(raised)
            assert(text:find("knl: append:", 1, true), "attribution: " .. text)
            assert(text:find("session is closed", 1, true), "reason: " .. text)
            assert(tostring(e) == text, "the table must render as its message")

            -- A raise that did not come from the kernel is reported whole
            -- rather than raising a second failure inside the handler.
            local other = knl.error("something else entirely")
            assert(other.kind == nil, "kind: " .. tostring(other.kind))
            assert(other.method == nil, "method: " .. tostring(other.method))
            assert(other.retryable == false)
            assert(other.message == "something else entirely",
                   "message: " .. tostring(other.message))

            -- …including one that merely looks like the shape.  Only a class
            -- the kernel publishes is read as one.
            local fake = knl.error("knl: append: nonsense: hello")
            assert(fake.kind == nil, "kind: " .. tostring(fake.kind))
            assert(fake.message == "knl: append: nonsense: hello")
        "#,
        )
        .exec()
        .expect("message compatibility chunk");
    }

    /// `knl.api().errors` is the kernel's class list itself, so the shell can
    /// hold its own declaration of the vocabulary against it instead of
    /// against a list somebody retyped.
    #[test]
    fn api_publishes_the_error_vocabulary() {
        let lua = vm();
        let published: Vec<String> = lua
            .load(r#"return knl.api().errors"#)
            .eval()
            .expect("read knl.api().errors");
        let declared: Vec<String> = knl::KnlError::KINDS
            .iter()
            .map(|kind| (*kind).to_string())
            .collect();
        assert_eq!(published, declared);

        // And every class a method's doc names is one of them, so the two
        // halves of the declaration cannot drift apart.
        for (name, doc) in SESSION_API.iter().chain(MODULE_API.iter()) {
            let Some((_, raises)) = doc.split_once("[raises: ") else {
                continue;
            };
            let raises = raises.split(']').next().unwrap_or("");
            for kind in raises.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                // A doc may add a clause after the list ("— only on a clean
                // exit"); the class is the first word of the entry.
                let kind = kind.split_whitespace().next().unwrap_or("");
                assert!(
                    knl::KnlError::KINDS.contains(&kind),
                    "{name} names a class the kernel does not publish: {kind:?}"
                );
            }
        }
    }

    /// A backend that is down surfaces as `storage`, not as the caller
    /// having done something wrong: the arguments were fine and the store
    /// could not do the work.
    #[test]
    fn a_store_that_is_down_surfaces_as_storage() {
        let (lua, _log) = vm_with_a_failing_store(2);
        lua.load(
            r#"
            local e = failure(function()
                do local s <close> = open_failing() end
            end)
            assert(e.kind == "storage", "kind: " .. tostring(e.kind))
            assert(e.method == "close", "method: " .. tostring(e.method))
            assert(e.retryable == false, "a store that is down is not a retry")
            assert(e.message == "the store is down", "message: " .. tostring(e.message))
        "#,
        )
        .exec()
        .expect("failing store chunk");
    }

    /// (I6) An explicit `close` wins: the scope exit that follows it is a
    /// no-op, so the reason in the log is the caller's and there is exactly
    /// one boundary.
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
        let finished: Vec<&Value> = log
            .iter()
            .filter(|e| e["kind"] == "session_closed")
            .collect();
        assert_eq!(finished.len(), 1, "exactly one boundary: {log:?}");
        assert_eq!(finished[0]["data"]["reason"], Value::from("done"));
    }

    /// (I6) The backstop: a handle that goes out of scope with no `<close>`
    /// and no explicit close still records the boundary when the collector
    /// reclaims it.  A session that ends by being forgotten is still an
    /// ended session, and a reader of the stream must not see it as open
    /// forever.
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
            Value::from("session_closed"),
            "the collector left the session open: {log:?}"
        );
        assert_eq!(last["data"]["reason"], Value::from("dropped"), "{last}");
    }

    // -- reading the log with SQL ------------------------------------------

    /// The fourth read face: one `SELECT` over the table the events live in.
    /// `$stream` is this session without the caller naming it, values are
    /// bound, and the second return says whether the cap cut anything off.
    #[test]
    fn query_reads_the_log_with_sql() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ owner = "q" })
            s:append({ kind = "msg_user", beat = "b1", data = { content = "hi" } })
            s:append({ kind = "note", meta = { label = "a" }, data = { text = "a note" } })

            local rows, truncated = s:query(
                "SELECT seq, kind FROM events WHERE stream = $stream ORDER BY seq")
            assert(#rows == 3, "rows: " .. tostring(#rows))
            assert(truncated == false, "nothing was cut off")
            assert(rows[1].kind == "session_opened", "first: " .. tostring(rows[1].kind))
            assert(rows[2].kind == "msg_user" and rows[2].seq == 2)
            assert(rows[3].kind == "note")

            -- A fold the kernel does not name is a query, not a view it had
            -- to be taught.
            local counted = s:query([[
                SELECT kind, COUNT(*) AS n FROM events
                WHERE stream = $stream GROUP BY kind ORDER BY kind]])
            assert(#counted == 3, "kinds: " .. tostring(#counted))

            -- The envelope is columns, so grouping a run by beat is a
            -- GROUP BY rather than a json path…
            local beats = s:query([[
                SELECT beat, COUNT(*) AS n FROM events
                WHERE stream = $stream AND beat IS NOT NULL GROUP BY beat]])
            assert(#beats == 1 and beats[1].beat == "b1" and beats[1].n == 1,
                   "beat is a column of its own")

            -- …while a kind's own shape is read out of `data`, and `meta`
            -- can be read without knowing the kind at all.
            local read = s:query([[
                SELECT json_extract(data, '$.content') AS content,
                       json_extract(meta, '$.label') AS label
                FROM events WHERE stream = $stream AND kind = 'msg_user']])
            assert(read[1].content == "hi", "data path: " .. tostring(read[1].content))
            assert(read[1].label == nil, "this one carried no meta")

            -- Values are bound: positionally…
            local one = s:query("SELECT kind FROM events WHERE kind = ?", { "note" })
            assert(#one == 1 and one[1].kind == "note", "positional bind")
            -- …and by name, with the prefix character left to SQLite.
            local named = s:query("SELECT kind FROM events WHERE kind = :kind",
                                  { kind = "msg_user" })
            assert(#named == 1 and named[1].kind == "msg_user", "named bind")

            -- A quote in a value is a character, not the end of a string, and
            -- a value that would be SQL if it were pasted in matches nothing.
            s:append({ kind = "it's odd" })
            local quoted = s:query("SELECT kind FROM events WHERE kind = ?", { "it's odd" })
            assert(#quoted == 1, "a quote in a bound value: " .. tostring(#quoted))
            local injected = s:query("SELECT kind FROM events WHERE kind = ?",
                                     { "x' OR 1=1 --" })
            assert(#injected == 0, "a bound value is never SQL: " .. tostring(#injected))

            -- The SQLite types come back as themselves, and a NULL column is
            -- absent rather than present-and-null, so it reads as nil.
            local typed = s:query(
                "SELECT 1 AS whole, 1.5 AS fraction, 'text' AS words, NULL AS absent")
            assert(typed[1].whole == 1 and typed[1].fraction == 1.5)
            assert(typed[1].words == "text")
            assert(typed[1].absent == nil, "a NULL column reads as nil")

            -- Reads keep working after the handle closed.
            s:close()
            assert(#s:query("SELECT 1 AS one") == 1, "a closed handle still reads")
        "#,
        )
        .exec()
        .expect("query chunk");
    }

    /// `$sessions` reads across the set it was given: two streams in one
    /// database, one statement.  This is what a session tree reads with.
    #[test]
    fn query_reads_across_the_session_set() {
        let lua = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path = path.to_str().expect("utf-8 path");

        lua.load(format!(
            r#"
            local path = "{path}"
            local a = knl.open({{ store = {{ sqlite = path }}, owner = "a" }})
            local b = knl.open({{ store = {{ sqlite = path }}, owner = "b" }})
            a:append({{ kind = "from_a" }})
            b:append({{ kind = "from_b" }})

            local sql = "SELECT stream, kind FROM events WHERE stream IN $sessions \
                         AND kind LIKE 'from_%' ORDER BY kind"

            -- Both streams, one statement.
            local both = a:query(sql, nil, {{ sessions = {{ a:id(), b:id() }} }})
            assert(#both == 2, "both streams: " .. tostring(#both))
            assert(both[1].kind == "from_a" and both[2].kind == "from_b")

            -- Left out, the set is the asking session's own stream.
            local mine = a:query(sql)
            assert(#mine == 1 and mine[1].kind == "from_a", "own stream only")

            -- An empty set is a mistake, not "all of them".
            local e = failure(function() a:query(sql, nil, {{ sessions = {{}} }}) end)
            assert(e.kind == "validation", "kind: " .. tostring(e.kind))
            assert(e.method == "query", "method: " .. tostring(e.method))
        "#
        ))
        .exec()
        .expect("session set chunk");
    }

    /// A query reads.  Anything that writes, and anything that is two
    /// statements, is refused as the caller's mistake — before the connection
    /// is reached, and on a connection that could not do it anyway.
    #[test]
    fn query_refuses_everything_that_is_not_one_read() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ owner = "q" })
            s:append({ kind = "note" })

            for _, sql in ipairs({
                "INSERT INTO events (stream) VALUES ('x')",
                "UPDATE events SET kind = 'x'",
                "DELETE FROM events",
                "DROP TABLE events",
                "PRAGMA table_info(events)",
                "ATTACH DATABASE '/tmp/other.db' AS other",
                "SELECT 1; DROP TABLE events",
            }) do
                local e = failure(function() s:query(sql) end)
                assert(e.kind == "validation", sql .. " -> " .. tostring(e.kind))
                assert(e.method == "query", sql .. " -> " .. tostring(e.method))
            end

            -- The log is exactly as it was.
            assert(s:len() == 2, "len after the refusals: " .. tostring(s:len()))

            -- And the arguments are checked too: a misspelt option is an
            -- error rather than a limit nobody applied.
            local e = failure(function() s:query("SELECT 1", nil, { rows = 10 }) end)
            assert(e.kind == "validation", "kind: " .. tostring(e.kind))
            local m = failure(function() s:query(42) end)
            assert(m.message:find("sql must be a string", 1, true), m.message)
        "#,
        )
        .exec()
        .expect("refusal chunk");
    }

    /// The row cap is reported, so a page can be told from a whole answer.
    #[test]
    fn query_caps_the_rows_and_says_when_it_cut() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ owner = "q" })
            for i = 1, 5 do s:append({ kind = "e" .. i }) end

            local rows, truncated = s:query(
                "SELECT kind FROM events ORDER BY seq", nil, { limit = 2 })
            assert(#rows == 2, "capped rows: " .. tostring(#rows))
            assert(truncated == true, "the cap cut rows off")

            local all, whole = s:query("SELECT kind FROM events ORDER BY seq", nil, { limit = 6 })
            assert(#all == 6 and whole == false, "nothing was cut off")
        "#,
        )
        .exec()
        .expect("limit chunk");
    }

    /// A query that will not finish is cut short and says so in its own
    /// class — "ask again" would be the wrong advice for a slow read.
    #[test]
    fn query_that_runs_too_long_reports_a_timeout() {
        let lua = vm();
        lua.load(
            r#"
            local s = knl.open({ owner = "q" })
            local e = failure(function()
                s:query([[WITH RECURSIVE forever(x) AS (
                              SELECT 1 UNION ALL SELECT x + 1 FROM forever)
                          SELECT COUNT(*) FROM forever]], nil, { timeout_ms = 50 })
            end)
            assert(e.kind == "timeout", "kind: " .. tostring(e.kind))
            assert(e.method == "query", "method: " .. tostring(e.method))
            assert(e.retryable == false, "a slow query is not a retry")

            -- The session is fine afterwards: a statement ended, not the
            -- reader.
            assert(#s:query("SELECT 1 AS one") == 1)
        "#,
        )
        .exec()
        .expect("timeout chunk");
    }

    /// `knl.api().schema` is the read contract: the table a query names, and
    /// its columns as SQLite reports them — including which two are the key.
    #[test]
    fn api_publishes_the_events_schema() {
        let lua = vm();
        lua.load(
            r#"
            local schema = knl.api().schema
            assert(schema.table == "events", "table: " .. tostring(schema.table))

            local names, keyed = {}, {}
            for _, column in ipairs(schema.columns) do
                assert(type(column.name) == "string" and #column.name > 0)
                assert(type(column.type) == "string" and #column.type > 0)
                table.insert(names, column.name)
                if column.pk then table.insert(keyed, column.name) end
            end
            assert(table.concat(names, ",")
                   == "stream,seq,epoch_ms,kind,schema_version,beat,meta,data",
                   "columns: " .. table.concat(names, ","))
            assert(table.concat(keyed, ",") == "stream,seq",
                   "primary key: " .. table.concat(keyed, ","))

            -- Every published column is one a query may actually name.
            local s = knl.open({ owner = "q" })
            local rows = s:query("SELECT " .. table.concat(names, ", ") ..
                                 " FROM " .. schema.table .. " WHERE stream = $stream")
            assert(#rows == 1, "the opening event: " .. tostring(#rows))
            assert(rows[1].kind == "session_opened")
            assert(rows[1].schema_version == 1, "the stored version is a column")
            assert(rows[1].beat == nil, "an undeclared beat is NULL")
            assert(type(rows[1].meta) == "string", "meta stays the stored text")
            assert(type(rows[1].data) == "string", "and so does data")
        "#,
        )
        .exec()
        .expect("schema chunk");
    }
}
