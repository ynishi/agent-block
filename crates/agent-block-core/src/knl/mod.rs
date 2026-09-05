//! `knl` — the kernel core: the log, the ledger, the session and its scope.
//!
//! This module doc is the kernel's design, stated once.  Each section below
//! is an invariant the code is held to, named so that the code depending on
//! it can cite it.
//!
//! # The kernel and the shell
//!
//! The kernel is written in two halves, and only the first of them is here.
//!
//! The **Rust half** — this module — is the kernel context: the session's
//! state, the syscalls that move it, and two fixed reads.  It is pure Rust:
//! nothing here knows about Lua.  Events are plain `serde_json` objects, so
//! the Lua ⇄ JSON conversion — and the re-entrancy discipline that comes
//! with walking a Lua table — stays in the [`crate::bridge::knl`] adapter,
//! one place, while the domain rules below stay unit-testable without a VM.
//!
//! The **Lua half** is the shell's kernel library (`knl`): the beat — one
//! model call plus the tools that call asks for — the device a beat calls
//! through, and the query views.  What a conversation looks like on the
//! wire, what a beat is allowed to cost, which tools may run, when to stop
//! asking: all of that is the shell's, and none of it is here.
//!
//! The line between the halves is what each of them refuses to renegotiate.
//! The kernel fixes the record, the quota and the boundaries of a session.
//! Everything a caller could reasonably want different sits above it.
//!
//! # Session and scope
//!
//! A session has a scope.  The two are different concepts sharing one
//! lifetime: the session is the stream (its history and the projections over
//! it), the [`Scope`] is the authority it is written under — a kernel-issued
//! [`ScopeId`], the owner, the granted quota.  A session holds its scope by
//! value, since neither outlives the other.
//!
//! The scope id is recorded on `session_opened` and on every `budget_*`
//! event, so the boundary is recoverable from the log — and unforgeable,
//! since those kinds are the kernel's alone to write ([`is_kernel_only`]).
//!
//! There is no per-event author: a session holds only its own events, so
//! ownership is the scope-level [`Session::owner`] — a real principal id, or
//! the reserved [`session::ANON`] / [`session::SYSTEM`] — total, and read by
//! the policy layer above the kernel.  An accounting of what was consumed
//! keys on the `kind`: in a session's own log an `llm_response` is a call it
//! made, so a reader that sums the counts needs no author to key on.
//!
//! All state lives inside a [`Session`] value — no statics — so two sessions
//! are fully independent.
//!
//! # A beat is declared, not numbered
//!
//! A `beat` is an opaque string the layer above mints and stamps on the
//! facts that belong together.  The kernel never generates one and never
//! requires one; it only insists that a present `beat` is a string.
//! Grouping and ordering read it back, and nothing else does.
//!
//! Numbering it here would put a cursor back into kernel state, and that
//! number would then have to survive a resume, two handles, and a store that
//! serializes writes in arrival order.  A declared id survives all three
//! while the kernel holds nothing.
//!
//! # The budget is a quota
//!
//! The budget is what an owner allows a session to consume — not a record of
//! what it used.  It buys two things: a stopping guarantee (termination is
//! undecidable, so a monotonically decreasing resource is injected from
//! outside) and an authority boundary (whatever the model decides, the owner
//! has bounded the run).
//!
//! **Two deductions, and neither holds anything for the other.**
//! [`Session::reserve`] is a deduction that *asks*: it refuses, without
//! deducting, when the balance will not cover `n`.  [`Session::spend`] is a
//! deduction that does not ask: it takes `n` off (flooring at zero) and
//! reports only that it was recorded.  There is no hold and no settlement
//! between them — nothing is reserved *for* a later spend to release or
//! reconcile — so **a beat that calls both deducts twice**, and which of the
//! two a beat uses is the layer above's to decide.  A run that wants to be
//! stopped before it spends asks with `reserve`; a run that only meters what
//! already happened deducts with `spend`.  No `append` moves the balance.
//!
//! **Every move is an event, and the balance is a fold over them.**  A
//! grant, a reservation, a refusal and an unasked deduction are each a
//! `budget_*` event ([`BUDGET_KINDS`]), written by the kernel alone, and
//! [`fold_balance`] over those events *is* the balance — there is no counter
//! beside them.  [`Session::remaining`] reads it back off the stream (cached
//! against the store's head, refolded when the head moves), so two handles
//! on one stream cannot hold two different answers.  A refusal is recorded
//! like the rest: that a request was turned down is a fact about the run.
//!
//! **Monotonicity.**  The ledger accepts non-negative amounts only, and
//! within a session the balance can only decrease.  It rises only when an
//! owner grants again ([`BudgetGrant`]), which a resumed session records like
//! any other fact.  There is no API to raise or reset it, and no release: a
//! reservation that was made is not handed back.
//!
//! **Usage is not accounting.**  What the providers reported is a separate
//! reading, taken off the recorded responses, and the kernel never folds it
//! into the balance.  A budget denominated in tokens will — if the layer
//! above deducts honestly, and deducts once — end with `granted - remaining`
//! equal to the usage total, because both are folds over the same log.  That
//! is a consequence, not a requirement, and nothing here checks it.
//!
//! **Allocation, not limit.**  A budget is an *allocation* axis: units are
//! consumed and do not come back, and a child scope can only be given what
//! its parent already holds.  A rate limit is a *limit* axis — replenished
//! by the passage of time — with different arithmetic, and it does not
//! belong in the same counter.  If the ledger ever grows named axes, each
//! axis declares which of the two it is.
//!
//! # Facts live in the kernel, structure is run by the supervisor
//!
//! A session can be opened *from* another one ([`Session::open_child`]), and
//! the kernel records exactly two facts about that and no more: the child's
//! stream names its parent on its `session_opened`, and the allocation that
//! paid for it is one transaction on the parent's ledger — a
//! `budget_reserved` naming the child, against a `budget_granted` on the
//! child naming the parent, or a `budget_refused` and no child at all.  The
//! child is opened on the *same database* as its parent, because a tree that
//! spanned two logs could not be read back by one statement, and both halves
//! of an allocation have to land or neither may.
//!
//! Nothing is released when a child closes.  An allocation is a spend from
//! the parent's point of view (§ *The budget is a quota*, "Allocation, not
//! limit"): the units left with the child, and a refund would be the balance
//! rising without an owner granting.
//!
//! **A close is never refused for a child that is still running.**  It
//! records them — `session_closed.data.open_children` — and lands, in the
//! same transaction as the scan that found them, because the log never turns
//! a write away and "this ended while its children had not" is precisely the
//! fact an audit is reading for.
//!
//! What the kernel does *not* know is what a tree is.  It does not walk one,
//! does not stop a close, does not cascade an ending, does not decide who may
//! allocate to whom, and holds no parent pointer in memory — the facts are in
//! the log and a reader assembles them ([`Session::query`]; the Lua kernel's
//! `knl.views.tree` is one recursive `SELECT` over exactly these fields).  A
//! supervisor pack above the kernel is where a policy over a tree belongs,
//! and it needs the kernel only for the part it cannot do for itself: making
//! the two sides of an allocation one write.
//!
//! # Views: the log is the only source of truth
//!
//! A *view* ([`projection`]) is derived from the log.  Folding never changes
//! the history, and a view's result is a cache rather than a capture —
//! whatever it says is recomputable from the events, so reading one is never
//! what makes it true, and a view that disagreed with the log would be the
//! view that is wrong.
//!
//! The views are deliberately spread across the two halves.  **The Rust half
//! has two built-in reads and they never grow**: [`Session::events`]
//! (`events(from)`, the record from a position on) and [`Session::view`]
//! (`tail`, the last events verbatim).  **Everything else is a Lua query
//! view** over [`Session::query`] — the conversation a provider is sent, the
//! beats of a run, the tool pairs, the ledger, the token account — each of
//! them one `SELECT` over the published event schema rather than a name the
//! kernel had to be taught.
//!
//! So the Rust half names a fold only when its consumer is fixed in kernel
//! terms, and `tail` is the one that is.  Token usage is not: the counts are
//! what an adapter normalized out of a provider's answer, which is the
//! shell's vocabulary, so it is a query view written in Lua.
//!
//! **Why the Rust side does not take query or fold features.**  There is no
//! query language here and no way to register a fold into one, and that is
//! the design rather than a gap.  The whole expressiveness of SQLite is
//! already reachable through [`Session::query`] ([`query`]) — one statement,
//! read-only, over a table whose columns are published ([`events_schema`]) —
//! so "the kernel needs dynamic queries" is answered by writing a Lua query
//! view.  A second, weaker query surface in Rust would only give the same
//! answers a name the kernel then has to keep.  A fold-registration hook
//! would be worse: it moves a caller's code inside the kernel, where neither
//! its cost nor its purity is the caller's problem any more.  That the table
//! *is* the read interface is what makes this a contract rather than a leak
//! — changing it is a change to the interface.
//!
//! **One backend, and the log is a table.**  A session's events live in
//! SQLite whether the session is durable (a file) or ephemeral (an in-memory
//! database) — [`SqliteEventStore`], the only [`EventStore`] the product has.
//! That is not an implementation detail: the read side above is SQL, and a
//! log that could not be queried would be a second, lesser kind of session.
//! The `Vec`-backed store is `#[cfg(test)]`.
//!
//! **A real session is a file.**  A session a script opens is opened on the
//! database the host owns — one file per project — and the in-memory database
//! is for tests and mocks: one session, one process, nothing shared.  It is
//! not a smaller version of the other one.  It is addressed by a shared-cache
//! URI ([`is_memory_database`]) and shared cache locks per *table*, so a second
//! writer meets `SQLITE_LOCKED` at once and no busy timeout waits that out —
//! which is exactly what a session tree is (children write to their parent's
//! database).  So the kernel does not offer a tree on one, and there is no
//! lock-waiting machinery here to make it work.
//!
//! # Stored shape: envelope, meta, data
//!
//! An event is an envelope ([`FIELD_KIND`], an optional `beat`, the kernel's
//! `seq` / `epoch_ms` / `_schema_version`), a shallow `meta`, and a `data`
//! object holding the kind's own content.  Nothing else may sit at the top
//! level.  The three levels are separated so that a reader can tell which of
//! them it is reading:
//!
//! ```text
//! envelope   kind, beat, seq, epoch_ms, _schema_version   ← columns; never renamed
//! meta       { label = "a", attempt = 2 }                 ← shallow by rule: scalars only
//! data       { content = { … }, usage = { … } }           ← the kind's own, any depth
//! ```
//!
//! - The **envelope** is the stable contract.  Its keys are the columns of
//!   the `events` table ([`events_schema`]), and they do not get renamed: a
//!   view built on them is unaffected by any kind changing shape.
//! - **`meta`** holds scalars — a string, a number or a boolean — and
//!   nesting is refused.  That is what makes it readable without knowing the
//!   kind: a label to group by, a flag to filter on.
//! - **`data`** is the only place structured JSON lives, and its shape
//!   belongs to whoever writes the kind.  The kernel checks the `data` of the
//!   six kinds it writes itself ([`is_kernel_only`]) and of no others; the
//!   kinds a beat is made of are the Lua kernel's, declared where they are
//!   written.
//!
//! The rule that follows, and the reason for the split: **a SQL view that
//! reads a `data` path is updated in the same round as the kind whose shape
//! it reads.**  When everything sat at one level, a change to what one kind
//! recorded broke a `json_extract` path with nothing to say which change had
//! done it.  Structured JSON is unavoidable in an event log; confining it to
//! one key is what makes its evolution reviewable.
//!
//! `_schema_version` is the whole *object's*, not `data`'s: the upcaster seam
//! ([`Upcaster`], [`Current`]) applies to the event as it was stored, and
//! `data` is simply where the changes it will have to absorb happen.
//!
//! # An append lands; a command decides in the store
//!
//! **The record is append-only.**  [`History`] has no mutation API — no
//! `update`, `delete` or `replace`.  `seq` is assigned by the kernel, starts
//! at `1` and increases strictly; a caller-supplied `seq` / `epoch_ms` is
//! overwritten rather than trusted.  Reads hand back clones, so a caller
//! cannot reach recorded state through a returned value.
//!
//! **An append lands.**  Recording a fact is never refused for what the
//! writing handle last saw: the store assigns the `seq` and serializes
//! writes per stream, so two handles on one stream both append and the log
//! interleaves in arrival order.  The one place a check belongs — "reserve
//! `n` only if the balance covers it" — runs inside that same serialized
//! write ([`EventStore::append_if`]), never against a cached balance.
//!
//! **The lifecycle is the session's own.**  There is no "run" inside a
//! session: it is bracketed by the `session_opened` that
//! [`Session::open_on`] records and the `session_closed` that
//! [`Session::close`] records.  Both are kernel-only ([`is_kernel_only`]), so
//! a caller can neither fake an opening nor end a session by appending an
//! event.
//!
//! **Closed is the handle's, not the stream's.**  A handle that closed
//! refuses its own later `append` / `spend`, while the log itself never
//! refuses a write: one arriving from another handle after an ending lands,
//! as evidence, and two handles that both close leave two endings rather
//! than one.  Exactly one reader consults `session_closed`, and it is
//! [`Session::resume`].
//!
//! **A session is disposable.**  It opens once and closes once, and
//! [`Session::resume`] refuses a stream whose `session_closed` is already in
//! the log: after an ending there is a new session, not a second life for
//! the old one.
//!
//! # Errors
//!
//! A failure is classified rather than described.  [`KnlError`] is a closed
//! set of eight classes ([`KnlError::KINDS`]) — `busy`, `storage`,
//! `corruption`, `closed`, `validation`, `unsupported`, `timeout`,
//! `refused` — and the
//! variant *is* the classification: the payload is a human-readable reason
//! and nothing a caller should branch on.  [`KnlError::is_retryable`] answers
//! the one question that belongs to a program rather than to a person, and
//! it is true for `busy` and nothing else.
//!
//! The core does not know which method a caller invoked, so
//! [`Display`](std::fmt::Display) writes `<kind>: <reason>` and the adapter
//! adds the attribution: a failure reaches Lua as the raised text
//! `knl: <method>: <kind>: <reason>`, which `knl.error(e)` reads back as
//! `{ kind, method, retryable, message }`.  The message is given a shape
//! because mlua cannot carry a table out of a Rust callback — the first
//! three fields are a closed vocabulary and only the last is prose.  See
//! [`crate::bridge::knl`].
//!
//! # Upcasting: a stored shape change ships with its upcaster
//!
//! Stored bytes are never rewritten.  A change to the shape of a stored
//! event ships with a bump of [`CURRENT_SCHEMA_VERSION`] and the matching
//! read-time [`Upcaster`] ([`kernel_upcasters`]), which every session's reads
//! pass through.
//!
//! Nothing has been released yet, so v1 is fixed until the first release:
//! the chain is empty and the version is `1`.  The seam is in place and
//! tested, and the first step that is owed has one site to be registered at.
//! The seam is a type: a backend deals in raw `Value`s, [`CurrentStore`]
//! reads through the chain and hands back [`Current`]s, and every fold takes
//! those — so a read that went round the chain does not compile.
//!
//! # Async: everything that waits, yields
//!
//! **The VM thread never waits on the OS.**  That is the whole rule, and the
//! syscalls follow it: `append`, `reserve`, `spend`, the reads and `close`
//! are `async fn`, and so are `Session::new` / `open_on` / `resume` and the
//! [`EventStore`] SPI underneath them.
//!
//! The thread this matters for is the Lua VM's.  It is the *only* worker of
//! the runtime that also drives every other coroutine that VM owns, every
//! timer they set and every cancellation watching them, so a syscall that
//! parked it would stop all of them — for as long as a contended SQLite write
//! takes, which is bounded by a busy timeout and a retry policy rather than by
//! anything a caller chose.  These calls used to be synchronous, and the
//! reasoning for that ("a local SQLite write is quick") mistook *where* the
//! blocking landed: the connection lives on its own thread, but the caller
//! was waiting on it from the one thread that must not wait.
//!
//! So waiting is yielding, everywhere.  [`SqliteEventStore`] sends each call
//! to the thread that owns its connection ([`rusqlite_isle::AsyncIsle`]) and
//! suspends on the answer; the bridge binds the session's methods with
//! `add_async_method`, so `s:append(...)` is a coroutine yield on the Lua
//! side and the beat's `pcall` / `<close>` / step structure is unchanged
//! (Lua 5.4 yields across all three).  Device I/O — an HTTP request, an MCP
//! call — was already async and is unaffected; what changed is that the
//! syscalls now behave the same way it does.
//!
//! Two things stay synchronous, and both are deliberate: the identity reads
//! (`id` / `scope_id` / `owner`), which answer out of the value and touch no
//! store, and [`Session::close_detached`], the drop backstop — `Drop` cannot
//! await and must not block, so it hands its `session_closed` to the store's
//! writer and lets go ([`EventStore::detach_append`]).  That works because the
//! connection threads outlive the sessions: their drivers belong to an
//! [`IsleDrivers`] the host holds and drains once, at shutdown.

