//! `knl.*` — Lua surface of the kernel syscall layer.
//!
//! This module is an adapter, nothing more.  The domain rules live in
//! [`crate::knl`] (pure Rust, unit-tested without a VM) and are stated in
//! that module's doc; here we only:
//!
//! 1. define the `Session` userdata and bind its methods,
//! 2. convert Lua tables ⇄ `serde_json::Value`,
//! 3. attribute failures as `knl: <method>: <kind>: <reason>`.
//!
//! Keeping the conversion in one place is what makes the re-entrancy
//! discipline checkable: walking a Lua table can call back into Lua, so
//! every conversion happens *outside* an active borrow of the session,
//! and the kernel core never sees a Lua value at all.
//!
//! # The Lua surface
//!
//! Five module functions and one userdata.
//!
//! - `knl.open(opts?) -> session` — `{ owner?, budget? = { amount, tag?,
//!   desc? }, store?, parent? }`.  State only: the policy half a beat runs
//!   against is built by the Lua kernel's own constructor, never here.
//!   `parent` opens the session *from* another one, on that one's database
//!   and out of its balance — `budget = { from_parent = n, tag? }` — which is
//!   one write: the child's opening and grant, and the parent's reservation.
//!   A balance that will not cover it records a refusal on the parent and
//!   raises `refused`, and no session is returned.
//! - `knl.resume(opts) -> session` — `{ session, store?, budget? }`: reopen a
//!   stream and re-fold it.  An absent `store` means what it means on open,
//!   the in-memory database.
//! - `knl.new_beat_id() -> string` — a time-ordered, session-free id for the
//!   caller to stamp on the events of one beat.
//! - `knl.error(e) -> { kind, method, retryable, message }` — read a raised
//!   failure back as data (see *Failures carry their class*).
//! - `knl.api() -> { session, module, errors, schema, types }` — the declared
//!   surface, the failure vocabulary, the columns a query may name, and the
//!   generated shapes of every argument and return (see *One declaration*).
//!
//! The session userdata answers `id`, `scope_id`, `owner`, `append`,
//! `events`, `len`, `view`, `query`, `reserve`, `spend`, `remaining`,
//! `exhausted` and `close`, plus the `__close` metamethod.  [`SESSION_API`]
//! and [`MODULE_API`] hold each one's contract in a line — including the
//! classes it can raise — and are what `knl.api()` hands back.
//!
//! # Everything that reaches the store yields
//!
//! `knl.open` / `knl.resume` and every session method that touches the log are
//! bound as **async** functions, so calling one suspends the coroutine rather
//! than stopping the VM.  That is not an optimization: the Lua VM's thread is
//! the only worker of its own runtime, so a syscall that waited there would
//! also stop every other coroutine on that VM, its timers and its
//! cancellation ([`crate::knl`] § Async).
//!
//! **Nothing about the Lua changes.**  A yield inside `pcall` / `xpcall`, a
//! `<close>` scope ending (cleanly or by error) and a `for` loop over beats
//! are all yieldable in Lua 5.4, so `s:append(...)` reads and behaves exactly
//! as it did.  The one requirement is the one the shell already meets: these
//! methods are reachable from inside a coroutine, which the main chunk and
//! every bus handler are.
//!
//! Three methods stay synchronous — `id`, `scope_id`, `owner` — because they
//! answer out of the value and wait for nothing, and so do `knl.new_beat_id`,
//! `knl.error` and `knl.api`.
//!
//! # The invariants the surface enforces
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
//!   the lifecycle that happened.  Three paths reach the closing boundary
//!   and the log never loses it: `close(reason?)` said
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
//! - **A child is paid for, and then it is on its own.**  `knl.open{ parent =
//!   s, budget = { from_parent = n } }` moves `n` out of `s`'s balance and
//!   opens a session on `s`'s database with `n` of its own, in one write:
//!   `s`'s ledger gains a `budget_reserved` naming the child, and the child's
//!   log opens with `parent` recorded on it.  Nothing comes back when the
//!   child closes — an allocation is a spend — and `s` holds no handle on it:
//!   the structure is in the log, and `knl.views.tree` reads it.  Closing a
//!   parent whose children are still open is not refused; the boundary
//!   records them (`session_closed.data.open_children`).
//! - **K2 model call.**  There is no composite call and the session keeps
//!   no backend of its own.  The driver reserves what it estimates the
//!   call will cost, calls the backend itself, appends the `llm_response`,
//!   and settles the difference with `spend`.
//!
//! # Driving a beat from Lua
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
//! # Reading the log
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
//! # Storage backend
//!
//! `knl.open` takes an optional `store`: absent or `"mem"`
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
//! # Failures carry their class
//!
//! A failure is raised as a message, because mlua raises every error a Rust
//! callback returns as its own userdata and offers no way to make a Lua table
//! *be* the raised value.  So the message is given a shape instead:
//!
//! ```text
//! knl: <method>: <kind>: <reason>
//!  │      │        │        └── prose, and the only part that is
//!  │      │        └── one of knl.api().errors (knl::KnlError::KINDS)
//!  │      └── the method that raised
//!  └── the fixed prefix
//! ```
//!
//! The first three fields are a closed vocabulary ([`knl::KnlError::KINDS`])
//! and only the fourth is prose.  `knl.error(e)` reads it back as
//! `{ kind, method, retryable, message }` — with `retryable` the kernel's own
//! judgement, true for `busy` alone — and an unattributed raise comes back
//! whole, `kind` absent and the entire text as `message`, so the reader never
//! fails on input it does not recognise.  `knl.api().errors` publishes the
//! class list, so the shell's own declaration of it is checked rather than
//! trusted.  A caller that only wants to print keeps working: the table
//! renders as the message it came from, and the message still contains what
//! it always did.
//!
//! # The declared surface, and the check that holds it
//!
//! What Lua can reach is [`SESSION_API`] plus [`MODULE_API`] and nothing
//! else, and that is a checked claim rather than a documented intention: a
//! test reflects over a live userdata and over the `knl` table and fails on a
//! method bound without an entry, and on an entry with no method.  A second
//! test holds `knl.api().schema` against the columns the store actually has.
//! The Lua kernel runs the mirror image of both, so a syscall added on one
//! side and not the other goes red rather than drifting.
//!
//! # One declaration: the Rust types
//!
//! The two tables above say what the methods are *called*; [`types`] says what
//! each one takes and answers, and it is the only place either is written
//! down.  Every argument is deserialized into one of those types
//! ([`from_lua`]) and every table a syscall answers with is built from one
//! ([`as_table`]), so the check runs on every call in both modes — including a
//! direct `s:append(...)`, which never passes through the Lua kernel's own dev
//! gate.
//!
//! The same types are rendered as lshape ([`lshape_module_source`], via
//! `schema-bridge`) and embedded as the Lua module `knl_types` at host start.
//! `knl.shapes.session` / `knl.shapes.module` point at that module rather than
//! restating it, and `knl.api().types` hands the source text back for tooling.
//! What this replaced was two declarations of one interface — these
//! signatures, and a hand-written lshape table beside them — held together by
//! a test that compared *names*, which is a test a renamed field walks
//! straight past.  The generated module is built at start rather than checked
//! in for the same reason: a generated file in the tree is one somebody can
//! edit, and an edited one is the second declaration all over again.

use mlua::prelude::*;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

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
         or { sqlite = path }); parent? opens a child on the parent's database with \
         budget = { from_parent = n, tag? }, moving n out of the parent's balance in one write \
         [raises: validation, refused, closed, busy, storage]",
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
        "api() -> { session = …, module = …, errors = { kind }, schema = { table, columns }, \
         fields = { amount, tag, … } } — the declared surface, the columns a query may name, and \
         the `data` paths a view reaches into",
    ),
];

/// The declared surface as Rust types — the single source both sides read.
///
/// # Why the types are the declaration
///
/// [`SESSION_API`] and [`MODULE_API`] say what the methods are *called* and
/// what each one is for; this module says what each one *takes and answers*.
/// Every entry above has its argument and return types here, and they are the
/// only place either is written down: the Lua kernel's own registry
/// (`knl.shapes.session` / `knl.shapes.module`) is built by pointing at
/// [`lshape_module_source`], which is generated from exactly these types at
/// host start.  Before this there were two declarations of one interface —
/// these signatures, and a hand-written lshape table beside them — and the
/// test that held them together compared *names*.  A misspelt field on either
/// side went unnoticed until a caller hit it.
///
/// # What that buys, beyond one declaration
///
/// The types are also the parser.  [`from_lua`] deserializes a caller's table
/// into them, so the check runs on every call in both modes rather than only
/// under the Lua dev gate, and a direct `s:append(...)` — which never passes
/// through that gate — is checked exactly like a call the shell made.
///
/// # The three shapes serde cannot state, and why
///
/// - [`Json`] / [`Meta`] — `data`, and a query's parameters, are opaque to the
///   kernel: their shape belongs to whoever writes the kind.  They map to
///   lshape's `any` and to a map of labels.
/// - [`StoreSpec`] / [`BudgetOpt`] — a string *or* a table, and two forms that
///   exclude each other.  `#[derive(SchemaBridge)]` renders an enum as its
///   variant names, which is right for [`ViewName`] and wrong for a union, so
///   these carry the schema by hand and the derive stays where it is honest.
/// - [`ViewName`] — the vocabulary is [`knl::VIEW_TAIL`] and stays the
///   kernel's: the schema is built *from* that constant rather than repeating
///   it, and an unknown name is still refused by the kernel, in its own words.
pub mod types {
    use schema_bridge::{Field, Schema, SchemaBridge};
    use serde::{Deserialize, Serialize};
    use serde_json::{Map, Value};
    use std::collections::BTreeMap;

    /// Opaque JSON: what a kind's `data` is about, and what a query binds.
    ///
    /// `Any` rather than a shape, and deliberately — the kernel records
    /// `data` as written and judges only its own six kinds, so a schema here
    /// would be this layer inventing a contract it does not hold anyone to.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct Json(pub Value);

    impl SchemaBridge for Json {
        fn to_ts() -> String {
            "unknown".to_string()
        }
        fn to_schema() -> Schema {
            Schema::Any
        }
    }