pub mod budget;
pub mod event;
pub mod event_store;
pub mod history;
pub mod projection;
pub mod query;
pub mod scope;
pub mod session;
pub mod sqlite_store;

pub use budget::{fold_balance, Allocation, BudgetGrant};
pub use event::{
    is_kernel_only, now_ms, validate_event, BUDGET_KINDS, FIELD_AMOUNT, FIELD_CHILD, FIELD_DESC,
    FIELD_DETAIL, FIELD_EPOCH_MS, FIELD_KIND, FIELD_OPEN_CHILDREN, FIELD_OWNER, FIELD_PARENT,
    FIELD_REASON, FIELD_REMAINING, FIELD_SCOPE_ID, FIELD_SEQ, FIELD_TAG,
};
#[cfg(test)]
pub use event_store::MemEventStore;
pub use event_store::{
    apply_upcasters, kernel_upcasters, ChildScan, ChildrenDecision, Committed, Current,
    CurrentDecision, CurrentSplitDecision, CurrentStore, Decision, EventStore, Split,
    SplitDecision, Upcaster, CURRENT_SCHEMA_VERSION, SCHEMA_VERSION_FIELD,
};
pub use history::History;
pub use query::{QueryOpts, QueryParams, QueryPlan, QueryRows, DEFAULT_LIMIT, DEFAULT_TIMEOUT_MS};
pub use scope::{Scope, ScopeId};
pub use session::{
    Session, ANON, CLOSE_REASON_DROPPED, CLOSE_REASON_ERROR, CLOSE_REASON_SCOPE_EXIT,
    DEFAULT_CLOSE_REASON, SYSTEM,
};
pub use sqlite_store::{
    events_schema, is_memory_database, IsleDrivers, SchemaColumn, SqliteEventStore, EVENTS_TABLE,
};

/// What went wrong in the kernel core, classified.
///
/// A failure is not one thing.  A contended database will succeed if it is
/// asked again; a row that will not decode never will.  A caller that passed
/// a negative amount has a bug in its own code; a caller that wrote to a
/// closed handle has finished with the session and needs a new one.  Folding
/// all four into one opaque string leaves every caller — the Lua shell most
/// of all — matching on message text to tell them apart, and message text is
/// the one part of an error that is meant to change.
///
/// So the variant *is* the classification, and it is the whole of it: the
/// payload is a human-readable sentence and nothing a caller should branch
/// on.  [`KnlError::kind`] names the class in one stable word, and
/// [`KnlError::is_retryable`] answers the only question whose answer is a
/// program's rather than a person's.
///
/// The core does not know which Lua method the caller invoked, so the
/// message carries the reason only; the adapter renders the
/// `knl: <method>: <kind>: <reason>` attribution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KnlError {
    /// Lock contention: the store was busy and the same call may succeed if
    /// it is made again.  The one retryable class ([`KnlError::is_retryable`]).
    #[error("busy: {0}")]
    Busy(String),
    /// The store could not do the work — an IO fault, a connection that is
    /// gone, an encode failure on the way in.  Not busy, so retrying it is a
    /// caller's gamble rather than the kernel's advice.
    #[error("storage: {0}")]
    Storage(String),
    /// A stored row could not be read back as the event it was written as.
    /// Distinct from [`KnlError::Storage`] on purpose: the IO succeeded and
    /// the bytes came back, so what is wrong is the data, and no retry and
    /// no reconnect will change it.
    #[error("corruption: {0}")]
    Corruption(String),
    /// The session is over — this handle closed, or a resume was pointed at
    /// a stream whose log already carries its ending.  A session is
    /// disposable, so the answer is to open another, not to try again.
    #[error("closed: {0}")]
    Closed(String),
    /// The caller asked for something the kernel refuses to record: an event
    /// that does not meet its kind's shape, a kernel-only kind, a negative
    /// amount, an unknown view or a malformed option.  Nothing was written.
    #[error("validation: {0}")]
    Validation(String),
    /// The request is well-formed but this backend cannot serve it — a query
    /// put to a store that keeps no queryable table.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A read ran past the time it was given and was cut short.  Distinct
    /// from [`KnlError::Busy`] on purpose: nothing was contended, the work
    /// itself was too slow, so making the same call again buys nothing —
    /// what changes the answer is a narrower query or a longer deadline.
    #[error("timeout: {0}")]
    Timeout(String),
    /// A quota did not cover what was asked for, and the refusal was
    /// recorded.
    ///
    /// The one class that reports a *decision* rather than a fault.  Nothing
    /// is wrong: the request was well-formed, the store answered, and the
    /// answer is no — [`Session::open_child`] raises it when the parent's
    /// balance will not cover the allocation, having written the
    /// `budget_refused` that says so.  Distinct from
    /// [`KnlError::Validation`] because the caller's arguments were fine, and
    /// not retryable, because the same call against the same balance gets the
    /// same answer; what changes it is the owner granting more.
    ///
    /// [`Session::reserve`] does *not* raise this — it answers `false`,
    /// because a reservation is asked for in a loop that is expected to be
    /// told no.  An allocation is not: it either produced a child or it did
    /// not, and there is no half-opened session to hand back.
    #[error("refused: {0}")]
    Refused(String),
}