    /// One label in an event's `meta`: a string, a number or a flag.
    ///
    /// The whole of the vocabulary.  `meta` is the half of the envelope a view
    /// can read without ever being broken by a change to a kind, and a nested
    /// value would make it a second `data` with none of that promise.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(
        untagged,
        expecting = "a label: meta is shallow (a string, a number or a boolean)"
    )]
    pub enum MetaValue {
        /// A word.
        Text(String),
        /// A count or a measurement.
        Number(f64),
        /// A flag.
        Flag(bool),
    }

    impl SchemaBridge for MetaValue {
        fn to_ts() -> String {
            "string | number | boolean".to_string()
        }
        fn to_schema() -> Schema {
            Schema::Union(vec![Schema::String, Schema::Number, Schema::Boolean])
        }
    }

    /// An event's `meta`: labels, and only labels.
    pub type Meta = BTreeMap<String, MetaValue>;

    // -- scalars ------------------------------------------------------------
    //
    // A newtype rather than a bare `String` / `u64`, because the registry
    // entry that names one is what makes the surface readable: `id()` answers
    // a `SessionId`, not "a string".

    /// The stream a session writes — what `knl.resume` reopens.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct SessionId(pub String);

    /// The kernel-issued authority a stream is written under.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct ScopeId(pub String);

    /// The principal a scope belongs to (a real id, or `anon` / `system`).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct Owner(pub String);

    /// A beat id: time-ordered, session-free, and opaque to the kernel.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct BeatId(pub String);

    /// An event's position in its stream — assigned by the kernel, and the
    /// `from` a read starts at.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct Seq(pub u64);

    /// How many events are recorded.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct Count(pub u64);

    /// What `reserve` / `spend` move: a whole number of budget units.
    ///
    /// Signed, so a caller's negative lands here as a value the kernel refuses
    /// rather than as a deserializer's type error about `u64` — the amount is
    /// a number the kernel has a rule about, and the rule is the kernel's.
    #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct Amount(pub i64);

    /// The balance, or nothing at all when the session was granted no budget.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct Remaining(pub Option<i64>);

    /// Whether the budget is used up (`false` without one).
    #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct Exhausted(pub bool);

    /// The statement a read is written as: one `SELECT` or `WITH`.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct Sql(pub String);

    /// Which kind of ending a close was — a short word a reader can fold on.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct CloseReason(pub String);

    /// The sentence only this close can tell: the message of the error a
    /// caller's own bracket caught, say.  Truncated before it is recorded.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct CloseDetail(pub String);

    /// Whatever a raise handed over — the argument of `knl.error`, and what
    /// `__close` is given when its block is unwinding.
    ///
    /// `any`, because it is: the bridge's own attributed message, a Lua-side
    /// `error("...")`, or a value from somewhere else entirely.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Raised;

    impl SchemaBridge for Raised {
        fn to_ts() -> String {
            "unknown".to_string()
        }
        fn to_schema() -> Schema {
            Schema::Any
        }
    }

    /// The one named fold: `tail`.
    ///
    /// The vocabulary is [`crate::knl::projection::VIEW_TAIL`] and the schema is built
    /// from it, so there is no second list to keep in step.  A name that is
    /// not in it reaches the kernel and is refused there, which is where the
    /// vocabulary lives.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ViewName(pub String);

    impl SchemaBridge for ViewName {
        fn to_ts() -> String {
            format!("{:?}", crate::knl::projection::VIEW_TAIL)
        }
        fn to_schema() -> Schema {
            Schema::Enum(vec![crate::knl::projection::VIEW_TAIL.to_string()])
        }
    }

    /// What a named fold takes: `tail`'s `n`, and nothing else.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(deny_unknown_fields)]
    pub struct ViewOpts {
        /// How many events from the end.  Absent is the kernel's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub n: Option<u64>,
    }

    /// The backend a session's log lives in.
    ///
    /// `"mem"` is an in-memory database that lives as long as the session
    /// does; `{ sqlite = "<path>" }` is a durable stream in a file.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(
        untagged,
        expecting = r#"a store: "mem", or a table { sqlite = <path> }"#
    )]
    pub enum StoreSpec {
        /// A backend named by a word — `"mem"`, and nothing else.
        Named(String),
        /// A durable stream at a path.
        File(SqliteStore),
    }

    impl SchemaBridge for StoreSpec {
        fn to_ts() -> String {
            r#""mem" | { sqlite: string }"#.to_string()
        }
        fn to_schema() -> Schema {
            Schema::Union(vec![
                Schema::Enum(vec![MEM_STORE.to_string()]),
                SqliteStore::to_schema(),
            ])
        }
    }

    /// The in-memory backend, by name.
    pub const MEM_STORE: &str = "mem";

    /// `{ sqlite = "<path>" }` — the durable half of [`StoreSpec`].
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(deny_unknown_fields)]
    pub struct SqliteStore {
        /// Where the database file is.
        pub sqlite: String,
    }

    /// What `opts.budget` asked for.
    ///
    /// One table with both forms in it, because that is what a caller writes
    /// and because the refusal for writing both has to name both.  Which of
    /// the two a given table *is* — an owner's grant (`amount`) or an
    /// allocation out of a parent's balance (`from_parent`) — is decided
    /// after the parse, where the two can be named against each other.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct BudgetOpt {
        /// What an owner allows this session, out of nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub amount: Option<i64>,
        /// What the unit is called.  The kernel reads the number and this
        /// rides onto `budget_granted` verbatim.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tag: Option<String>,
        /// What was allowed and why.  A grant's alone: an allocation records
        /// the parent it came from, which is the whole of what the kernel
        /// knows about why it happened.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub desc: Option<String>,
        /// What the parent named in `opts.parent` hands over out of its own
        /// balance.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub from_parent: Option<i64>,
    }

    impl SchemaBridge for BudgetOpt {
        fn to_ts() -> String {
            format!("{} | {}", BudgetGrant::to_ts(), BudgetAllocation::to_ts())
        }
        /// The union the field really is, rather than the flat table it is
        /// parsed as: a reader of the schema should see that the two forms
        /// exclude each other, which the parse form cannot say.
        fn to_schema() -> Schema {
            Schema::Union(vec![
                BudgetGrant::to_schema(),
                BudgetAllocation::to_schema(),
            ])
        }
    }

    /// `{ amount, tag?, desc? }` — a balance appearing, which only an owner
    /// may do.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct BudgetGrant {
        /// A whole number of units.
        pub amount: i64,
        /// What the unit is called.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tag: Option<String>,
        /// What was allowed and why.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub desc: Option<String>,
    }

    /// `{ from_parent, tag? }` — a balance changing hands: the parent's falls
    /// by exactly what the child's rises by, in one write.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct BudgetAllocation {
        /// A whole number of units, out of the parent's balance.
        pub from_parent: i64,
        /// What the unit is called (the parent's, when it is left out).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tag: Option<String>,
    }

    /// `knl.open(opts?)` — state only.  Policy has its own constructor.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(deny_unknown_fields)]
    pub struct OpenOpts {
        /// The principal the session belongs to.  Absent is the reserved
        /// anonymous id, so the layer above always has a real key to read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub owner: Option<String>,
        /// The quota, and where it came from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub budget: Option<BudgetOpt>,
        /// Where the log lives.  Absent is the in-memory database — except
        /// for a child, which goes where its parent already is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub store: Option<StoreSpec>,
        /// The session this one is opened *from*.
        ///
        /// Declared, never read here.  The value is the kernel's own
        /// userdata — a live handle rather than data — so the key is taken
        /// off the table before the rest is deserialized (`without_parent`)
        /// and the handle itself is read directly (`parse_parent`).  The
        /// field stays because the *shape* has to say that a parent is part
        /// of `open`; it is `any` because no data schema can describe a
        /// handle.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub parent: Option<Json>,
    }

    /// `knl.resume(opts)` — reopen a stream and re-fold it.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(deny_unknown_fields)]
    pub struct ResumeOpts {
        /// Where the stream lives.  Absent means the same thing it does on
        /// open — the in-memory database — which is resumable for exactly as
        /// long as some handle is still holding it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub store: Option<StoreSpec>,
        /// The stream to reopen.
        pub session: String,
        /// The owner granting *again*: recorded and added to the balance the
        /// log already carries, rather than replacing it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub budget: Option<BudgetOpt>,
    }

    /// What `s:append` records: the envelope, and nothing beside it.
    ///
    /// Open, and deliberately: the kernel stamps `seq` / `epoch_ms` /
    /// `_schema_version` on a stored event, so what comes back out of
    /// `events()` carries more keys than what went in.  The closure — no
    /// other top-level key — is the kernel's, enforced at the syscall where
    /// the stamps are known.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct AppendEvent {
        /// What happened.
        pub kind: String,
        /// The beat this event belongs to, when the caller declared one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub beat: Option<String>,
        /// Shallow labels a view can group or filter on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub meta: Option<Meta>,
        /// What the kind is about.  An empty table when none was written.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub data: Option<Json>,
    }

    /// One recorded event, as it comes back out.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct EventRow {
        /// What happened.
        pub kind: String,
        /// Where in the stream, assigned by the kernel.
        pub seq: u64,
        /// When, assigned by the kernel.
        pub epoch_ms: u64,
        /// Which revision of the event vocabulary this was read through.
        pub _schema_version: u64,
        /// The beat this event belongs to, when one was declared.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub beat: Option<String>,
        /// The shallow labels it was written with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub meta: Option<Meta>,
        /// What the kind is about, as written.
        pub data: Json,
    }

    /// The record from a position on: what `s:events(from?)` answers.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(transparent)]
    pub struct EventRows(pub Vec<EventRow>);

    /// The values a statement binds.
    ///
    /// A list is the values for the `?` parameters, in order; a table with
    /// names is the values for `:name` / `@name` / `$name`.  A statement is
    /// written one way or the other and the two are not mixed.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(
        untagged,
        expecting = "the values a statement binds: a list for `?`, or a table of names"
    )]
    pub enum QueryParams {
        /// Values for the anonymous `?` parameters.
        Positional(Vec<Value>),
        /// Values for the named ones.
        Named(Map<String, Value>),
    }

    impl SchemaBridge for QueryParams {
        fn to_ts() -> String {
            "unknown[] | Record<string, unknown>".to_string()
        }
        fn to_schema() -> Schema {
            Schema::Union(vec![
                Schema::Array(Box::new(Schema::Any)),
                Schema::Record {
                    key: Box::new(Schema::String),
                    value: Box::new(Schema::Any),
                },
            ])
        }
    }

    /// What a caller asks for beyond the SQL itself.
    ///
    /// Closed: an option the kernel does not know must not quietly do
    /// nothing, which is exactly what a misspelt `limit` or `timeout_ms`
    /// would do.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, SchemaBridge)]
    #[serde(deny_unknown_fields)]
    pub struct QueryOpts {
        /// The streams `$sessions` expands to.  Omitted is this session's own
        /// stream and nothing else.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub sessions: Option<Vec<String>>,
        /// How long the read may run.  Absent is the kernel's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub timeout_ms: Option<u64>,
        /// How many rows before the rest are cut off.  Absent is the
        /// kernel's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<u64>,
    }

    /// What `s:query` answers: the rows, and whether the cap cut any off.
    ///
    /// A pair rather than a table, because that is what the call returns —
    /// two values, so a page can be told from a complete answer without
    /// unwrapping anything.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct QueryResult(pub Vec<Json>, pub bool);

    /// A raised kernel failure, read back as data (`knl.error(e)`).
    ///
    /// `kind` and `method` are optional because a raise that carried no
    /// attribution is reported whole rather than rejected: `message` then
    /// holds the entire text.  So `message` is the field a reader can always
    /// count on, and `kind` is the one it must ask for.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct ErrorTable {
        /// The class, when the raise carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub kind: Option<String>,
        /// The method that raised, when the raise carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub method: Option<String>,
        /// The kernel's own judgement, true for contention alone.
        pub retryable: bool,
        /// What went wrong, in prose.
        pub message: String,
    }

    /// One entry of the declared surface: a name and the contract it holds.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct ApiEntry {
        /// What Lua calls it.
        pub name: String,
        /// The contract, in a line.
        pub doc: String,
    }

    /// One column of the table a query reads.
    ///
    /// The one type here whose schema is written out rather than derived.
    /// `type` is a Rust keyword, so the field is `declared_type` and serde is
    /// told to rename it — and `#[derive(SchemaBridge)]` reads
    /// `serde(rename_all)` but not a per-field `serde(rename)`, so the derive
    /// would declare `declared_type` while the value carries `type`.  That is
    /// exactly the drift these types exist to remove, and the generated-shape
    /// test caught it, so the schema says what serde does.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ApiColumn {
        /// What SQL calls it.
        pub name: String,
        /// How SQLite declared it.
        #[serde(rename = "type")]
        pub declared_type: String,
        /// Whether it is part of the primary key.
        pub pk: bool,
    }

    impl SchemaBridge for ApiColumn {
        fn to_ts() -> String {
            "{ name: string; type: string; pk: boolean; }".to_string()
        }
        fn to_schema() -> Schema {
            Schema::Object(vec![
                Field::new("name", Schema::String),
                Field::new("type", Schema::String),
                Field::new("pk", Schema::Boolean),
            ])
        }
    }

    /// The read contract: the table a query names, and the columns it has.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct ApiSchema {
        /// The table the events live in.
        pub table: String,
        /// Its columns, as SQLite reports them.
        pub columns: Vec<ApiColumn>,
    }

    /// The `data` field names of the kinds the kernel writes itself.
    ///
    /// The columns are published above ([`ApiSchema`]) and they are only half
    /// of what a view has to spell: everything a `budget_*` or `session_*`
    /// event is *about* lives inside the `data` column, and a Lua view reaches
    /// it with a `json_extract` path — `knl.views.ledger` reads `$.amount` and
    /// `$.tag`, `knl.views.tree` reads `$.parent` and `$.open_children`.  Those
    /// paths are the Rust `FIELD_*` constants spelled out in SQL, in another
    /// language, in another file; nothing held them together, so a rename here
    /// would have left the view answering NULL for every row.
    ///
    /// So the names are published from the constants themselves, and the view
    /// is held against them where a store exists
    /// (`tests/fixtures/knl_beat_test.lua`, inv11) — the same two-sided
    /// arrangement the columns and the error classes already have.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct ApiFields {
        /// `budget_*`: how much, in the grant's unit.
        pub amount: String,
        /// `budget_*`: the grant's unit, when it named one.
        pub tag: String,
        /// `budget_granted`: the owner's free-text note.
        pub desc: String,
        /// `budget_refused`: the balance the refusal was measured against.
        pub remaining: String,
        /// `session_opened` / `budget_*`: the authority it was written under.
        pub scope_id: String,
        /// `session_opened`: the principal the scope belongs to.
        pub owner: String,
        /// `session_opened` / `budget_granted`: the stream this was opened
        /// from.
        pub parent: String,
        /// `budget_reserved` / `budget_refused`: the stream the units went to.
        pub child: String,
        /// `session_closed`: which kind of ending it was.
        pub reason: String,
        /// `session_closed`: the sentence only that close could tell.
        pub detail: String,
        /// `session_closed`: the children that had not ended when it did.
        pub open_children: String,
    }

    /// What `knl.api()` answers: the whole declared surface, as data.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaBridge)]
    pub struct ApiReport {
        /// Every method the session userdata answers to.
        pub session: Vec<ApiEntry>,
        /// Every function the `knl` global carries.
        pub module: Vec<ApiEntry>,
        /// The closed list of classes `knl.error(e).kind` can report.
        pub errors: Vec<String>,
        /// The columns a query may name.
        pub schema: ApiSchema,
        /// The `data` paths a view reaches into, as the kernel spells them.
        pub fields: ApiFields,
        /// The generated `knl_types` module, as source text — the same one
        /// the host embeds, for a tool that wants to read the surface
        /// without loading it.
        pub types: String,
    }

    /// Whether a stray key in a table is a violation or a pass-through.
    ///
    /// lshape's `T.shape` is open by default, which is right for the tables
    /// a caller writes and wrong for the two that are contracts: an option
    /// the kernel does not know must not quietly do nothing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Strict {
        /// Extra keys pass (lshape's default).
        Open,
        /// Extra keys are a violation.
        Closed,
    }

    /// Every type the syscall surface is declared in, with the name Lua sees.
    ///
    /// This list *is* the module `knl_types` — nothing is generated that is
    /// not here, and a test holds every entry of it against a reference from
    /// the Lua registry, so a type nobody declares and a declaration with no
    /// type are both failures.
    pub fn declared() -> Vec<(&'static str, Schema, Strict)> {
        vec![
            ("SessionId", SessionId::to_schema(), Strict::Open),
            ("ScopeId", ScopeId::to_schema(), Strict::Open),
            ("Owner", Owner::to_schema(), Strict::Open),
            ("BeatId", BeatId::to_schema(), Strict::Open),
            ("Seq", Seq::to_schema(), Strict::Open),
            ("Count", Count::to_schema(), Strict::Open),
            ("Amount", Amount::to_schema(), Strict::Open),
            ("Remaining", Remaining::to_schema(), Strict::Open),
            ("Exhausted", Exhausted::to_schema(), Strict::Open),
            ("Sql", Sql::to_schema(), Strict::Open),
            ("CloseReason", CloseReason::to_schema(), Strict::Open),
            ("CloseDetail", CloseDetail::to_schema(), Strict::Open),
            ("Raised", Raised::to_schema(), Strict::Open),
            ("ViewName", ViewName::to_schema(), Strict::Open),
            ("ViewOpts", ViewOpts::to_schema(), Strict::Closed),
            ("OpenOpts", OpenOpts::to_schema(), Strict::Open),
            ("ResumeOpts", ResumeOpts::to_schema(), Strict::Open),
            ("AppendEvent", AppendEvent::to_schema(), Strict::Open),
            ("EventRows", EventRows::to_schema(), Strict::Open),
            ("QueryParams", QueryParams::to_schema(), Strict::Open),
            ("QueryOpts", QueryOpts::to_schema(), Strict::Closed),
            ("QueryResult", QueryResult::to_schema(), Strict::Open),
            ("ErrorTable", ErrorTable::to_schema(), Strict::Open),
            ("ApiReport", ApiReport::to_schema(), Strict::Open),
        ]
    }
}

/// The `knl_types` Lua module, as source text.
///
/// Generated from [`types::declared`] at every host start rather than checked
/// in: a generated file in the tree is a file that can be edited, and one that
/// has been edited is a second declaration wearing the first one's name.  The
/// host adds it to the embedded module set ([`crate::host`]) so the Lua kernel
/// can `require("knl_types")`, and `knl.api().types` hands back this same text
/// for tooling that wants to read the surface without loading it.
///
/// `schema_bridge_lshape::generate_lshape_file` would do all of this in one
/// call, except that it has no way to emit lshape's strict mode
/// ([`types::Strict::Closed`]) — so the module is assembled here from that
/// crate's per-schema renderer, in the same layout, and the two tables whose
/// extra keys are violations get the option appended.
pub fn lshape_module_source() -> String {
    let mut out = String::from(
        "-- Generated at host start by agent-block-core from the argument and\n\
         -- return types of `bridge/knl.rs` (schema-bridge -> lshape). Not a file\n\
         -- in the tree: there is nothing here to edit, and so nothing to drift.\n\
         local T = require(\"lshape\").t\n\nlocal M = {}\n\n",
    );
    for (name, schema, strict) in types::declared() {
        let body = schema_bridge_lshape::schema_to_lshape(&schema)
            .unwrap_or_else(|e| unreachable!("knl_types {name} does not map to lshape: {e}"));
        let body = match strict {
            types::Strict::Open => body,
            types::Strict::Closed => close_shape(name, body),
        };
        out.push_str(&format!("M.{name} = {}\n\n", named_scalar(name, body)));
    }
    out.push_str("return M\n");
    out
}

/// A type whose schema is a bare primitive, given its name back.
///
/// `SessionId` and `Owner` are both strings, and lshape's `T.string` is one
/// table: rendered as they stand, the two names would be the same value, and
/// a registry that says `id() -> SessionId` and `owner() -> Owner` would be
/// saying one thing twice — with no way left to tell a type nobody references
/// from one that is referenced under another name.  `:describe` wraps the
/// primitive in a node of its own carrying the name, which `check` passes
/// straight through and `reflect` reads back, so the declaration keeps the
/// distinction the Rust type made.
///
/// Only the bare case: anything with a combinator in it (`T.one_of({…})`,
/// `T.shape({…})`, `T.integer:is_optional()`) is already a fresh table.
fn named_scalar(name: &str, body: String) -> String {
    let bare = body
        .strip_prefix("T.")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase()));
    if bare {
        format!("{body}:describe({name:?})")
    } else {
        body
    }
}

/// `T.shape({ … })` with lshape's strict mode turned on.
///
/// A text edit rather than a generator option because the generator has none
/// (schema-bridge-lshape 0.2 renders `T.shape(fields)` and stops).  It is
/// exact rather than approximate: the renderer's output for an object ends in
/// `})` and in nothing else, so the tail is replaced rather than searched for,
/// and a schema that did not render as a shape is a mistake in
/// [`types::declared`] rather than something to paper over.
fn close_shape(name: &str, body: String) -> String {
    let Some(fields) = body.strip_suffix("})") else {
        unreachable!("knl_types {name} is marked strict but did not render as a T.shape: {body}");
    };
    format!("{fields}}}, {{ open = false }})")
}

/// Read a Lua value as `T`, attributing a refusal to `method`.
///
/// The type is the check.  What a syscall accepts used to be a hand-written
/// walk of the table — one per argument, each with its own idea of how to say
/// "that is not a string" — and this is the whole of it now: the same types
/// the surface is declared in are the ones a caller's table is read into, so
/// the check and the declaration cannot disagree.
///
/// Three things make the refusal readable:
///
/// - `noun` names the argument (`opts`, `event`, `budget`), and
///   `serde_path_to_error` adds the field the deserializer was at, so a caller
///   gets `budget.tag: invalid type: number, expected a string` rather than
///   the leaf message alone;
/// - the class is [`knl::KnlError::VALIDATION`], the same one the kernel's own
///   validator uses, so the shell sees one vocabulary either side of the
///   boundary;
/// - nothing is turned off.  A value serde has no representation for — a
///   function where a string belonged — is refused rather than skipped, which
///   is what `lua_to_json` has always done on the way to the store.  The one
///   value that legitimately cannot cross is `opts.parent`, and it is lifted
///   off the table before this runs ([`without_parent`]) rather than bought
///   with a deserializer that ignores every other one too.
fn from_lua<T: serde::de::DeserializeOwned>(
    method: &str,
    noun: &str,
    value: LuaValue,
) -> LuaResult<T> {
    let de = mlua::serde::Deserializer::new(value);
    serde_path_to_error::deserialize(de).map_err(|error| {
        let path = error.path().to_string();
        let at = if path.is_empty() || path == "." {
            noun.to_string()
        } else {
            format!("{noun}.{path}")
        };
        // mlua renders every deserializer failure as `deserialize error: …`.
        // The class is already the third field of the attribution, so the
        // prefix would be the message saying twice what it is and once what
        // went wrong.
        let reason = error.into_inner().to_string();
        let reason = reason
            .strip_prefix("deserialize error: ")
            .unwrap_or(&reason)
            .to_string();
        err(method, format!("{at}: {reason}"))
    })
}