impl KnlError {
    /// The stable name of the [`KnlError::Busy`] class.
    pub const BUSY: &'static str = "busy";
    /// The stable name of the [`KnlError::Storage`] class.
    pub const STORAGE: &'static str = "storage";
    /// The stable name of the [`KnlError::Corruption`] class.
    pub const CORRUPTION: &'static str = "corruption";
    /// The stable name of the [`KnlError::Closed`] class.
    pub const CLOSED: &'static str = "closed";
    /// The stable name of the [`KnlError::Validation`] class.
    pub const VALIDATION: &'static str = "validation";
    /// The stable name of the [`KnlError::Unsupported`] class.
    pub const UNSUPPORTED: &'static str = "unsupported";
    /// The stable name of the [`KnlError::Timeout`] class.
    pub const TIMEOUT: &'static str = "timeout";
    /// The stable name of the [`KnlError::Refused`] class.
    pub const REFUSED: &'static str = "refused";

    /// Every class a kernel failure can have, in one closed list.
    ///
    /// Published so a caller can hold its own error vocabulary against the
    /// kernel's — the Lua bridge hands this to `knl.api()`, and the shell's
    /// declaration is checked against it rather than against a list somebody
    /// retyped.
    pub const KINDS: &'static [&'static str] = &[
        Self::BUSY,
        Self::STORAGE,
        Self::CORRUPTION,
        Self::CLOSED,
        Self::VALIDATION,
        Self::UNSUPPORTED,
        Self::TIMEOUT,
        Self::REFUSED,
    ];

    /// This failure's class, as one of [`KnlError::KINDS`].
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Busy(_) => Self::BUSY,
            Self::Storage(_) => Self::STORAGE,
            Self::Corruption(_) => Self::CORRUPTION,
            Self::Closed(_) => Self::CLOSED,
            Self::Validation(_) => Self::VALIDATION,
            Self::Unsupported(_) => Self::UNSUPPORTED,
            Self::Timeout(_) => Self::TIMEOUT,
            Self::Refused(_) => Self::REFUSED,
        }
    }

    /// Whether making the same call again could succeed.
    ///
    /// True for [`KnlError::Busy`] and nothing else.  A storage fault *might*
    /// clear, but the kernel does not know that it will, and an error that
    /// says "try again" when it means "maybe" is how a retry loop becomes an
    /// infinite one.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Busy(_))
    }

    /// Whether a class *name* is the retryable one.
    ///
    /// The same answer as [`KnlError::is_retryable`], for a caller that has
    /// the word rather than the value — the Lua bridge, which parses a kind
    /// back out of an attributed message.
    pub fn kind_is_retryable(kind: &str) -> bool {
        kind == Self::BUSY
    }

    /// The reason, without the class name or any attribution prefix.
    ///
    /// [`Display`](std::fmt::Display) writes `<kind>: <reason>`; this is the
    /// second half alone, for a caller that renders the class itself.
    pub fn reason(&self) -> &str {
        match self {
            Self::Busy(reason)
            | Self::Storage(reason)
            | Self::Corruption(reason)
            | Self::Closed(reason)
            | Self::Validation(reason)
            | Self::Unsupported(reason)
            | Self::Timeout(reason)
            | Self::Refused(reason) => reason,
        }
    }
}

/// Result alias for the kernel core.
pub type KnlResult<T> = Result<T, KnlError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// One error of every class, so a test over the classification cannot
    /// quietly skip a variant that was added later.
    fn one_of_each() -> Vec<KnlError> {
        vec![
            KnlError::Busy("contended".to_string()),
            KnlError::Storage("gone".to_string()),
            KnlError::Corruption("not json".to_string()),
            KnlError::Closed("session is closed".to_string()),
            KnlError::Validation("kind is required".to_string()),
            KnlError::Unsupported("this store keeps no queryable table".to_string()),
            KnlError::Timeout("query interrupted".to_string()),
            KnlError::Refused("the balance does not cover it".to_string()),
        ]
    }

    /// Every variant names its class with a stable word, and the published
    /// list is exactly those words in that order.
    #[test]
    fn every_variant_names_its_class() {
        let kinds: Vec<&str> = one_of_each().iter().map(KnlError::kind).collect();
        assert_eq!(
            kinds,
            vec![
                "busy",
                "storage",
                "corruption",
                "closed",
                "validation",
                "unsupported",
                "timeout",
                "refused"
            ]
        );
        assert_eq!(
            kinds,
            KnlError::KINDS.to_vec(),
            "KINDS is the vocabulary itself, not a second copy of it"
        );
    }

    /// Only contention says "ask again".  A storage fault might clear on its
    /// own, but the kernel does not know that, and an error that promises a
    /// retry it cannot back is how a loop stops terminating.
    #[test]
    fn only_busy_is_retryable() {
        for error in one_of_each() {
            let expected = error.kind() == KnlError::BUSY;
            assert_eq!(error.is_retryable(), expected, "{error}");
            assert_eq!(
                KnlError::kind_is_retryable(error.kind()),
                expected,
                "the name and the value must agree: {error}"
            );
        }
        assert!(!KnlError::kind_is_retryable("nonsense"));
    }

    /// `Display` is `<kind>: <reason>`, and `reason` is the second half on
    /// its own — the adapter renders the class itself, so it must be able to
    /// get the sentence without it.
    #[test]
    fn display_carries_the_class_and_reason_carries_only_the_sentence() {
        let error = KnlError::Validation("kind is required (string)".to_string());
        assert_eq!(error.to_string(), "validation: kind is required (string)");
        assert_eq!(error.reason(), "kind is required (string)");

        for error in one_of_each() {
            assert_eq!(
                error.to_string(),
                format!("{}: {}", error.kind(), error.reason())
            );
        }
    }
}