/// K5 session: the only handle the Lua side has on kernel state.
struct Session {
    // A `tokio::sync::Mutex`, and not the `RefCell` this used to be.  Every
    // method that reaches the store yields now, so a second coroutine on the
    // same VM can call one while the first is suspended in the middle of
    // another — which a `RefCell` answers by panicking.  An async lock answers
    // it by making the second call wait for the first, which is the same
    // serialization the kernel already promises per stream.
    //
    // Holding the guard across the store's `.await` is the point rather than a
    // hazard: a session's own calls are meant to be one at a time, and the
    // lock's whole job is to say so.
    state: Mutex<knl::Session>,
    /// The three identity reads, copied out at construction.
    ///
    /// `id` / `scope_id` / `owner` are immutable once this userdata exists —
    /// the stream is adopted before the value is built (`open_sqlite`,
    /// `resume_on`, `Session::open_child`, and `knl::Session::new` inside
    /// itself), and neither the scope id nor the owner has a setter at all —
    /// so the answer is a field here rather than a read behind the lock.
    ///
    /// That is what makes those three methods *never raise*, which is what
    /// [`SESSION_API`] says about them.  Behind the lock they could not:
    /// a `try_lock` has an answer for "somebody is mid-call" and every answer
    /// to that is wrong for an identity read — raising turns `s:id()` into a
    /// call a caller has to handle, and waiting is the one thing a
    /// synchronous method on the VM's thread must not do.
    identity: Identity,
}

/// What a session answers about itself without touching the store.
///
/// Fixed at construction and never written again, so the reads are plain
/// field reads and the lock stays for the calls that actually reach the log.
struct Identity {
    /// The stream this session writes (`s:id()`).
    id: String,
    /// The authority the stream is written under (`s:scope_id()`).
    scope_id: String,
    /// The principal the scope belongs to (`s:owner()`).
    owner: String,
}

impl Session {
    /// Wrap a kernel session as the Lua userdata.
    ///
    /// The identity is read *here*, which is why every caller adopts the
    /// stream id before handing the session over: after this line the three
    /// values are the userdata's own, and the kernel session's copy of them
    /// can no longer be reached from Lua.
    fn from_state(state: knl::Session) -> Self {
        let identity = Identity {
            id: state.id().to_string(),
            scope_id: state.scope_id().to_string(),
            owner: state.owner().to_string(),
        };
        Self {
            state: Mutex::new(state),
            identity,
        }
    }

    /// Open a session for `owner` with an optional budget grant, on the
    /// in-memory store.
    async fn new(
        owner: String,
        grant: Option<knl::BudgetGrant>,
        drivers: &knl::IsleDrivers,
    ) -> LuaResult<Self> {
        let state = knl::Session::new(owner, grant, drivers)
            .await
            .map_err(|e| knl_err("open", &e))?;
        Ok(Self::from_state(state))
    }
}

/// The backstop under `close` and `<close>`: a handle nobody ended still
/// records the session's boundary, here, where the value dies.
///
/// A dropped handle is the one close path with no caller left to tell, and now
/// also the one with nowhere to wait: `Drop` cannot be `async`, and this runs
/// on the VM's own thread inside a Lua collection cycle, where blocking on
/// SQLite would stop every other coroutine, timer and cancellation that VM
/// owns.  So the boundary is *submitted* rather than awaited
/// ([`knl::Session::close_detached`]): the event goes to the connection
/// thread, whose driver the host holds, and lands there while nothing waits
/// for it.
///
/// The lock is taken with `try_lock`, not awaited: a session still borrowed by
/// a suspended call has an owner, and this collection cycle is not it.  A
/// failure is a `warn!` and nothing else — panicking in `drop` would abort the
/// process, and a session already past its last reader is not worth that.
impl Drop for Session {
    fn drop(&mut self) {
        // `get_mut` rather than a lock at all: `Drop` has `&mut self`, so no
        // other holder of the session can exist by definition.
        let state = self.state.get_mut();
        if state.is_closed() {
            return;
        }
        state.close_detached(knl::CLOSE_REASON_DROPPED);
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
        // The three identity reads below are the only synchronous methods on
        // the session: each answers out of a field of the userdata itself
        // ([`Identity`]), touching neither the store nor the lock, which is
        // the work a sync `add_method` is still for.  They cannot fail, and
        // that is a property of where the value is kept rather than a promise
        // made about a lock nobody was supposed to be holding.  Everything
        // after them reaches the store, so everything after them yields.

        // s:id() -> string
        methods.add_method("id", |_, this, ()| Ok(this.identity.id.clone()));

        // s:scope_id() -> string
        //
        // The kernel-issued id of the scope this session is written under,
        // as recorded on `session_opened` and on every `budget_*` event.
        // Not `s:id()`: that names the stream, this names the authority the
        // stream is written under, and neither is a caller's to choose.
        methods.add_method("scope_id", |_, this, ()| Ok(this.identity.scope_id.clone()));

        // s:owner() -> string
        //
        // The principal the scope belongs to (a real id, or the reserved
        // "anon" / "system").  Total — never nil.
        methods.add_method("owner", |_, this, ()| Ok(this.identity.owner.clone()));

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
        methods.add_async_method("append", |lua, this, event: LuaValue| async move {
            // Two readings of one table, and they answer different questions.
            //
            // The first is the type: `kind` is a string, `beat` is a string,
            // `meta` holds labels and nothing deeper — the contract
            // `knl_types.AppendEvent` publishes, checked here on every call in
            // both modes rather than only under the Lua dev gate, which a
            // direct `s:append(...)` never passes through.
            let _: types::AppendEvent = from_lua("append", "event", event.clone())?;
            // The second is the object the kernel records.  It is the table
            // itself rather than the parse above, because the envelope's
            // closure is the kernel's rule and it is stated where the stamps
            // are known: a stray top-level key is refused there, and `seq` /
            // `epoch_ms` given by a caller are overwritten rather than
            // rejected.  Both conversions run before the session is reached —
            // walking a Lua table can call back into Lua.
            let obj = table_to_object(&lua, "append", "event", event)?;
            this.state
                .lock()
                .await
                .append(obj)
                .await
                .map_err(|e| knl_err("append", &e))
        });

        // s:events(from?) -> array of event tables (deep copy)
        //
        // K1: the returned tables are freshly built from the stored JSON
        // on every call, so mutating them cannot reach kernel state.
        methods.add_async_method("events", |lua, this, from: Option<u64>| async move {
            let selected = {
                let state = this.state.lock().await;
                state
                    .events(from.unwrap_or(0))
                    .await
                    .map_err(|e| knl_err("events", &e))?
                // The guard is released here, before the conversion below.
            };
            // The events come out of the kernel as `Current` — the proof that
            // they were read through the upcaster seam — and that proof stops
            // at this boundary: what Lua gets is a table, so the objects are
            // taken back out here, at the one place they leave the kernel.
            let selected: Vec<Value> = selected
                .into_iter()
                .map(|event| Value::Object(event.into_inner()))
                .collect();
            // The session is released above: json_to_lua re-enters Lua.
            json_to_lua(&lua, Value::Array(selected))
        });

        // s:len() -> number of recorded events
        methods.add_async_method("len", |_, this, ()| async move {
            let n = this
                .state
                .lock()
                .await
                .len()
                .await
                .map_err(|e| knl_err("len", &e))?;
            Ok(n as u64)
        });

        // s:view(name, opts?) -> projection (fresh table each call)
        //
        // `tail` (`opts.n` events from the end), and that is the whole
        // vocabulary: an unknown name is an error, because a projection the
        // kernel does not name is the shell's to build — from
        // `events(from)`, or as a query view over the published schema.
        methods.add_async_method(
            "view",
            |lua, this, (name, opts): (LuaValue, LuaValue)| async move {
                // The name's *type* is settled here and its vocabulary is
                // not: which folds exist is the kernel's, and an unknown one
                // is refused there, in its own words.
                let types::ViewName(name) = from_lua("view", "name", name)?;
                let opts = view_opts(from_lua("view", "opts", opts)?);
                let value = {
                    let mut state = this.state.lock().await;
                    state
                        .view(&name, opts.as_ref())
                        .await
                        .map_err(|e| knl_err("view", &e))?
                };
                // The session is released above: json_to_lua re-enters Lua.
                json_to_lua(&lua, value)
            },
        );

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
        methods.add_async_method(
            "query",
            |lua, this, (sql, params, opts): (LuaValue, LuaValue, LuaValue)| async move {
                // All three are read before the session is reached: walking a
                // Lua table can re-enter Lua.
                let types::Sql(sql) = from_lua("query", "sql", sql)?;
                let params = query_params(from_lua("query", "params", params)?);
                let opts = query_opts(from_lua("query", "opts", opts)?);

                let found = {
                    let state = this.state.lock().await;
                    state
                        .query(&sql, params, &opts)
                        .await
                        .map_err(|e| knl_err("query", &e))?
                };
                let rows: Vec<Value> = found.rows.into_iter().map(Value::Object).collect();
                // The session is released above: json_to_lua re-enters Lua.
                let rows = json_to_lua(&lua, Value::Array(rows))?;
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
        methods.add_async_method("reserve", |_, this, amount: LuaValue| async move {
            // Whole is the type's business, non-negative is the kernel's:
            // the balance rule belongs where the balance is.
            let types::Amount(amount) = from_lua("reserve", "amount", amount)?;
            let mut state = this.state.lock().await;
            let granted = state
                .reserve(amount)
                .await
                .map_err(|e| knl_err("reserve", &e))?;
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
        methods.add_async_method("spend", |_, this, amount: LuaValue| async move {
            let types::Amount(amount) = from_lua("spend", "amount", amount)?;
            this.state
                .lock()
                .await
                .spend(amount)
                .await
                .map_err(|e| knl_err("spend", &e))
        });

        // s:remaining() -> number or nil (no budget)
        //
        // Raises when the ledger cannot be read: a store that is down has no
        // balance to report, and both values this could otherwise return —
        // a stale number, or the nil that means "no budget here" — read as
        // facts a run would carry on spending against.
        methods.add_async_method("remaining", |_, this, ()| async move {
            this.state
                .lock()
                .await
                .remaining()
                .await
                .map_err(|e| knl_err("remaining", &e))
        });

        // s:exhausted() -> boolean (always false without a budget)
        //
        // Raises for the same reason `remaining` does: a `false` that meant
        // "the store could not be read" is the one answer a run must never
        // be handed, because it reads as "carry on".
        methods.add_async_method("exhausted", |_, this, ()| async move {
            this.state
                .lock()
                .await
                .exhausted()
                .await
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
        methods.add_async_method(
            "close",
            |_, this, (reason, detail): (LuaValue, LuaValue)| async move {
                let reason: Option<types::CloseReason> = from_lua("close", "reason", reason)?;
                let reason = reason.map(|types::CloseReason(text)| text);
                let detail: Option<types::CloseDetail> = from_lua("close", "detail", detail)?;
                let detail = detail.map(|types::CloseDetail(text)| truncated(&text));
                // A close whose `session_closed` append fails (a database
                // contended past its retries, a store that is gone) surfaces
                // here: the session stays open and the caller knows the
                // boundary was not recorded, instead of a silent closed=true
                // with no record.
                this.state
                    .lock()
                    .await
                    .close_with(reason.as_deref(), detail.as_deref())
                    .await
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
        methods.add_async_meta_method(
            LuaMetaMethod::Close,
            |_, this, error: LuaValue| async move {
                // Computed before the session is reached: nothing about the
                // error value is read while it is held.
                let unwinding = !matches!(error, LuaValue::Nil);
                let (reason, detail) = match error {
                    LuaValue::Nil => (knl::CLOSE_REASON_SCOPE_EXIT, None),
                    error => (knl::CLOSE_REASON_ERROR, Some(error_detail(&error))),
                };
                let mut state = this.state.lock().await;
                if state.is_closed() {
                    return Ok(());
                }
                let outcome = state.close_with(Some(reason), detail.as_deref()).await;
                match outcome {
                    Ok(()) => Ok(()),
                    Err(e) if unwinding => {
                        tracing::warn!(
                            session = %state.id(),
                            error = %e,
                            "knl: session_closed was not recorded; \
                             the block's own error is propagating instead"
                        );
                        Ok(())
                    }
                    Err(e) => Err(knl_err("close", &e)),
                }
            },
        );
    }
}

/// What `opts.budget` asked for, once the two forms have been told apart.
///
/// Two things a caller can mean by "this session's budget", and they are not
/// interchangeable: `amount` is an owner *granting* — a balance out of
/// nothing the kernel can account for, which only an owner may do — while
/// `from_parent` is an *allocation*, units moved out of the balance a parent
/// session already holds.  One is a quota appearing, the other is a quota
/// changing hands, so they are separated here and refused together
/// ([`budget_source`]).
enum BudgetSource {
    /// `{ amount, tag?, desc? }` — what an owner allows this session.
    Grant(knl::BudgetGrant),
    /// `{ from_parent, tag? }` — what the parent named in `opts.parent`
    /// hands over out of its own balance.
    FromParent(knl::Allocation),
}

/// Decide which of the two `budget` forms a caller wrote.
///
/// The table's *shape* was settled by the deserializer ([`types::BudgetOpt`]),
/// including the refusal of a misspelt field — a misspelt cap that reads as
/// "no cap" is exactly the failure a budget exists to prevent.  What is left
/// is the part no schema states: the two amounts exclude each other, and
/// naming both is refused rather than resolved by precedence, because "the
/// owner allows 100" and "the parent hands over 100" are different claims
/// about where a balance came from and a table that makes both says neither.
/// `desc` belongs to a grant alone — an allocation records the parent it came
/// from, which is the whole of what the kernel knows about why it happened.
fn budget_source(
    method: &str,
    budget: Option<types::BudgetOpt>,
) -> LuaResult<Option<BudgetSource>> {
    let Some(budget) = budget else {
        return Ok(None);
    };
    let types::BudgetOpt {
        amount,
        tag,
        desc,
        from_parent,
    } = budget;

    if let Some(from_parent) = from_parent {
        if amount.is_some() {
            return Err(err(
                method,
                "budget names both amount and from_parent: an owner's grant and an allocation \
                 out of a parent's balance are different claims about where the quota came from",
            ));
        }
        if desc.is_some() {
            return Err(err(
                method,
                "budget.desc belongs to an owner's grant; an allocation records the parent it \
                 came from instead",
            ));
        }
        if from_parent < 0 {
            return Err(err(
                method,
                format!(
                    "budget.from_parent must be a non-negative whole number, got {from_parent}"
                ),
            ));
        }
        return Ok(Some(BudgetSource::FromParent(knl::Allocation {
            amount: from_parent,
            tag,
        })));
    }

    let Some(amount) = amount else {
        return Err(err(
            method,
            "budget.amount is required (non-negative whole number), or budget.from_parent to \
             allocate out of a parent's balance",
        ));
    };
    if amount < 0 {
        return Err(err(
            method,
            format!("budget.amount must be a non-negative whole number, got {amount}"),
        ));
    }

    Ok(Some(BudgetSource::Grant(knl::BudgetGrant {
        amount,
        tag,
        desc,
    })))
}

/// Read `opts.budget` where only an owner's grant makes sense.
///
/// `knl.resume` reopens a stream that already exists, so there is no parent
/// to allocate from and no child being opened: an allocation there is a
/// caller reaching for the wrong call, and it is named as such rather than
/// silently read as a grant of the same size.
fn grant_only(
    method: &str,
    budget: Option<types::BudgetOpt>,
) -> LuaResult<Option<knl::BudgetGrant>> {
    match budget_source(method, budget)? {
        None => Ok(None),
        Some(BudgetSource::Grant(grant)) => Ok(Some(grant)),
        Some(BudgetSource::FromParent(_)) => Err(err(
            method,
            "budget.from_parent allocates from a parent's balance, which is what \
             open{ parent = … } does; this call takes an owner's grant (amount)",
        )),
    }
}

/// Read `opts.owner`: the principal the session belongs to.
///
/// Total: an absent owner is the reserved anonymous id rather than `nil`, so
/// the policy layer above the kernel always has a real key to read.  The
/// reserved ids are the kernel's own namespace and an untrusted Lua caller
/// must not claim one, or it could impersonate a reserved principal on
/// `session_opened`.  Compared against the consts, not literal strings, so the
/// guard tracks the kernel's definition.
fn owner_of(owner: Option<String>) -> LuaResult<String> {
    let Some(owner) = owner else {
        return Ok(knl::ANON.to_string());
    };
    if owner == knl::ANON || owner == knl::SYSTEM {
        return Err(err("open", format!("owner {owner:?} is reserved")));
    }
    Ok(owner)
}

/// The `params` of `s:query`, in the kernel's terms.
///
/// A list is the values for the `?` parameters, in order; a table with names
/// is the values for `:name` / `@name` / `$name`.  An absent or empty table is
/// neither: the statement is expected to have no parameters of its own.
fn query_params(params: Option<types::QueryParams>) -> knl::QueryParams {
    match params {
        None => knl::QueryParams::None,
        Some(types::QueryParams::Positional(values)) if values.is_empty() => knl::QueryParams::None,
        Some(types::QueryParams::Positional(values)) => knl::QueryParams::Positional(values),
        Some(types::QueryParams::Named(named)) if named.is_empty() => knl::QueryParams::None,
        Some(types::QueryParams::Named(named)) => knl::QueryParams::Named(named),
    }
}

/// The `opts` of `s:query`, in the kernel's terms.
///
/// An option the caller left out is the kernel's own default rather than a
/// value this layer picks: the deadline and the row cap are the store's
/// policy, and a second copy of either here would be a second thing to change.
/// An empty `sessions` list is passed through as the empty set, which the
/// kernel refuses in its own words rather than being read as "all of them".
fn query_opts(opts: Option<types::QueryOpts>) -> knl::QueryOpts {
    let Some(opts) = opts else {
        return knl::QueryOpts::default();
    };
    knl::QueryOpts {
        sessions: opts.sessions,
        timeout_ms: opts.timeout_ms.unwrap_or(knl::DEFAULT_TIMEOUT_MS),
        limit: opts.limit.map_or(knl::DEFAULT_LIMIT, |n| n as usize),
    }
}

/// The `opts` of `s:view`, as the object the kernel's projections read.
///
/// Built field by field rather than serialized, so the map holds exactly what
/// the caller named: an absent `n` is absent, and `tail` falls back to its own
/// default instead of being handed a null to interpret.
fn view_opts(opts: Option<types::ViewOpts>) -> Option<Map<String, Value>> {
    let opts = opts?;
    let mut out = Map::new();
    if let Some(n) = opts.n {
        out.insert("n".to_string(), Value::from(n));
    }
    Some(out)
}

/// The storage backend a session's log goes in.
enum StoreTarget {
    /// The in-memory store (the default): absent or `"mem"`.
    Mem,
    /// A durable SQLite stream at the given path.
    Sqlite(String),
}

/// Read `opts.store`: `"mem"` → in-memory, `{ sqlite = "<path>" }` → durable.
///
/// The two forms are the deserializer's ([`types::StoreSpec`]); what is left
/// here is the one word the union cannot state — that the only *named* store
/// is the in-memory one.  `method` names the caller (`open` / `resume`) for
/// attribution.
fn store_target(method: &str, spec: types::StoreSpec) -> LuaResult<StoreTarget> {
    match spec {
        types::StoreSpec::Named(name) if name == types::MEM_STORE => Ok(StoreTarget::Mem),
        types::StoreSpec::Named(name) => Err(err(
            method,
            format!(r#"unknown store {name:?} (expected "mem" or {{ sqlite = <path> }})"#),
        )),
        types::StoreSpec::File(file) => Ok(StoreTarget::Sqlite(file.sqlite)),
    }
}

/// Read `opts.parent`: the session this one is opened from, if any.
///
/// Taken off the table by hand, and before the rest of it is read as data: a
/// parent is a live handle rather than a value, and serde has no
/// representation for a userdata (the deserializer is told to drop it, which
/// is what lets an otherwise closed `opts` carry one).  Only its presence and
/// its type are settled here — whether it really is a kernel session, and
/// whether its balance covers what is being asked for, is answered where the
/// allocation runs, with the parent borrowed.
/// `opts` with its `parent` taken out, so the rest can be read strictly.
///
/// The companion of [`parse_parent`], and the reason [`from_lua`] can leave
/// the deserializer's defaults alone.  A parent is a live session userdata,
/// which serde cannot carry; the way to let one through would be to tell the
/// deserializer to skip every value it has no representation for, and that
/// would skip a function a caller wrote where a string belonged as well.  So
/// the one key that cannot cross is lifted out here — `parse_parent` already
/// holds the real handle — and what is left is read with nothing turned off.
///
/// A shallow copy, because `parent` is a top-level key: everything nested is
/// shared with the caller's table and read from it exactly as before.
fn without_parent(lua: &Lua, opts: &LuaValue) -> LuaResult<LuaValue> {
    let LuaValue::Table(table) = opts else {
        return Ok(opts.clone());
    };
    let rest = lua.create_table()?;
    for pair in table.clone().pairs::<LuaValue, LuaValue>() {
        let (key, value) = pair?;
        if let LuaValue::String(name) = &key {
            if name.to_str()? == "parent" {
                continue;
            }
        }
        rest.set(key, value)?;
    }
    Ok(LuaValue::Table(rest))
}

fn parse_parent(opts: &LuaValue) -> LuaResult<Option<LuaAnyUserData>> {
    let LuaValue::Table(opts) = opts else {
        return Ok(None);
    };
    match opts.get::<LuaValue>("parent")? {
        LuaValue::Nil => Ok(None),
        LuaValue::UserData(parent) => Ok(Some(parent)),
        other => Err(err(
            "open",
            format!(
                "parent must be a session (the userdata knl.open returns), got {}",
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
async fn open_sqlite(
    owner: String,
    grant: Option<knl::BudgetGrant>,
    path: &str,
    drivers: &knl::IsleDrivers,
) -> LuaResult<Session> {
    let stream = uuid::Uuid::new_v4().to_string();
    let store = knl::SqliteEventStore::open(std::path::Path::new(path), stream.clone(), drivers)
        .await
        .map_err(|e| knl_err("open", &e))?;
    let mut state = knl::Session::open_on(owner, grant, Box::new(store))
        .await
        .map_err(|e| knl_err("open", &e))?;
    state.adopt_id(stream);
    Ok(Session::from_state(state))
}

/// Reopen the stream `session_id` and resume it.
///
/// `store` is the backend the stream lives in — a file, or the in-memory
/// database of that name while some handle still holds it open.
/// `Session::resume` re-folds the log; the reopened stream's id is adopted so
/// `s:id()` matches the stream the caller named.
async fn resume_on(
    grant: Option<knl::BudgetGrant>,
    store: knl::SqliteEventStore,
    session_id: String,
) -> LuaResult<Session> {
    // Resumed with no grant, so nothing has been written yet when the check
    // below runs: a refused resume must leave the stream exactly as it found
    // it, and a `budget_granted` recorded before the refusal would be the
    // caller writing into a stream it was not allowed to touch.
    let mut state = knl::Session::resume(None, Box::new(store))
        .await
        .map_err(|e| knl_err("resume", &e))?;
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
        state
            .grant_more(grant)
            .await
            .map_err(|e| knl_err("resume", &e))?;
    }
    state.adopt_id(session_id);
    Ok(Session::from_state(state))
}

/// Open the store a child's stream will live in.
///
/// `named` is what the caller asked for and `parent_db` is where the parent
/// is; an absent `store` means the child goes where its parent already is,
/// which is the only answer that always works.  A store the caller *did*
/// name is opened as asked and handed to the kernel, which refuses it if it
/// turns out to be a different database — the check belongs there, next to
/// the transaction that would have to span both.
async fn open_child_store(
    named: Option<StoreTarget>,
    parent_db: &str,
    stream: &str,
    drivers: &knl::IsleDrivers,
) -> LuaResult<Box<dyn knl::EventStore>> {
    let store = match named {
        // The parent's database, addressed exactly as it was opened (a path,
        // or the in-memory database's shared-cache URI).
        None => knl::SqliteEventStore::open(std::path::Path::new(parent_db), stream, drivers).await,
        Some(StoreTarget::Sqlite(path)) => {
            knl::SqliteEventStore::open(std::path::Path::new(&path), stream, drivers).await
        }
        Some(StoreTarget::Mem) => knl::SqliteEventStore::open_memory(stream, drivers).await,
    };
    Ok(Box::new(store.map_err(|e| knl_err("open", &e))?))
}

/// Open a session from `parent`, paying for it out of the parent's balance —
/// the `knl.open{ parent = … }` path.
///
/// The parent is held for the whole of it: the allocation is one transaction
/// on the parent's store, so the parent's own calls wait for it exactly as
/// they wait for any other syscall of its own.
async fn open_child_session(
    lua: Lua,
    parent: LuaAnyUserData,
    owner: String,
    allocation: knl::Allocation,
    named_store: Option<StoreTarget>,
    drivers: knl::IsleDrivers,
) -> LuaResult<LuaAnyUserData> {
    let handle = parent.borrow::<Session>().map_err(|_| {
        err(
            "open",
            "parent must be a session returned by knl.open / knl.resume",
        )
    })?;
    let child = {
        let mut state = handle.state.lock().await;
        let parent_db = state
            .database()
            .ok_or_else(|| {
                err(
                    "open",
                    "the parent's store keeps a single stream, so there is no database to open a \
                     child on",
                )
            })?
            .to_string();
        // Minted here and adopted by the child below, so the id `s:id()`
        // reports is the stream the parent's log names as its child.
        let stream = uuid::Uuid::new_v4().to_string();
        let store = open_child_store(named_store, &parent_db, &stream, &drivers).await?;
        state
            .open_child(stream, owner, allocation, store)
            .await
            .map_err(|e| knl_err("open", &e))?
    };
    // The parent is released before Lua is re-entered to build the userdata.
    drop(handle);
    lua.create_userdata(Session::from_state(child))
}

/// Build a session userdata from `opts` — the body of `knl.open`.
///
/// `opts.owner` is the principal (default the reserved anonymous id),
/// `opts.budget` the grant (`{ amount, tag?, desc? }`), and `opts.store`
/// the backend (in-memory by default, or `{ sqlite = "<path>" }` for a
/// durable stream).
///
/// `opts.parent` is the other way to open: a session this one is opened
/// *from*, with `budget = { from_parent = n, tag? }` moving `n` out of that
/// session's balance in the same write that opens this one.  The two forms
/// are exclusive in both directions — a parent with an owner's grant would be
/// a child whose quota nobody paid for, and `from_parent` with no parent has
/// nowhere to take it from — so each is refused with the other named.
async fn open_session(
    lua: Lua,
    opts: LuaValue,
    drivers: knl::IsleDrivers,
) -> LuaResult<LuaAnyUserData> {
    // The parent comes off the table first and by hand: it is a live session
    // handle, which serde cannot carry (see `parse_parent`).
    let parent = parse_parent(&opts)?;
    // Everything else is read as data, in one step, by the same types the
    // surface is declared in.  The Lua value is consumed here and nothing of
    // it survives into the awaits below, which is the rule a `LuaTable` held
    // across a suspension point would break.
    let opts: types::OpenOpts = match opts {
        LuaValue::Nil => types::OpenOpts::default(),
        value => from_lua("open", "opts", without_parent(&lua, &value)?)?,
    };
    let owner = owner_of(opts.owner)?;
    let budget = budget_source("open", opts.budget)?;
    // An absent `store` is *not* the in-memory one here: a child with no
    // store goes where its parent already is, and a child that asked for
    // "mem" asked for a different database and is refused.  Telling the two
    // apart is why the question is asked as an Option.
    let named_store = opts
        .store
        .map(|spec| store_target("open", spec))
        .transpose()?;

    let Some(parent) = parent else {
        let grant = match budget {
            None => None,
            Some(BudgetSource::Grant(grant)) => Some(grant),
            Some(BudgetSource::FromParent(_)) => {
                return Err(err(
                    "open",
                    "budget.from_parent allocates out of a parent's balance, so it needs \
                     opts.parent: the session to open this one from",
                ));
            }
        };
        let session = match named_store.unwrap_or(StoreTarget::Mem) {
            StoreTarget::Mem => Session::new(owner, grant, &drivers).await?,
            StoreTarget::Sqlite(path) => open_sqlite(owner, grant, &path, &drivers).await?,
        };
        return lua.create_userdata(session);
    };

    let allocation = match budget {
        Some(BudgetSource::FromParent(allocation)) => allocation,
        _ => {
            return Err(err(
                "open",
                "a child's quota comes out of its parent's: opts.parent needs \
                 budget = { from_parent = n, tag? }",
            ));
        }
    };
    open_child_session(lua, parent, owner, allocation, named_store, drivers).await
}

/// Resume a persisted session — the body of `knl.resume`.
///
/// Requires `opts.session = "<stream id>"`.  `opts.store` says where the
/// stream lives — `{ sqlite = "<path>" }` for a durable one — and means what
/// it means on open when it is left out: the in-memory database, which is
/// reopenable for exactly as long as some handle is still holding it.
/// `opts.budget` is optional and means the owner grants
/// *again*: it is recorded and added to the balance the log already
/// carries, rather than replacing it.  The returned userdata is the same
/// one `knl.open` returns, only pre-loaded with the balance folded from the
/// ledger.
async fn resume_session(
    lua: Lua,
    opts: LuaValue,
    drivers: knl::IsleDrivers,
) -> LuaResult<LuaAnyUserData> {
    if matches!(opts, LuaValue::Nil) {
        return Err(err("resume", "opts must be a table with store and session"));
    }
    // Read as data, in one step, and consumed here: nothing of the Lua table
    // survives into the awaits below.
    let opts: types::ResumeOpts = from_lua("resume", "opts", opts)?;
    let grant = grant_only("resume", opts.budget)?;
    let store = match opts.store {
        None => StoreTarget::Mem,
        Some(spec) => store_target("resume", spec)?,
    };
    let session_id = opts.session;
    let store = match store {
        StoreTarget::Sqlite(path) => {
            knl::SqliteEventStore::open(std::path::Path::new(&path), session_id.clone(), &drivers)
                .await
        }
        // An in-memory stream is reopenable too, for as long as it exists:
        // the database is named after the stream, so a second handle on a
        // live one finds the same log.  It cannot outlive the process, and it
        // does not pretend to — a name nobody is holding open resumes as an
        // empty stream, which is refused for having no session in it.
        StoreTarget::Mem => knl::SqliteEventStore::open_memory(session_id.clone(), &drivers).await,
    }
    .map_err(|e| knl_err("resume", &e))?;
    let state = resume_on(grant, store, session_id).await?;
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

    let mut read = types::ErrorTable {
        kind: None,
        method: None,
        retryable: false,
        message: text.clone(),
    };

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
                read.method = Some(method.to_string());
                read.kind = Some(kind.to_string());
                read.retryable = knl::KnlError::kind_is_retryable(kind);
                read.message = message.to_string();
            }
        }
    }

    // Built from the declared type rather than field by field, so the table a
    // caller reads is the one `knl_types.ErrorTable` describes.
    let out = as_table(lua, "error", &read)?;

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

/// Build a Lua table from a declared type.
///
/// The other half of [`from_lua`]: what a syscall answers is built by
/// serializing the type the registry names, so a return cannot grow a field
/// the declaration does not have.  A failure here is the bridge's own bug
/// rather than the caller's, and it is attributed as `validation` all the same
/// — the vocabulary is closed and there is no class for "the kernel could not
/// describe itself".
fn as_table<T: serde::Serialize>(lua: &Lua, method: &str, value: &T) -> LuaResult<LuaTable> {
    match lua.to_value(value).map_err(|e| err(method, e))? {
        LuaValue::Table(table) => Ok(table),
        other => Err(err(
            method,
            format!(
                "the answer did not serialize as a table, got {}",
                other.type_name()
            ),
        )),
    }
}

/// The declared surface as a Lua table — the body of `knl.api()`.
///
/// [`types::ApiReport`], built from [`SESSION_API`], [`MODULE_API`],
/// [`knl::KnlError::KINDS`], the events table itself and
/// [`lshape_module_source`], so a caller reads what the kernel offers from the
/// same places the reflection test holds the registration to.
///
/// `errors` is the closed list of classes `knl.error(e).kind` can report.  It
/// is published for the same reason the two method lists are: the shell keeps
/// its own declaration of the vocabulary, and a declaration nobody can check
/// is one that drifts.  `fields` is the other half of the read contract: the
/// `data` paths a Lua view spells in `json_extract`, taken from the kernel's
/// own [`knl::FIELD_AMOUNT`] and friends.  `types` is the generated
/// `knl_types` module as source text — the same one the host embeds — so a
/// tool can read the argument and return shapes without loading them.
fn api(lua: &Lua, _: ()) -> LuaResult<LuaTable> {
    /// One `{ name, doc }` list, in declaration order.
    fn listed(entries: &[(&str, &str)]) -> Vec<types::ApiEntry> {
        entries
            .iter()
            .map(|(name, doc)| types::ApiEntry {
                name: (*name).to_string(),
                doc: (*doc).to_string(),
            })
            .collect()
    }

    // The read contract: the table a query names and the columns it has.
    // Read off SQLite itself (`PRAGMA table_info`) rather than written out
    // here, so the published schema is the schema — a caller's SQL is written
    // against columns that exist, and the shell's own declaration of them can
    // be checked instead of trusted.
    let schema = types::ApiSchema {
        table: knl::EVENTS_TABLE.to_string(),
        columns: knl::events_schema()
            .map_err(|e| knl_err("api", &e))?
            .into_iter()
            .map(|column| types::ApiColumn {
                name: column.name,
                declared_type: column.declared_type,
                pk: column.pk,
            })
            .collect(),
    };

    // The other half of the read contract: the `data` paths a view reaches
    // into, taken from the constants the writers use rather than retyped, so
    // a Lua `json_extract('$.amount')` can be held against the field the
    // kernel actually wrote.
    let fields = types::ApiFields {
        amount: knl::FIELD_AMOUNT.to_string(),
        tag: knl::FIELD_TAG.to_string(),
        desc: knl::FIELD_DESC.to_string(),
        remaining: knl::FIELD_REMAINING.to_string(),
        scope_id: knl::FIELD_SCOPE_ID.to_string(),
        owner: knl::FIELD_OWNER.to_string(),
        parent: knl::FIELD_PARENT.to_string(),
        child: knl::FIELD_CHILD.to_string(),
        reason: knl::FIELD_REASON.to_string(),
        detail: knl::FIELD_DETAIL.to_string(),
        open_children: knl::FIELD_OPEN_CHILDREN.to_string(),
    };

    let report = types::ApiReport {
        session: listed(SESSION_API),
        module: listed(MODULE_API),
        errors: knl::KnlError::KINDS.iter().map(|k| k.to_string()).collect(),
        schema,
        fields,
        types: lshape_module_source(),
    };
    as_table(lua, "api", &report)
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
///
/// `drivers` is where the connection threads a session opens are kept.  They
/// cannot belong to the session: the drop backstop hands its closing event to
/// the thread *after* the handle is gone, so a thread the store had already
/// stopped could not take it.  The host owns them for the length of a run and
/// drains them once at the end of it ([`knl::IsleDrivers`]).
///
/// `open` and `resume` are async because opening a stream waits — for the
/// connection thread to start, for the schema, for the opening events to land
/// — and this is called on the Lua VM's own thread, which must not wait for
/// anything.  Both are reachable only from inside a coroutine, which the main
/// chunk and every bus handler already are.
pub fn register(lua: &Lua, drivers: knl::IsleDrivers) -> LuaResult<()> {
    let knl_tbl = lua.create_table()?;

    // knl.open(opts?) -> Session userdata
    {
        let drivers = drivers.clone();
        knl_tbl.set(
            "open",
            lua.create_async_function(move |lua, opts: LuaValue| {
                let drivers = drivers.clone();
                open_session(lua, opts, drivers)
            })?,
        )?;
    }

    // knl.resume(opts) -> Session userdata (durable stream re-folded)
    knl_tbl.set(
        "resume",
        lua.create_async_function(move |lua, opts: LuaValue| {
            let drivers = drivers.clone();
            resume_session(lua, opts, drivers)
        })?,
    )?;

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

/// The generated declaration, held against the types it was generated from.
///
/// The claim this round makes is that there is one declaration of the syscall
/// surface and it is the Rust types.  These are the tests that make it a claim
/// rather than an intention: the module has to *load* under the lshape the
/// host ships, and a value built from each Rust type has to *pass* the shape
/// generated for it.  A field renamed on one side and not the other cannot
/// survive both.
#[cfg(test)]
mod generated_types {
    use super::types::*;
    use super::*;
    use serde_json::json;

    /// The vendored lshape, in dependency order: `luacats` needs `reflect`,
    /// and the aggregate needs all four.  The same sources the host embeds.
    const LSHAPE_PARTS: [(&str, &str); 4] = [
        ("lshape.t", include_str!("../../blocks/lib/lshape/t.lua")),
        (
            "lshape.reflect",
            include_str!("../../blocks/lib/lshape/reflect.lua"),
        ),
        (
            "lshape.check",
            include_str!("../../blocks/lib/lshape/check.lua"),
        ),
        (
            "lshape.luacats",
            include_str!("../../blocks/lib/lshape/luacats.lua"),
        ),
    ];

    const LSHAPE_ROOT: &str = include_str!("../../blocks/lib/lshape/init.lua");

    /// A VM with the vendored lshape on it and the generated module loaded,
    /// arranged the way the host arranges them.
    fn types_vm() -> (Lua, LuaTable, LuaFunction) {
        let lua = Lua::new();
        let package: LuaTable = lua.globals().get("package").expect("package");
        let loaded: LuaTable = package.get("loaded").expect("package.loaded");
        for (name, source) in LSHAPE_PARTS {
            let module: LuaValue = lua
                .load(source)
                .set_name(name)
                .eval()
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            loaded.set(name, module).expect("preload");
        }
        let root: LuaValue = lua
            .load(LSHAPE_ROOT)
            .set_name("lshape")
            .eval()
            .expect("lshape");
        loaded.set("lshape", root.clone()).expect("preload lshape");

        let module: LuaTable = lua
            .load(lshape_module_source())
            .set_name("knl_types")
            .eval()
            .expect("the generated module must load under the vendored lshape");
        let check: LuaFunction = lua
            .load(r#"return require("lshape").check.check"#)
            .eval()
            .expect("lshape.check.check");
        (lua, module, check)
    }

    /// The declared types, each with a value built from the Rust type it was
    /// generated from.
    ///
    /// One entry per name in [`types::declared`] — the test below holds the
    /// two lists against each other, so a type added there without a sample
    /// here is a failure rather than a gap nobody notices.
    fn samples(lua: &Lua) -> Vec<(&'static str, LuaValue)> {
        fn to(lua: &Lua, value: impl serde::Serialize) -> LuaValue {
            lua.to_value(&value).expect("a declared type serializes")
        }
        vec![
            ("SessionId", to(lua, SessionId("s-1".into()))),
            ("ScopeId", to(lua, ScopeId("scope-1".into()))),
            ("Owner", to(lua, Owner("user-42".into()))),
            ("BeatId", to(lua, BeatId("beat-1".into()))),
            ("Seq", to(lua, Seq(7))),
            ("Count", to(lua, Count(3))),
            ("Amount", to(lua, Amount(10))),
            ("Remaining", to(lua, Remaining(Some(90)))),
            ("Exhausted", to(lua, Exhausted(false))),
            ("Sql", to(lua, Sql("SELECT 1".into()))),
            ("CloseReason", to(lua, CloseReason("done".into()))),
            (
                "CloseDetail",
                to(lua, CloseDetail("the block raised".into())),
            ),
            // `Raised` is whatever a raise handed over, so the sample is a
            // value no other type would take.
            ("Raised", to(lua, json!({ "anything": [1, "at", true] }))),
            (
                "ViewName",
                to(lua, ViewName(crate::knl::projection::VIEW_TAIL.into())),
            ),
            ("ViewOpts", to(lua, ViewOpts { n: Some(5) })),
            (
                "OpenOpts",
                to(
                    lua,
                    OpenOpts {
                        owner: Some("user-42".into()),
                        budget: Some(BudgetOpt {
                            amount: Some(1000),
                            tag: Some("tokens".into()),
                            desc: Some("one nightly run".into()),
                            from_parent: None,
                        }),
                        store: Some(StoreSpec::File(SqliteStore {
                            sqlite: "/tmp/knl.db".into(),
                        })),
                        parent: None,
                    },
                ),
            ),
            (
                "ResumeOpts",
                to(
                    lua,
                    ResumeOpts {
                        store: Some(StoreSpec::Named(MEM_STORE.into())),
                        session: "s-1".into(),
                        budget: Some(BudgetOpt {
                            from_parent: Some(25),
                            tag: Some("tokens".into()),
                            amount: None,
                            desc: None,
                        }),
                    },
                ),
            ),
            (
                "AppendEvent",
                to(
                    lua,
                    AppendEvent {
                        kind: "msg_user".into(),
                        beat: Some("beat-1".into()),
                        meta: Some(Meta::from([
                            ("label".to_string(), MetaValue::Text("seed".into())),
                            ("n".to_string(), MetaValue::Number(1.0)),
                            ("on".to_string(), MetaValue::Flag(true)),
                        ])),
                        data: Some(Json(json!({ "content": "hi" }))),
                    },
                ),
            ),
            (
                "EventRows",
                to(
                    lua,
                    EventRows(vec![EventRow {
                        kind: "msg_user".into(),
                        seq: 2,
                        epoch_ms: 1_700_000_000_000,
                        _schema_version: 1,
                        beat: None,
                        meta: None,
                        data: Json(json!({ "content": "hi" })),
                    }]),
                ),
            ),
            (
                "QueryParams",
                to(lua, QueryParams::Positional(vec![json!("note")])),
            ),
            (
                "QueryOpts",
                to(
                    lua,
                    QueryOpts {
                        sessions: Some(vec!["s-1".into(), "s-2".into()]),
                        timeout_ms: Some(250),
                        limit: Some(10),
                    },
                ),
            ),
            (
                "QueryResult",
                to(
                    lua,
                    QueryResult(vec![Json(json!({ "kind": "msg_user" }))], true),
                ),
            ),
            (
                "ErrorTable",
                to(
                    lua,
                    ErrorTable {
                        kind: Some("closed".into()),
                        method: Some("append".into()),
                        retryable: false,
                        message: "the session is closed".into(),
                    },
                ),
            ),
            (
                "ApiReport",
                to(
                    lua,
                    ApiReport {
                        session: vec![ApiEntry {
                            name: "append".into(),
                            doc: "append(event) -> seq".into(),
                        }],
                        module: vec![ApiEntry {
                            name: "open".into(),
                            doc: "open(opts?) -> session".into(),
                        }],
                        errors: vec!["busy".into()],
                        schema: ApiSchema {
                            table: "events".into(),
                            columns: vec![ApiColumn {
                                name: "seq".into(),
                                declared_type: "INTEGER".into(),
                                pk: true,
                            }],
                        },
                        fields: ApiFields {
                            amount: "amount".into(),
                            tag: "tag".into(),
                            desc: "desc".into(),
                            remaining: "remaining".into(),
                            scope_id: "scope_id".into(),
                            owner: "owner".into(),
                            parent: "parent".into(),
                            child: "child".into(),
                            reason: "reason".into(),
                            detail: "detail".into(),
                            open_children: "open_children".into(),
                        },
                        types: "-- generated".into(),
                    },
                ),
            ),
        ]
    }

    /// (c) The module loads under the lshape the host ships, and exports one
    /// shape per declared type and nothing else.
    #[test]
    fn the_generated_module_exports_exactly_the_declared_types() {
        let (_lua, module, _check) = types_vm();

        let mut exported: Vec<String> = module
            .pairs::<String, LuaValue>()
            .map(|pair| pair.expect("a module entry").0)
            .collect();
        exported.sort();

        let mut expected: Vec<String> = declared()
            .into_iter()
            .map(|(name, _, _)| name.to_string())
            .collect();
        expected.sort();

        assert_eq!(exported, expected, "the generated module drifted");
    }

    /// (c) A value built from each Rust type passes the shape generated for
    /// it.  This is the test that the two agree: `to_value` of the fixture is
    /// what a syscall would hand Lua, and `check.check` is what the Lua
    /// kernel's dev gate runs.
    #[test]
    fn every_declared_type_accepts_a_value_built_from_its_rust_type() {
        let (lua, module, check) = types_vm();

        let samples = samples(&lua);
        let mut sampled: Vec<&str> = samples.iter().map(|(name, _)| *name).collect();
        sampled.sort_unstable();
        let mut expected: Vec<&str> = declared().into_iter().map(|(name, _, _)| name).collect();
        expected.sort_unstable();
        assert_eq!(
            sampled, expected,
            "every declared type needs a sample built from it"
        );

        for (name, value) in samples {
            let shape: LuaValue = module.get(name).expect("the shape of a declared type");
            let (ok, why): (bool, Option<String>) = check
                .call((value, shape))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(ok, "{name}: {}", why.unwrap_or_default());
        }
    }

    /// (c) And the shapes refuse: a closed options table is closed on the Lua
    /// side too, which is what makes the generated declaration worth running.
    #[test]
    fn the_generated_shapes_refuse_what_the_rust_types_refuse() {
        let (lua, module, check) = types_vm();

        let refused: [(&str, LuaValue); 3] = [
            // An option the kernel does not know must not quietly do nothing.
            (
                "QueryOpts",
                lua.to_value(&json!({ "rows": 10 })).expect("value"),
            ),
            // `n` counts events.
            (
                "ViewOpts",
                lua.to_value(&json!({ "count": 2 })).expect("value"),
            ),
            // `meta` is shallow.
            (
                "AppendEvent",
                lua.to_value(&json!({ "kind": "note", "meta": { "deep": { "no": 1 } } }))
                    .expect("value"),
            ),
        ];

        for (name, value) in refused {
            let shape: LuaValue = module.get(name).expect("the shape of a declared type");
            let (ok, _why): (bool, Option<String>) = check
                .call((value, shape))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!ok, "{name} accepted a value its Rust type refuses");
        }
    }

    /// (c) `knl.api().types` is the same text the host embeds, so a tool that
    /// asks the kernel what it takes reads the module that is actually loaded.
    #[test]
    fn the_api_publishes_the_module_it_generated() {
        let lua = Lua::new();
        register(&lua, knl::IsleDrivers::new()).expect("register knl");
        let published: String = lua
            .load(r#"return knl.api().types"#)
            .eval()
            .expect("knl.api().types");
        assert_eq!(published, lshape_module_source());
    }
}

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

    /// A Lua VM with the `knl` bridge on it, and the two things a session
    /// now needs around it: a runtime to yield into, and somewhere for its
    /// connection threads to be owned.
    ///
    /// Every session method that reaches the store suspends, so a chunk that
    /// calls one has to run as a coroutine on a runtime — [`Vm::exec`] is
    /// that, and it is why the chunks below say `vm.exec(...)` where they
    /// used to say `lua.load(...).exec()`.  The assertions inside them are
    /// unchanged.
    struct Vm {
        lua: Lua,
        /// The connection threads of every session the chunks open, held for
        /// the test's lifetime exactly as the host holds them for a run's.
        drivers: knl::IsleDrivers,
        rt: tokio::runtime::Runtime,
    }

    impl Vm {
        /// Fresh VM with only the `knl` bridge registered.
        fn new() -> Self {
            let lua = Lua::new();
            let drivers = knl::IsleDrivers::new();
            register(&lua, drivers.clone()).expect("register knl");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the VM to yield into");
            rt.block_on(async { lua.load(FIXTURES).exec_async().await })
                .expect("fixtures");
            Self { lua, drivers, rt }
        }

        /// Run `chunk` to completion, as a coroutine.
        fn exec(&self, chunk: &str) -> LuaResult<()> {
            self.rt
                .block_on(async { self.lua.load(chunk).exec_async().await })
        }

        /// Run `chunk` and take what it returns.
        fn eval<R: mlua::FromLuaMulti>(&self, chunk: &str) -> LuaResult<R> {
            self.rt
                .block_on(async { self.lua.load(chunk).eval_async::<R>().await })
        }

        /// Run a chunk that is expected to fail, returning the message.
        fn expect_err(&self, chunk: &str) -> String {
            self.exec(chunk)
                .expect_err("chunk was expected to fail")
                .to_string()
        }

        /// Drive `f` on this VM's runtime — for the assertions that read a
        /// store directly rather than through Lua.
        fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
            self.rt.block_on(f)
        }

        /// Let go of the VM and drain its connection threads.
        ///
        /// Dropping the Lua state collects every session userdata, which is
        /// where a handle nobody closed submits its boundary without waiting;
        /// shutting the drivers down is what waits for those writes to land.
        /// A test that reads the database afterwards calls this first.
        fn finish(self) {
            let Self { lua, drivers, rt } = self;
            drop(lua);
            let failures = rt.block_on(drivers.shutdown());
            assert!(
                failures.is_empty(),
                "the connection threads did not shut down cleanly: {failures:?}"
            );
        }
    }

    /// Fresh Lua VM with only the `knl` bridge registered.
    fn vm() -> Vm {
        Vm::new()
    }

    /// (Happy path) append assigns strictly increasing seq numbers, `len`
    /// tracks them, and `events()` exposes the caller fields plus the
    /// kernel-owned `seq` / `epoch_ms`.  Seq 1 is the kernel's own
    /// `session_opened`.
    #[test]
    fn append_assigns_monotonic_seq_and_len_tracks() {
        let vm = vm();
        vm.exec(
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
        .expect("happy path chunk");
    }

    /// (I1) No mutation API is reachable on the session userdata.
    #[test]
    fn session_exposes_no_mutation_api() {
        let vm = vm();
        vm.exec(
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
        .expect("mutation-surface chunk");
    }

    /// (I1) The table returned by `events()` is a deep copy: mutating it,
    /// including nested tables and the array itself, leaves the history
    /// untouched.
    #[test]
    fn events_returns_a_deep_copy() {
        let vm = vm();
        vm.exec(
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
        .expect("deep copy chunk");
    }

    /// (I1) `seq` / `epoch_ms` are kernel-owned: a caller-supplied value is
    /// overwritten rather than trusted.  There is no `author` field.
    #[test]
    fn kernel_owned_fields_override_caller_values() {
        let vm = vm();
        vm.exec(
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
        .expect("kernel-owned field chunk");
    }

    /// `events(from)` returns the tail with `seq >= from`.
    #[test]
    fn events_from_filters_by_seq() {
        let vm = vm();
        vm.exec(
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
        .expect("events(from) chunk");
    }

    /// (attribution) `append` rejects a missing / non-string `kind` and a
    /// non-table event, with `knl: append:` in the message.
    #[test]
    fn append_validates_event_shape_with_attributed_errors() {
        let vm = vm();

        let msg = vm.expect_err(r#"knl.open():append({ text = "no kind" })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("missing field `kind`"), "{msg}");

        let msg = vm.expect_err(r#"knl.open():append({ kind = 42 })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("event.kind"), "{msg}");
        assert!(msg.contains("expected a string"), "{msg}");

        let msg = vm.expect_err(r#"knl.open():append("not a table")"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("event:"), "{msg}");
        assert!(msg.contains("expected table"), "{msg}");

        // A rejected append leaves no trace in the history.
        vm.exec(
            r#"
            local s = knl.open()
            pcall(function() s:append({ text = "no kind" }) end)
            assert(s:len() == 1, "rejected append was recorded")
            assert(s:append({ kind = "ok" }) == 2, "seq must not be consumed by a failure")
        "#,
        )
        .expect("rejected-append chunk");
    }

    /// (I3) A negative `spend` is an error, attributed to `knl: spend:`,
    /// and leaves the balance untouched.
    #[test]
    fn spend_rejects_negative_amounts() {
        let vm = vm();
        let msg = vm.expect_err(
            r#"
            local s = knl.open({ budget = { amount = 100, tag = "beats" } })
            s:spend(-1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("non-negative"), "{msg}");

        vm.exec(
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
        .expect("negative-spend chunk");
    }

    /// (I3) `remaining` is non-increasing across a call sequence, is
    /// floored at zero, and `exhausted()` flips once the budget is used
    /// up.  `spend` itself answers nothing: the balance is `remaining()`.
    #[test]
    fn spend_is_monotonic_and_flips_exhausted() {
        let vm = vm();
        vm.exec(
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
        .expect("budget monotonicity chunk");
    }

    /// (I3) Without a budget, `remaining()` is nil, `spend` records nothing
    /// and the session is never exhausted.
    #[test]
    fn session_without_budget_reports_nil() {
        let vm = vm();
        vm.exec(
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
        .expect("no-budget chunk");
    }

    /// (attribution) Malformed `budget` options are rejected by
    /// `knl.open` itself.
    ///
    /// Two kinds of refusal meet here and the messages say which is which.
    /// The *shape* is the declared type's ([`types::BudgetOpt`], read by
    /// [`from_lua`]), so a misspelt field or a mistyped one names the path it
    /// was at — `opts.budget.tag` — and the whole set of fields it could have
    /// been.  The *rule* is the bridge's: a quota has to be there, it cannot
    /// be negative, and the two forms exclude each other.  No schema states
    /// any of those three, so they are checked after the parse and keep their
    /// own words.
    #[test]
    fn session_validates_budget_options() {
        let vm = vm();

        let msg = vm.expect_err(r#"knl.open({ budget = { amount = -1 } })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("budget.amount"), "{msg}");

        let msg = vm.expect_err(r#"knl.open({ budget = {} })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("required"), "{msg}");

        // A misspelt field is an error, not a silently ignored cap: the
        // failure a budget exists to prevent is exactly "the limit I set
        // was not read".  The refusal names the field and the set it is not
        // in, which is the deserializer reading the declared type.
        let msg = vm.expect_err(r#"knl.open({ budget = { tokens = 100 } })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("unknown field `tokens`"), "{msg}");
        assert!(msg.contains("`amount`"), "{msg}");

        let msg = vm.expect_err(r#"knl.open({ budget = { amount = 10, tag = 7 } })"#);
        assert!(msg.contains("opts.budget.tag"), "{msg}");
        assert!(msg.contains("expected a string"), "{msg}");

        let msg = vm.expect_err(r#"knl.open({ budget = { amount = 1.5 } })"#);
        assert!(msg.contains("opts.budget.amount"), "{msg}");

        // The words are optional, and carried verbatim when given.
        vm.exec(
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
        .expect("grant options chunk");

        let msg = vm.expect_err(r#"knl.open({ budget = 100 })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("opts.budget"), "{msg}");
        assert!(msg.contains("expected table"), "{msg}");

        let msg = vm.expect_err(r#"knl.open("nope")"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("opts:"), "{msg}");
        assert!(msg.contains("expected table"), "{msg}");
    }

    /// (I6) Two sessions share nothing: ids differ and history / budget
    /// of one is invisible to the other.
    #[test]
    fn two_sessions_are_independent() {
        let vm = vm();
        vm.exec(
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
        .expect("session independence chunk");
    }

    /// (I6) After `close()`, `append` and `spend` are errors; read-only
    /// methods keep working and `close()` is idempotent.
    #[test]
    fn closed_session_rejects_append_and_spend() {
        let vm = vm();

        let msg = vm.expect_err(
            r#"
            local s = knl.open()
            s:close()
            s:append({ kind = "after_close" })
        "#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        let msg = vm.expect_err(
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "beats" } })
            s:close()
            s:spend(1)
        "#,
        );
        assert!(msg.contains("knl: spend:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        // A closed session cannot be granted more either.
        let msg = vm.expect_err(
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "beats" } })
            s:close()
            s:reserve(1)
        "#,
        );
        assert!(msg.contains("knl: reserve:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");

        vm.exec(
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
        .expect("closed-session read chunk");
    }

    /// (I6) The bridge installs exactly one global and keeps no state
    /// there: a second VM starts with its own fresh session.
    #[test]
    fn state_lives_in_the_userdata_not_in_globals() {
        let vm_a = vm();
        vm_a.exec(
            r#"
            local s = knl.open()
            s:append({ kind = "in_vm_a" })
            assert(s:len() == 2)
            -- `knl` itself carries no session state.
            assert(knl.events == nil and knl.append == nil and knl.spend == nil)
        "#,
        )
        .expect("vm a chunk");

        let vm_b = vm();
        vm_b.exec(
            r#"
            local s = knl.open()
            assert(s:len() == 1, "a second VM starts with only its own session_opened")
            assert(s:events()[1].kind == "session_opened")
        "#,
        )
        .expect("vm b chunk");
    }

    /// The kernel brackets the session: `session_opened` on open,
    /// `session_closed` on close, with the caller's reason (or the default).
    #[test]
    fn session_boundaries_are_recorded_by_the_kernel() {
        let vm = vm();
        vm.exec(
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
        .expect("session boundary chunk");

        let msg = vm.expect_err(r#"knl.open():close({ not_a = "string" })"#);
        assert!(msg.contains("knl: close:"), "missing attribution: {msg}");
        assert!(msg.contains("reason:"), "{msg}");
        assert!(msg.contains("expected a string"), "{msg}");
    }

    /// The kernel checks the *envelope* of every event — the closed set of
    /// top-level keys, a string beat, a shallow meta, a table data — and the
    /// shape of a kind's own `data` is the writer's business, not its.
    #[test]
    fn the_envelope_is_validated_and_a_kinds_own_data_is_not() {
        let vm = vm();

        // A kind's own field at the top level: refused, and the message says
        // where it goes.
        let msg = vm.expect_err(r#"knl.open():append({ kind = "msg_user", content = "hi" })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("content"), "{msg}");
        assert!(msg.contains("under data"), "{msg}");

        // `meta` is shallow: nesting belongs under `data`.
        let msg =
            vm.expect_err(r#"knl.open():append({ kind = "note", meta = { deep = { a = 1 } } })"#);
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("meta is shallow"), "{msg}");

        let msg = vm.expect_err(r#"knl.open():append({ kind = "note", data = 7 })"#);
        assert!(msg.contains("data must be a table"), "{msg}");

        vm.exec(
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
        .expect("envelope chunk");
    }

    /// The budget ledger is the kernel's: Lua can read those events but not
    /// write them.  Appending one by hand would be granting yourself the
    /// quota your owner set, so it is refused and the balance does not
    /// move.
    #[test]
    fn lua_cannot_append_the_budget_kinds_by_hand() {
        let vm = vm();

        let msg = vm.expect_err(
            r#"
            local s = knl.open({ budget = { amount = 10, tag = "beats" } })
            s:append({ kind = "budget_reserved", data = { amount = 5 } })
        "#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("kernel only"), "{msg}");
        assert!(msg.contains("budget_reserved"), "{msg}");

        vm.exec(
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
        .expect("kernel-only kind chunk");
    }

    /// (K4) `reserve` is the decision point: it takes what fits, refuses
    /// what does not without moving the balance, and names the grant when
    /// it refuses.  Every answer is a fact in the log.
    #[test]
    fn reserve_grants_refuses_and_records_both() {
        let vm = vm();
        vm.exec(
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
        .expect("reserve chunk");

        // Without a budget every reservation is granted, and nothing is
        // recorded: a session with no quota keeps no ledger.
        vm.exec(
            r#"
            local s = knl.open()
            local ok, tag = s:reserve(999999)
            assert(ok == true and tag == nil, "no budget must grant everything")
            assert(s:len() == 1, "a session with no quota recorded a ledger event")
        "#,
        )
        .expect("no-budget reserve chunk");

        let msg = vm.expect_err(r#"knl.open({ budget = { amount = 10 } }):reserve(-1)"#);
        assert!(msg.contains("knl: reserve:"), "missing attribution: {msg}");
        assert!(msg.contains("non-negative"), "{msg}");

        let msg = vm.expect_err(r#"knl.open():reserve("many")"#);
        assert!(msg.contains("knl: reserve:"), "missing attribution: {msg}");
    }

    /// The counter is a cache of the log: folding `granted − reserved −
    /// spent` over what Lua can read reproduces `remaining()` exactly.
    #[test]
    fn the_balance_lua_reads_is_the_fold_of_the_ledger() {
        let vm = vm();
        vm.exec(
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
        .expect("fold chunk");
    }

    /// The session's own boundaries are the kernel's: Lua can read them but
    /// not write them.  Hand-appending either would be claiming an opening
    /// the stream never had, or an ending it never reached, so both are
    /// refused and the session stays open.
    #[test]
    fn lua_cannot_append_the_session_boundary_kinds_by_hand() {
        let vm = vm();

        let msg = vm.expect_err(
            r#"
            local s = knl.open()
            s:append({ kind = "session_closed", data = { reason = "carried over" } })
        "#,
        );
        assert!(msg.contains("knl: append:"), "missing attribution: {msg}");
        assert!(msg.contains("kernel only"), "{msg}");
        assert!(msg.contains("session_closed"), "{msg}");

        vm.exec(
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
        .expect("session boundary kind chunk");
    }

    /// `knl.new_beat_id()` mints the beat id the shell stamps on its events:
    /// a fresh non-empty string every call, needing no session, and ordered
    /// by the time it was minted (UUID v7) so a stream's beats sort the way
    /// they happened.
    #[test]
    fn new_beat_id_mints_distinct_time_ordered_ids() {
        let vm = vm();
        vm.exec(
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
        .expect("new_beat_id chunk");
    }

    /// `view("tail", { n = k })` returns the last k events verbatim.
    #[test]
    fn view_tail_returns_the_last_events() {
        let vm = vm();
        vm.exec(
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
        .expect("tail view chunk");

        // `n` counts events, so a negative one is not a value the fold has to
        // have an opinion about: the declared type says whole and unsigned
        // and the refusal names the field it was reading.
        let msg = vm.expect_err(r#"knl.open():view("tail", { n = -1 })"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("opts.n"), "{msg}");

        // And an option `tail` does not have is a typo rather than a knob
        // that quietly does nothing.
        let msg = vm.expect_err(r#"knl.open():view("tail", { count = 2 })"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("unknown field `count`"), "{msg}");
    }

    /// (attribution) The view vocabulary is closed: an unknown name is an
    /// error, and so is a non-string name or non-table opts.
    #[test]
    fn view_rejects_unknown_names_and_bad_arguments() {
        let vm = vm();

        let msg = vm.expect_err(r#"knl.open():view("dialog")"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains(r#"unknown view "dialog""#), "{msg}");

        // The token account is one of the names the kernel does not have:
        // it reads the `data` of an `llm_response`, so it is a query view in
        // Lua (`knl.views.usage`) over the published schema.
        let msg = vm.expect_err(r#"knl.open():view("usage")"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains(r#"unknown view "usage""#), "{msg}");

        let msg = vm.expect_err(r#"knl.open():view(42)"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("name:"), "{msg}");
        assert!(msg.contains("expected a string"), "{msg}");

        let msg = vm.expect_err(r#"knl.open():view("tail", "n=2")"#);
        assert!(msg.contains("knl: view:"), "missing attribution: {msg}");
        assert!(msg.contains("opts:"), "{msg}");
        assert!(msg.contains("expected table"), "{msg}");
    }

    /// (I1) A view is a fresh table every call: mutating it cannot reach
    /// the history.
    #[test]
    fn view_returns_a_fresh_table_each_call() {
        let vm = vm();
        vm.exec(
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
        .expect("view copy chunk");
    }

    /// `store = "mem"` is the in-memory default spelled out; an unknown
    /// store string is a `knl: open:` error.
    #[test]
    fn store_mem_is_the_default_and_unknown_stores_are_rejected() {
        let vm = vm();
        vm.exec(
            r#"
            local s = knl.open({ store = "mem", owner = "x", budget = { amount = 10, tag = "beats" } })
            assert(s:len() == 2, "mem store opens like the default: session_opened + the grant")
            assert(s:owner() == "x")
            assert(s:append({ kind = "note" }) == 3)
        "#,
        )
        .expect("mem store chunk");

        let msg = vm.expect_err(r#"knl.open({ store = "postgres" })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("unknown store"), "{msg}");

        // A table that is not the durable form is refused with both forms
        // named, which is the union the declared type states.
        let msg = vm.expect_err(r#"knl.open({ store = { redis = "x" } })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("opts.store"), "{msg}");
        assert!(msg.contains("sqlite"), "{msg}");
    }

    /// The parent is the one thing in `opts` that is a handle rather than a
    /// value, and it is the *only* one: everything else is read as data, with
    /// nothing turned off.
    ///
    /// The two halves are one decision.  Letting a userdata through by telling
    /// the deserializer to skip what it cannot represent would also skip a
    /// function written where a string belonged — the option would read as
    /// absent and the session would open with the default.  So `parent` comes
    /// off the table by hand and the rest is read strictly.
    #[test]
    fn the_parent_is_the_only_value_read_as_a_handle() {
        let vm = vm();

        vm.exec(
            r#"
            local p = knl.open({ owner = "p", budget = { amount = 10, tag = "beats" } })
            local c = knl.open({ owner = "c", parent = p, budget = { from_parent = 4 } })
            assert(c:remaining() == 4, "the child's balance: " .. tostring(c:remaining()))
            assert(p:remaining() == 6, "the parent paid: " .. tostring(p:remaining()))
        "#,
        )
        .expect("parent chunk");

        // A function where a string belonged is refused, naming the field —
        // not read as "no owner given".
        let msg = vm.expect_err(r#"knl.open({ owner = function() end })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("opts.owner"), "{msg}");

        // And `parent` is still only accepted as a session.
        let msg = vm.expect_err(r#"knl.open({ parent = "s-1", budget = { from_parent = 1 } })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("must be a session"), "{msg}");
    }

    /// (Fix 6) The reserved owner ids are the kernel's own namespace: an
    /// untrusted Lua caller cannot claim "system" or "anon" (a spoofing hole
    /// for the future permission layer), each rejected as a `knl: open:` error.
    /// An unspecified owner still defaults to the kernel-assigned "anon", and a
    /// real principal id is accepted verbatim.
    #[test]
    fn open_rejects_reserved_owner_ids_from_the_caller() {
        let vm = vm();

        let msg = vm.expect_err(r#"knl.open({ owner = "system" })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("reserved"), "{msg}");
        assert!(msg.contains("system"), "{msg}");

        let msg = vm.expect_err(r#"knl.open({ owner = "anon" })"#);
        assert!(msg.contains("knl: open:"), "missing attribution: {msg}");
        assert!(msg.contains("reserved"), "{msg}");
        assert!(msg.contains("anon"), "{msg}");

        vm.exec(
            r#"
            -- Unspecified owner is the kernel-assigned reserved anon.
            assert(knl.open():owner() == "anon", "default owner must be anon")
            assert(knl.open({}):owner() == "anon", "empty opts default owner must be anon")
            -- A real principal id is accepted verbatim.
            assert(knl.open({ owner = "alice" }):owner() == "alice", "owner not carried")
        "#,
        )
        .expect("reserved-owner chunk");
    }

    /// (scope) A session has a scope: `s:scope_id()` is a real kernel-issued
    /// string, it is not the session id, and it is what `session_opened` and
    /// every `budget_*` event were written under.  Two runs are two scopes.
    #[test]
    fn a_session_reports_its_scope_id_and_records_it_on_the_log() {
        let vm = vm();
        vm.exec(
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
        .expect("scope id chunk");
    }

    /// (scope, durable) The scope outlives the process: a reopened stream
    /// resumes under the id its `session_opened` recorded — not a fresh one —
    /// and the ledger it goes on writing names that same scope.
    #[test]
    fn a_resumed_session_keeps_the_scope_id_the_log_recorded() {
        let vm = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path = path.to_str().expect("utf-8 path");

        vm.exec(&format!(
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
        let vm = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path = path.to_str().expect("utf-8 path");

        vm.exec(&format!(
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
        .expect("durable open/resume chunk");
    }

    /// (owner namespace) resume holds the same reserved-principal line as
    /// open: a stream the host opened as SYSTEM cannot be reopened from
    /// Lua, or an untrusted caller could write into the reserved namespace
    /// through the resume side door.
    #[test]
    fn resume_rejects_a_reserved_system_owned_stream() {
        let vm = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        // The host side (Rust) legitimately opens a SYSTEM-owned stream, on
        // the same collection of connection threads the VM's sessions use.
        let stream = "system-stream".to_string();
        let drivers = vm.drivers.clone();
        vm.block_on(async {
            let store = crate::knl::SqliteEventStore::open(&path, stream.clone(), &drivers)
                .await
                .expect("open store");
            let state =
                crate::knl::Session::open_on(crate::knl::SYSTEM.to_string(), None, Box::new(store))
                    .await
                    .expect("open system session");
            drop(state);
        });

        // Lua resuming it is refused, exactly as claiming SYSTEM at open is.
        let msg = vm.expect_err(&format!(
            r#"knl.resume({{ store = {{ sqlite = "{path_str}" }}, session = "{stream}" }})"#
        ));
        assert!(
            msg.contains("reserved"),
            "must name the reserved owner: {msg}"
        );
    }

    /// (attribution) resume needs a session id, and a stream that is one:
    /// each missing piece is a `knl: resume:` error.
    #[test]
    fn resume_requires_a_session_id_and_a_stream_that_holds_a_session() {
        let vm = vm();

        let msg = vm.expect_err(r#"knl.resume()"#);
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");

        let msg = vm.expect_err(r#"knl.resume({ store = { sqlite = "/tmp/x.db" } })"#);
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");
        assert!(msg.contains("missing field `session`"), "{msg}");

        // A name nobody is holding open is an empty stream, not a session:
        // an in-memory database exists only while a handle does, so resuming
        // one that has gone is refused for having no opening in it — the same
        // answer a fresh file gives.
        let msg = vm.expect_err(r#"knl.resume({ session = "never-opened" })"#);
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");
        assert!(msg.contains("no session to resume"), "{msg}");
    }

    /// (mem) An in-memory session is a session: it is resumable while it is
    /// alive, by the id it reports, and the resumed handle reads the same log
    /// and continues the same ledger.  What it cannot do is outlive the
    /// process, and nothing here pretends otherwise.
    #[test]
    fn an_in_memory_stream_is_resumable_while_it_is_open() {
        let vm = vm();
        vm.exec(
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
        .expect("in-memory resume chunk");
    }

    // -- session lifecycle: `<close>` and the drop backstop ----------------
    //
    // Every one of these reads the boundary back out of a *reopened* SQLite
    // stream rather than off the handle that wrote it: the question is
    // whether the record landed, and only the durable log answers that.

    /// The persisted events of `stream`, read through a fresh connection.
    ///
    /// A runtime of its own, and a collection of its own that is drained
    /// before the rows are handed back: this is a plain read, and it should
    /// leave nothing running behind it.
    fn persisted(path: &std::path::Path, stream: &str) -> Vec<Value> {
        use crate::knl::EventStore;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime to read on");
        rt.block_on(async {
            let drivers = knl::IsleDrivers::new();
            let store = crate::knl::SqliteEventStore::open(path, stream, &drivers)
                .await
                .expect("reopen the stream");
            let log = store.read(0, usize::MAX).await.expect("read the stream");
            drop(store);
            assert!(drivers.shutdown().await.is_empty(), "the reader joined");
            log
        })
    }

    /// Run `chunk` in a fresh VM and return the session id it yields.
    ///
    /// The VM is dropped *and its connection threads drained* before the
    /// caller reads the stream: collecting the Lua state is what makes the
    /// drop backstop submit its boundary, and draining the threads is what
    /// waits for that submitted write to land.  Only then is the log
    /// inspected.
    fn stream_id_from(chunk: String) -> String {
        let vm = vm();
        let id = vm.eval::<String>(&chunk).expect("close scope chunk");
        vm.finish();
        id
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
    /// The shared log a [`FlakyStore`] writes to.
    ///
    /// `Arc<tokio::sync::Mutex<_>>` rather than the `Rc<RefCell<_>>` it used
    /// to be: an [`knl::EventStore`] is `Send + Sync` now (the durable one's
    /// calls travel to a connection thread), and the SPI is `async`, so the
    /// lock has to be one that may be held across a suspension point.
    type SharedLog = std::sync::Arc<Mutex<knl::MemEventStore>>;

    struct FlakyStore {
        /// The real log, shared with the test so it can be read after the
        /// session that owned it is gone.
        inner: SharedLog,
        /// Which append (1-based) fails; `0` fails none.
        fails_on: usize,
        /// How many appends have been attempted.
        attempts: std::sync::atomic::AtomicUsize,
    }

    impl FlakyStore {
        /// A store whose `fails_on`-th append reports a failure, plus the
        /// handle on the log it writes to.
        fn new(fails_on: usize) -> (Self, SharedLog) {
            let inner: SharedLog = std::sync::Arc::default();
            let store = Self {
                inner: std::sync::Arc::clone(&inner),
                fails_on,
                attempts: std::sync::atomic::AtomicUsize::new(0),
            };
            (store, inner)
        }

        /// Whether this attempt is the one that fails.
        fn fails_now(&self) -> bool {
            let attempt = self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            attempt == self.fails_on
        }

        /// The log, borrowed.
        async fn log(&self) -> tokio::sync::MutexGuard<'_, knl::MemEventStore> {
            self.inner.lock().await
        }
    }

    #[async_trait::async_trait]
    impl knl::EventStore for FlakyStore {
        async fn append(&mut self, event: Map<String, Value>) -> knl::KnlResult<knl::Committed> {
            if self.fails_now() {
                return Err(knl::KnlError::Storage("the store is down".to_string()));
            }
            self.log().await.append(event).await
        }

        /// A batch is *one* write, as it is on the durable backend: it counts
        /// as one attempt, and when that attempt is the failing one nothing
        /// in the batch is recorded.  A stand-in that let half a batch land
        /// would be modelling a store the SPI does not allow.
        async fn append_many(
            &mut self,
            events: Vec<Map<String, Value>>,
        ) -> knl::KnlResult<Vec<knl::Committed>> {
            if self.fails_now() {
                return Err(knl::KnlError::Storage("the store is down".to_string()));
            }
            let mut log = self.log().await;
            let mut committed = Vec::with_capacity(events.len());
            for event in events {
                committed.push(log.append(event).await?);
            }
            Ok(committed)
        }

        async fn append_if(
            &mut self,
            kinds: Option<&[&str]>,
            decide: knl::Decision,
        ) -> knl::KnlResult<Option<knl::Committed>> {
            if self.fails_now() {
                return Err(knl::KnlError::Storage("the store is down".to_string()));
            }
            self.log().await.append_if(kinds, decide).await
        }

        async fn read_kinds(
            &self,
            kinds: Option<&[&str]>,
            from_seq: u64,
            limit: usize,
        ) -> knl::KnlResult<Vec<Value>> {
            self.log().await.read_kinds(kinds, from_seq, limit).await
        }

        async fn head(&self) -> knl::KnlResult<Option<u64>> {
            self.log().await.head().await
        }

        async fn len(&self) -> knl::KnlResult<usize> {
            self.log().await.len().await
        }
    }

    /// The kinds an in-memory log holds, in order.
    fn kinds_in(log: &SharedLog) -> Vec<String> {
        use crate::knl::EventStore;

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime to read on");
        rt.block_on(async { log.lock().await.read(0, usize::MAX).await })
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
    fn vm_with_a_failing_store(fails_on: usize) -> (Vm, SharedLog) {
        let vm = vm();
        let (store, log) = FlakyStore::new(fails_on);
        // Handed over once, from inside an async function like `knl.open`
        // itself: opening a session is a write, so it suspends.
        let store = std::sync::Arc::new(Mutex::new(Some(store)));
        let open_failing =
            vm.lua
                .create_async_function(move |lua, ()| {
                    let store = std::sync::Arc::clone(&store);
                    async move {
                        let store = store.lock().await.take().ok_or_else(|| {
                            err("open", "the failing store can only be opened once")
                        })?;
                        let state = knl::Session::open_on("t".to_string(), None, Box::new(store))
                            .await
                            .map_err(|e| knl_err("open", &e))?;
                        lua.create_userdata(Session::from_state(state))
                    }
                })
                .expect("create open_failing");
        vm.lua
            .globals()
            .set("open_failing", open_failing)
            .expect("register open_failing");
        (vm, log)
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
        let (vm, log) = vm_with_a_failing_store(2);

        vm.exec(
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
        let (vm, log) = vm_with_a_failing_store(2);

        let msg = vm.expect_err(
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
    #[tokio::test]
    async fn an_open_that_cannot_be_recorded_leaves_the_stream_empty() {
        use crate::knl::EventStore;

        // The whole opening is one write, so it is the first attempt.
        let (store, log) = FlakyStore::new(1);
        let err = knl::Session::open_on(
            "t".to_string(),
            Some(knl::BudgetGrant::new(100)),
            Box::new(store),
        )
        .await
        .expect_err("the open must fail");
        assert_eq!(err.reason(), "the store is down");

        let recorded = log.lock().await.read(0, usize::MAX).await.expect("read");
        assert!(
            recorded.is_empty(),
            "a failed open records nothing at all: {recorded:?}"
        );
    }

    /// The other side of it: an open that *does* land records both events, in
    /// order, from the one write.
    #[tokio::test]
    async fn an_open_records_its_boundary_and_its_grant_together() {
        use crate::knl::{event::kind_of, EventStore};

        let (store, log) = FlakyStore::new(0);
        let session = knl::Session::open_on(
            "t".to_string(),
            Some(knl::BudgetGrant::new(100)),
            Box::new(store),
        )
        .await
        .expect("the open lands");
        let recorded = log.lock().await.read(0, usize::MAX).await.expect("read");
        let kinds: Vec<&str> = recorded.iter().map(kind_of).collect();
        assert_eq!(kinds, ["session_opened", "budget_granted"]);
        assert_eq!(session.remaining().await, Ok(Some(100)));
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
        let vm = vm();
        vm.exec(
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
        .expect("close forms chunk");

        // And a non-string detail is refused, naming which argument it was.
        let msg = vm.expect_err(r#"knl.open({ owner = "t" }):close("error", 7)"#);
        assert!(msg.contains("knl: close:"), "missing attribution: {msg}");
        assert!(msg.contains("detail:"), "{msg}");
        assert!(msg.contains("expected a string"), "{msg}");
    }

    /// A long `detail` is cut to the cap, exactly as the `<close>` path cuts
    /// the message of a raised error: one bad turn must not put a page into
    /// the log, whichever side records it.
    #[test]
    fn a_long_close_detail_is_truncated() {
        let vm = vm();
        vm.exec(
            r#"
            local s = knl.open({ owner = "t" })
            s:close("error", string.rep("x", 500))
            local last = s:events()[s:len()].data
            assert(#last.detail == 203, "detail length: " .. tostring(#last.detail))
            assert(last.detail:sub(-3) == "...", "a cut detail says it was cut")
        "#,
        )
        .expect("long detail chunk");
    }

    /// (disposable) A closed stream is not reopened.  The session ended; what
    /// comes after an ending is a new session, and `knl.resume` says so
    /// instead of handing back a handle onto a finished log.
    #[test]
    fn resume_refuses_a_closed_stream() {
        let vm = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        let msg = vm.expect_err(&format!(
            r#"
                local s = knl.open({{ store = {{ sqlite = "{path_str}" }}, owner = "t" }})
                local id = s:id()
                s:close("done")
                knl.resume({{ store = {{ sqlite = "{path_str}" }}, session = id }})
            "#
        ));
        assert!(msg.contains("knl: resume:"), "missing attribution: {msg}");
        assert!(msg.contains("session is closed"), "{msg}");
        assert!(msg.contains("disposable"), "{msg}");
    }

    /// (F3) A resume that is refused writes nothing.  The reserved-owner
    /// check runs before any append, so a caller cannot leave a
    /// `budget_granted` in a stream it was not allowed to reopen.
    #[test]
    fn a_refused_resume_records_no_grant() {
        let vm = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path");

        // The host side legitimately opens a SYSTEM-owned stream.
        let stream = "system-grant-stream".to_string();
        let drivers = vm.drivers.clone();
        vm.block_on(async {
            let store = crate::knl::SqliteEventStore::open(&path, stream.clone(), &drivers)
                .await
                .expect("open store");
            let state =
                crate::knl::Session::open_on(crate::knl::SYSTEM.to_string(), None, Box::new(store))
                    .await
                    .expect("open system session");
            drop(state);
        });
        let before = persisted(&path, &stream).len();

        let msg = vm.expect_err(&format!(
            r#"knl.resume({{ store = {{ sqlite = "{path_str}" }}, session = "{stream}",
                                 budget = {{ amount = 100, tag = "beats" }} }})"#
        ));
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
        let vm = vm();

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
        let session: LuaAnyUserData = vm
            .eval(r#"return knl.open({ owner = "t" })"#)
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
        let mut bound: Vec<String> = vm
            .eval::<Vec<String>>(
                r#"
                local names = {}
                for name, value in pairs(knl) do
                    if type(value) == "function" then table.insert(names, name) end
                end
                return names
            "#,
            )
            .expect("reflect over the knl global");
        bound.sort();
        assert_eq!(bound, module, "the module surface is MODULE_API");

        // And `knl.api()` hands the same two lists to Lua, each entry with
        // the name and its one-line contract.
        vm.exec(
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
        .expect("api() chunk");

        let counted: usize = vm
            .eval(r#"local a = knl.api() return #a.session + #a.module"#)
            .expect("count the api entries");
        assert_eq!(counted, SESSION_API.len() + MODULE_API.len());
    }

    /// A raised failure carries its class, and `knl.error` hands it back as
    /// a table: what a caller branches on is a word from a closed list, not
    /// a sentence that is free to be reworded.
    #[test]
    fn a_raised_failure_reports_its_class_through_knl_error() {
        let vm = vm();
        vm.exec(
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
        .expect("classified failures chunk");
    }

    /// The class did not cost the message.  A caller that only prints, or
    /// searches the text it caught, reads exactly what it read before — and
    /// the table stands in for the raised value wherever one was.
    #[test]
    fn a_classified_failure_still_reads_as_a_message() {
        let vm = vm();
        vm.exec(
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
        .expect("message compatibility chunk");
    }

    /// `knl.api().errors` is the kernel's class list itself, so the shell can
    /// hold its own declaration of the vocabulary against it instead of
    /// against a list somebody retyped.
    #[test]
    fn api_publishes_the_error_vocabulary() {
        let vm = vm();
        let published: Vec<String> = vm
            .eval(r#"return knl.api().errors"#)
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
        let (vm, _log) = vm_with_a_failing_store(2);
        vm.exec(
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
        let vm = vm();
        vm.exec(
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
        .expect("query chunk");
    }

    /// `$sessions` reads across the set it was given: two streams in one
    /// database, one statement.  This is what a session tree reads with.
    #[test]
    fn query_reads_across_the_session_set() {
        let vm = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path = path.to_str().expect("utf-8 path");

        vm.exec(&format!(
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
        .expect("session set chunk");
    }

    /// A query reads.  Anything that writes, and anything that is two
    /// statements, is refused as the caller's mistake — before the connection
    /// is reached, and on a connection that could not do it anyway.
    #[test]
    fn query_refuses_everything_that_is_not_one_read() {
        let vm = vm();
        vm.exec(
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
            assert(m.message:find("sql:", 1, true), m.message)
            assert(m.message:find("expected a string", 1, true), m.message)
        "#,
        )
        .expect("refusal chunk");
    }

    /// The row cap is reported, so a page can be told from a whole answer.
    #[test]
    fn query_caps_the_rows_and_says_when_it_cut() {
        let vm = vm();
        vm.exec(
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
        .expect("limit chunk");
    }

    /// A query that will not finish is cut short and says so in its own
    /// class — "ask again" would be the wrong advice for a slow read.
    #[test]
    fn query_that_runs_too_long_reports_a_timeout() {
        let vm = vm();
        vm.exec(
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
        .expect("timeout chunk");
    }

    /// `knl.api().schema` is the read contract: the table a query names, and
    /// its columns as SQLite reports them — including which two are the key.
    #[test]
    fn api_publishes_the_events_schema() {
        let vm = vm();
        vm.exec(
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
        .expect("schema chunk");
    }

    // -- the rule this round exists for -------------------------------------

    /// **A slow write does not stop the VM.**
    ///
    /// This is the property the whole round is about, so it is asserted
    /// directly rather than inferred from the shape of the code: a second
    /// coroutine on the same Lua state goes on running — advancing a counter
    /// through an async function of its own — for the *whole* time an
    /// `s:append` is waiting on a write lock another connection is holding.
    ///
    /// Before this round the session's methods were synchronous, so the
    /// append would have parked the VM's one thread and the ticker would have
    /// counted nothing until the lock was released.
    #[test]
    fn a_slow_write_does_not_block_another_coroutine_on_the_same_vm() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        /// How long the blocker holds the write lock.
        const HELD: Duration = Duration::from_millis(300);
        /// How long each tick takes, so ~60 fit inside `HELD`.
        const TICK: Duration = Duration::from_millis(5);
        /// The floor the assertion uses.  Far below what should actually
        /// happen (~60), because the point is "the VM kept running", not a
        /// measurement of how fast it ran.
        const AT_LEAST: usize = 5;

        let vm = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path").to_string();

        // `tick()` waits like any async bridge function does; `ticks()` reads
        // the counter without waiting for anything.
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let tick = vm
            .lua
            .create_async_function(move |_, ()| {
                let counter = Arc::clone(&counter);
                async move {
                    tokio::time::sleep(TICK).await;
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            })
            .expect("create tick");
        vm.lua.globals().set("tick", tick).expect("set tick");
        let counter = Arc::clone(&ticks);
        let read_ticks = vm
            .lua
            .create_function(move |_, ()| Ok(counter.load(Ordering::Relaxed)))
            .expect("create ticks");
        vm.lua
            .globals()
            .set("ticks", read_ticks)
            .expect("set ticks");

        // The session is opened before the lock is taken, so the only thing
        // waiting on it is the append below.
        vm.exec(&format!(
            r#"session = knl.open({{ store = {{ sqlite = "{path_str}" }}, owner = "t" }})"#
        ))
        .expect("open the durable session");

        // A second connection holds the write lock for `HELD`, on a thread of
        // its own so the test can go on driving the VM.
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let blocker_path = path.clone();
        let blocker = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&blocker_path).expect("open the blocker");
            conn.busy_timeout(HELD).expect("busy timeout");
            conn.execute_batch("BEGIN EXCLUSIVE")
                .expect("take the write lock");
            locked_tx.send(()).expect("announce the lock");
            std::thread::sleep(HELD);
            conn.execute_batch("ROLLBACK").expect("release the lock");
        });
        locked_rx.recv().expect("the lock was taken");

        // Two coroutines, driven together on the VM's runtime: one blocked on
        // the write, one counting.  `during` is the number of ticks that
        // landed while the append was waiting.
        let during: usize = vm.block_on(async {
            let writer = vm
                .lua
                .load(
                    r#"
                    local before = ticks()
                    session:append({ kind = "slow" })
                    return ticks() - before
                "#,
                )
                .eval_async::<usize>();
            let ticker = vm.lua.load(r#"for _ = 1, 200 do tick() end"#).exec_async();
            // Both futures poll the same Lua state on this one thread, which
            // is exactly what the VM's own LocalSet does with its coroutines.
            let (written, _ticked) = tokio::join!(writer, ticker);
            written.expect("the append eventually lands")
        });

        blocker.join().expect("the blocker thread");

        assert!(
            during >= AT_LEAST,
            "the VM stopped while the write was waiting: only {during} tick(s) ran"
        );

        // And the write itself landed once the lock was released.
        vm.exec(r#"assert(kinds_of(session) == "session_opened,slow", kinds_of(session))"#)
            .expect("the slow append landed");
    }

    /// **An identity read never waits and never raises.**
    ///
    /// `id` / `scope_id` / `owner` are declared as methods that answer out of
    /// the value, and [`SESSION_API`] lists no class for them.  They used to
    /// reach the session behind a `try_lock`, which has exactly one answer for
    /// "somebody else is mid-call" and it is a raise — so a second coroutine
    /// asking a session its own id while the first was suspended inside
    /// `s:append` got a `validation` failure instead of a string.  The three
    /// values are copied into the userdata at construction now, and this is
    /// the case that says so: same shape as the non-blocking write above, with
    /// the identity read taking the ticker's place.
    #[test]
    fn an_identity_read_answers_while_another_coroutine_holds_the_session() {
        use std::time::Duration;

        /// How long the blocker holds the write lock — long enough that the
        /// append below is certainly still suspended when the reader runs.
        const HELD: Duration = Duration::from_millis(300);

        let vm = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let path_str = path.to_str().expect("utf-8 path").to_string();

        // One yield for the reader, so it asks from inside the same suspension
        // the writer is parked in rather than before the writer got there.
        let pause = vm
            .lua
            .create_async_function(|_, ()| async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(())
            })
            .expect("create pause");
        vm.lua.globals().set("pause", pause).expect("set pause");

        vm.exec(&format!(
            r#"
            session = knl.open({{ store = {{ sqlite = "{path_str}" }}, owner = "u-7" }})
            -- What the reads must still answer while the session is held.
            expected_id, expected_scope, expected_owner =
                session:id(), session:scope_id(), session:owner()
            "#
        ))
        .expect("open the durable session");

        // A second connection holds the write lock, so the append below is
        // parked inside the store with the session's own lock held.
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let blocker_path = path.clone();
        let blocker = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&blocker_path).expect("open the blocker");
            conn.busy_timeout(HELD).expect("busy timeout");
            conn.execute_batch("BEGIN EXCLUSIVE")
                .expect("take the write lock");
            locked_tx.send(()).expect("announce the lock");
            std::thread::sleep(HELD);
            conn.execute_batch("ROLLBACK").expect("release the lock");
        });
        locked_rx.recv().expect("the lock was taken");

        let read: String = vm.block_on(async {
            let writer = vm
                .lua
                .load(r#"session:append({ kind = "slow" })"#)
                .exec_async();
            // The reader yields once first, so the writer is inside the store
            // — and therefore holding the session — before it asks.
            let reader = vm
                .lua
                .load(
                    r#"
                    pause()
                    local id, scope, owner = session:id(), session:scope_id(), session:owner()
                    assert(id == expected_id, "id: " .. tostring(id))
                    assert(scope == expected_scope, "scope_id: " .. tostring(scope))
                    assert(owner == expected_owner, "owner: " .. tostring(owner))
                    assert(owner == "u-7", "owner: " .. tostring(owner))
                    return id
                "#,
                )
                .eval_async::<String>();
            let (written, read) = tokio::join!(writer, reader);
            written.expect("the append eventually lands");
            read.expect("an identity read must answer while the session is held")
        });

        blocker.join().expect("the blocker thread");
        assert!(!read.is_empty(), "the read answered the stream's own id");
    }

    /// `knl.open{ parent = s, budget = { from_parent = n } }` opens a session
    /// on the parent's database and out of its balance, in one write: the
    /// child's log names the parent and carries the grant, and the parent's
    /// ledger carries the reservation naming the child.
    #[test]
    fn open_with_a_parent_allocates_out_of_the_parents_balance() {
        let vm = vm();
        vm.exec(
            r#"
            local parent = knl.open({ owner = "u", budget = { amount = 100, tag = "tokens" } })
            local child  = knl.open({
                owner  = "worker",
                parent = parent,
                budget = { from_parent = 40 },
            })

            assert(child:id() ~= parent:id(), "a child is its own stream")
            assert(child:owner() == "worker", child:owner())
            assert(parent:remaining() == 60, "parent: " .. tostring(parent:remaining()))
            assert(child:remaining() == 40, "child: " .. tostring(child:remaining()))

            -- the child's own log: opened, and opened with the grant
            assert(kinds_of(child) == "session_opened,budget_granted", kinds_of(child))
            local opened = child:events()[1]
            assert(opened.data.parent == parent:id(), tostring(opened.data.parent))
            local granted = child:events()[2]
            assert(granted.data.parent == parent:id(), tostring(granted.data.parent))
            assert(granted.data.amount == 40, tostring(granted.data.amount))
            assert(granted.data.tag == "tokens", "the parent's unit by default")

            -- the parent's side: a reservation naming where the units went
            local ledger = parent:events()
            local reserved = ledger[#ledger]
            assert(reserved.kind == "budget_reserved", reserved.kind)
            assert(reserved.data.child == child:id(), tostring(reserved.data.child))

            -- and closing the child gives nothing back
            child:close("done")
            assert(parent:remaining() == 60, "an allocation is a spend")
            parent:close("done")
        "#,
        )
        .expect("the allocation");
        vm.finish();
    }

    /// A balance that will not cover the allocation raises `refused` — the
    /// class that reports a decision — with the refusal in the parent's log
    /// and no session handed back.
    #[test]
    fn a_child_the_parent_cannot_pay_for_is_refused() {
        let vm = vm();
        vm.exec(
            r#"
            local parent = knl.open({ owner = "u", budget = { amount = 10, tag = "tokens" } })
            local read, raised = failure(knl.open, {
                owner = "worker", parent = parent, budget = { from_parent = 40 },
            })
            assert(read.kind == "refused", "kind: " .. tostring(read.kind))
            assert(read.method == "open", "method: " .. tostring(read.method))
            assert(read.retryable == false, "the same balance answers the same")
            assert(tostring(raised):find("40", 1, true), tostring(raised))

            -- recorded on the parent, and the balance did not move
            assert(parent:remaining() == 10, tostring(parent:remaining()))
            local ledger = parent:events()
            local refused = ledger[#ledger]
            assert(refused.kind == "budget_refused", refused.kind)
            assert(refused.data.remaining == 10, tostring(refused.data.remaining))
            assert(type(refused.data.child) == "string", "the refusal names the child")
            parent:close("done")
        "#,
        )
        .expect("the refusal");
        vm.finish();
    }

    /// The two forms of `budget` are exclusive in both directions, and a
    /// child on another store is refused as the validation it is.  A quota
    /// nobody paid for and a tree spread over two logs are the two shapes
    /// this rules out.
    #[test]
    fn a_parent_and_a_grant_are_not_mixed() {
        let vm = vm();
        vm.exec(
            r#"
            local parent = knl.open({ owner = "u", budget = { amount = 100, tag = "tokens" } })

            -- from_parent with nobody to take it from
            local orphan = failure(knl.open, { owner = "w", budget = { from_parent = 5 } })
            assert(orphan.kind == "validation", orphan.kind)
            assert(orphan.message:find("opts.parent", 1, true), orphan.message)

            -- a parent, and an owner's grant instead of an allocation
            local granted = failure(knl.open, {
                owner = "w", parent = parent, budget = { amount = 5 },
            })
            assert(granted.kind == "validation", granted.kind)
            assert(granted.message:find("from_parent", 1, true), granted.message)

            -- both at once says neither
            local both = failure(knl.open, {
                owner = "w", parent = parent, budget = { amount = 5, from_parent = 5 },
            })
            assert(both.kind == "validation", both.kind)

            -- a parent that is not a session
            local nonsense = failure(knl.open, {
                owner = "w", parent = "s-1", budget = { from_parent = 5 },
            })
            assert(nonsense.kind == "validation", nonsense.kind)
            assert(nonsense.message:find("must be a session", 1, true), nonsense.message)

            -- a child on a store of its own is a second log, and a tree is one
            local split = failure(knl.open, {
                owner = "w", parent = parent, budget = { from_parent = 5 }, store = "mem",
            })
            assert(split.kind == "validation", split.kind)
            assert(split.message:find("one log", 1, true), split.message)

            -- none of it moved the balance
            assert(parent:remaining() == 100, tostring(parent:remaining()))
            parent:close("done")
        "#,
        )
        .expect("the refusals");
        vm.finish();
    }

    /// Closing a parent whose children are still open is not refused: the
    /// boundary records them and lands.
    #[test]
    fn a_close_records_the_children_that_were_still_open() {
        let vm = vm();
        vm.exec(
            r#"
            local parent = knl.open({ owner = "u", budget = { amount = 100, tag = "tokens" } })
            local running = knl.open({ owner = "w", parent = parent, budget = { from_parent = 10 } })
            local done    = knl.open({ owner = "w", parent = parent, budget = { from_parent = 10 } })
            done:close("done")

            parent:close("done")
            local events = parent:events()
            local boundary = events[#events]
            assert(boundary.kind == "session_closed", boundary.kind)
            local open_children = boundary.data.open_children
            assert(type(open_children) == "table", type(open_children))
            assert(#open_children == 1, "one child was still open, got " .. #open_children)
            assert(open_children[1] == running:id(), tostring(open_children[1]))
            running:close("done")
        "#,
        )
        .expect("the close");
        vm.finish();
    }

    /// A durable tree: the child goes into the parent's file without being
    /// told where that is, and one recursive statement reads the shape back
    /// out of the log.
    #[test]
    fn a_childs_stream_lands_in_the_parents_database() {
        let vm = vm();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tree.db");
        let path_str = path.to_str().expect("utf-8 path").to_string();

        let rows: usize = vm
            .eval(&format!(
                r#"
                local parent = knl.open({{
                    owner = "u",
                    budget = {{ amount = 100, tag = "tokens" }},
                    store = {{ sqlite = "{path_str}" }},
                }})
                local child = knl.open({{
                    owner = "w", parent = parent, budget = {{ from_parent = 25 }},
                }})
                -- The child was never told where the log is, and it is in it:
                -- one statement over the parent's own store reaches both.
                local found = parent:query(
                    "SELECT stream FROM events WHERE kind = 'session_opened' ORDER BY stream"
                )
                child:close("done")
                parent:close("done")
                return #found
            "#
            ))
            .expect("the durable tree");
        assert_eq!(rows, 2, "the parent and its child are in one database");
        vm.finish();
    }
}
