//! K5 — the session.
//!
//! A session binds one history, one budget and the projection caches
//! together and is the only handle on kernel state.  All of it lives in
//! the value: two sessions share nothing.
//!
//! # The lifecycle is the session's
//!
//! There is no "run" inside a session.  A session opens once, records, and
//! closes once; the two events that bracket it — `session_opened` and
//! `session_closed` — are written by the kernel on those two occasions and
//! by nothing else.  A caller cannot hand-append either
//! ([`super::event::is_kernel_only`]), because a stream that claims an
//! opening it never had, or an ending it never reached, is exactly what a
//! resume and an audit read.
//!
//! Beats are the layer above's, not the kernel's: the shell mints a beat id
//! and stamps it on the facts that belong together.  The kernel neither
//! numbers nor requires one — see [`super::event`].
//!
//! # The session *has a* scope
//!
//! A scope and a session are two things, sharing one lifetime: both begin
//! when the session opens and end when it closes.  The session is the
//! stream — this history and its projections.  The [`Scope`] is the
//! authority it is written under: a kernel-issued [`ScopeId`], the `owner`,
//! and the quota that owner granted.  The session holds it *by value*,
//! because neither outlives the other and there is nothing to share.
//!
//! A session holds only its own events, so ownership is not a per-event
//! question: the scope carries one `owner` — a real principal id, or the
//! reserved [`ANON`] / [`SYSTEM`] id — and it is *total* (never `Option`,
//! never a "kernel vs caller" flag).  System-originated work is a session
//! whose owner is [`SYSTEM`]; unspecified is [`ANON`].  The policy layer
//! above the kernel reads [`Session::owner`] to authorize; the kernel itself
//! does not branch on it.
//!
//! The scope is in the log, not only in the value.  Its id is recorded on
//! `session_opened` beside the owner, and on every `budget_*` event, so the
//! boundary is recoverable — and unforgeable, since the kinds that carry it
//! are the kernel's alone to write ([`super::event::is_kernel_only`]).
//! [`Session::resume`] restores the scope from those records rather than
//! being told what it was.
//!
//! # One append, and it lands
//!
//! There is a single write path.  [`Session::append`] validates, stamps
//! the kernel-owned `seq` / `epoch_ms`, and pushes.  It adds nothing else:
//! what an event says beyond the envelope is the caller's.
//!
//! An append *records a fact*, so it is never refused for what the handle
//! last saw.  The store assigns the `seq` and the ordering, and serializes
//! the write per stream; two handles on one stream both write and the log
//! interleaves in arrival order.  No handle keeps a head of its own to be
//! measured against — the store's head is read when something needs it.
//!
//! A *command with an invariant* is the other shape, and it is decided
//! inside the store ([`EventStore::append_if`]), which folds the events it
//! is handed and writes only if the invariant holds, all under the same
//! serialization.  Checking a cached value out here and appending
//! afterwards is exactly the race that would let two handles reserve the
//! same allowance twice.  Every move of the balance is one:
//! [`Session::reserve`] writes a `budget_reserved` if the ledger covers what
//! was asked and a `budget_refused` if it does not, [`Session::spend`] writes
//! a `budget_spent` if there is a ledger at all, and
//! [`Session::grant_on_resume`] writes a `budget_granted` only where one
//! already is.  What all three decide first is the same question — *does this
//! stream have a ledger* — and it is the log's answer, not the handle's: a
//! grant this handle never saw still binds it.  None of them asks whether the
//! session ended — see below.
//!
//! The one thing written as a *batch* is the session's own opening:
//! [`Session::open_on`] records `session_opened` and the `budget_granted`
//! that says what it opened under through [`EventStore::append_many`], so a
//! reader never meets a session that opened without its quota.
//!
//! # The log never refuses a write
//!
//! `closed` is the *handle's* state, not the stream's.  A handle that has
//! closed will not operate again — [`Session::append`], [`Session::reserve`],
//! [`Session::spend`] and [`Session::close`] all read that one local flag —
//! but the log itself turns nothing away.  A write that arrives after a
//! `session_closed`, from another handle that never saw the ending, is
//! recorded like any other, because it *is* a fact: something wrote to a
//! stream that had ended, which is exactly what an audit is there to find.
//! Refusing it would delete the evidence of the bug that produced it.
//!
//! So two handles closing leave two `session_closed` events, not one, and
//! that is the truthful record.  The store's job is to serialize appends and
//! land them; deciding what a stream *ought* to have looked like is a
//! reader's.
//!
//! # A session is disposable
//!
//! Nothing else looks at `session_closed`: [`Session::resume`] is its one
//! reader.  A stream whose log already carries an ending is not continued —
//! what comes after an ending is a new session, not a second life for the
//! old one, or "closed" would say nothing about what a reader of the log can
//! expect after it.  That is where a closed stream refuses; the writes do
//! not.
//!
//! # A child is a fact, not a handle
//!
//! [`Session::open_child`] opens a session from this one and pays for it out
//! of this one's balance.  It is the second *command with an invariant* the
//! kernel has, and the only one whose write spans two streams
//! ([`EventStore::append_if_many`]): the child's `session_opened` and
//! `budget_granted` land on the child's stream in the same transaction as the
//! `budget_reserved` on this one, or a `budget_refused` lands here and no
//! child is opened at all.  Both streams are in one database, which is
//! checked before anything is written — an allocation that could half-land
//! would leave units in neither ledger.
//!
//! The session holds nothing afterwards.  There is no list of children here,
//! no pointer to a parent, and no cascade: what the kernel knows about the
//! structure is in the log — `session_opened.data.parent` on the child, and
//! the child's stream named on the parent's ledger entry — and a supervisor
//! reads it back with a query.  A close *records* the children that had not
//! ended ([`Session::close`], `session_closed.data.open_children`) inside the
//! same write as the boundary, and lands anyway: the log never refuses a
//! write, and what to do about a subtree that outlived its root is a
//! decision, which is not the kernel's to take.
//!
//! # Stored shape change ⇒ upcaster
//!
//! Every read a session makes — the restore fold, the view folds, `events`,
//! the balance fold — goes through the read-time upcaster chain
//! ([`super::event_store::kernel_upcasters`]), so a log written under an
//! older shape reads as the current one and the stored bytes are never
//! rewritten.  The chain is empty until the first release, because there is
//! no released shape to read yet; from then on, a round that changes what is
//! stored ships the matching `n → n+1` step in the same breath.  See the
//! [`super::event_store`] module docs.
//!
//! That is a property of the types here, not a rule to remember: a session
//! holds a [`CurrentStore`] and never a bare backend, so every event it hands
//! to a fold — or out through [`Session::events`] — is a
//! [`Current`](super::event_store::Current), and a read that skipped the
//! chain has no way to reach one.
//!
//! An append does not charge.  It is a record of something that happened,
//! and the budget is a quota: [`Session::reserve`] is a deduction that
//! refuses when the balance is short, [`Session::spend`] is a deduction that
//! does not ask.  They are independent — nothing is held and nothing is
//! released, so a beat that calls both deducts twice — and the layer that
//! knows what a call costs picks.  Folding the budget into the append is what
//! turned it into a flag that only stands up once the allowance is already
//! gone; the balance and what a run actually consumed (a query view over the
//! recorded `llm_response` payloads, on the Lua side) are independent
//! readings and neither is the other's ledger.
//!
//! # The budget is in the log, and nowhere else
//!
//! Every move of the balance is an event — `budget_granted` when an owner
//! allows, `budget_reserved` / `budget_refused` at the decision point,
//! `budget_spent` for a deduction that did not ask — written through the same
//! store.  The
//! balance is not session-local state that dies with the process, and it is
//! not a number kept beside the log either: it *is*
//! [`super::budget::fold_balance`] over the stream, which is why
//! [`Session::remaining`] is right on a stream more than one handle writes
//! to.  Reading it is cheap because the fold is cached against the store's
//! head and retaken only when the head has moved; nothing but that fold ever
//! sets it.  Those kinds are the kernel's alone to write
//! ([`super::event::is_kernel_only`]) — [`Session::append`] refuses them
//! from a caller, because writing one is moving the account.
//!
//! The kernel writes the session's own boundaries through the same append:
//! [`Session::new`] appends `session_opened` and [`Session::close`] appends
//! `session_closed`, so a session is bracketed in the history whether or not
//! the shell remembers to say so.  After a close, *this handle's* `append` /
//! `reserve` / `spend` are errors while reads keep working — the record
//! outlives the session, and another handle's writes go on landing in it.
//!
//! What ends a session is [`close`], and only [`close`] writes the
//! `session_closed` that says so: the flag and the event are set on one
//! path, so a handle's state and what it wrote cannot disagree.  Taking that
//! path twice on one handle writes once, because the flag is already set the
//! second time.
//!
//! [`close`]: Session::close

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};

use super::budget::{self, fold_balance, last_grant, Allocation, BudgetGrant};
use super::event::{
    data_field, is_kernel_only, kernel_event, BUDGET_KINDS, FIELD_AMOUNT, FIELD_CHILD, FIELD_DESC,
    FIELD_DETAIL, FIELD_KIND, FIELD_OPEN_CHILDREN, FIELD_OWNER, FIELD_PARENT, FIELD_REASON,
    FIELD_REMAINING, FIELD_SCOPE_ID, FIELD_TAG, KIND_BUDGET_GRANTED, KIND_BUDGET_REFUSED,
    KIND_BUDGET_RESERVED, KIND_BUDGET_SPENT, KIND_SESSION_CLOSED, KIND_SESSION_OPENED,
};
use super::event_store::{kernel_upcasters, ChildScan, Current, CurrentStore, EventStore, Split};
use super::projection::{tail_count, VIEW_TAIL};
use super::query::{self, QueryOpts, QueryParams, QueryRows};
use super::scope::{Scope, ScopeId};
use super::sqlite_store::{IsleDrivers, SqliteEventStore};
use super::{projection, KnlError, KnlResult};

/// Reason recorded by `close()` when the caller does not give one.
pub const DEFAULT_CLOSE_REASON: &str = "closed";
/// Reason recorded when a session ended on its own: the Lua `<close>`
/// variable holding the session went out of scope with no error.
pub const CLOSE_REASON_SCOPE_EXIT: &str = "scope_exit";
/// Reason recorded when a run scope ended because the block raised: the
/// message goes to [`FIELD_DETAIL`], never into the reason.
pub const CLOSE_REASON_ERROR: &str = "error";
/// Reason recorded by the backstop: the handle died without anyone closing
/// it, so the boundary is written where the value is dropped.
pub const CLOSE_REASON_DROPPED: &str = "dropped";

/// What a closed handle says when it is asked to write.
///
/// One sentence for both places it comes from — a handle that has closed,
/// and a [`Session::resume`] of a stream whose log already ended — because
/// from the caller's side they are the same answer: this session is over,
/// go and open another.
const CLOSED: &str = "session is closed";

/// Whether the stream already carries its ending.
///
/// Asked in exactly one place, [`Session::resume`], because a session is
/// disposable: there is no reopening kind, so a single `session_closed`
/// anywhere in the log means the stream is not a state to continue from.
/// No *write* asks this — a write records what happened, and something
/// writing after an ending is the fact an audit most wants recorded.
///
/// It reads the one kind it is asking about, and at most one of those: the
/// question is whether an ending exists, not where it is or how many there
/// are.
async fn has_ended(store: &CurrentStore) -> KnlResult<bool> {
    let ending = store.read_kinds(Some(&[KIND_SESSION_CLOSED]), 0, 1).await?;
    Ok(!ending.is_empty())
}

/// Reserved owner: no principal was named when the session opened.
pub const ANON: &str = "anon";
/// Reserved owner: the session belongs to the system itself.
pub const SYSTEM: &str = "system";

/// A `budget_granted` event for `grant`, written under `scope_id`.
///
/// Only what the owner said is written — an absent `tag` is an absent
/// field, not a null — so the record carries the grant and nothing more.
/// The fields are the kind's own, so they go under `data`
/// ([`super::event`]); the envelope carries the log's vocabulary and not the
/// ledger's.
fn granted_event(grant: &BudgetGrant, scope_id: &str) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert(
        FIELD_SCOPE_ID.to_string(),
        Value::from(scope_id.to_string()),
    );
    data.insert(FIELD_AMOUNT.to_string(), Value::from(grant.amount));
    if let Some(tag) = grant.tag.as_ref() {
        data.insert(FIELD_TAG.to_string(), Value::from(tag.clone()));
    }
    if let Some(desc) = grant.desc.as_ref() {
        data.insert(FIELD_DESC.to_string(), Value::from(desc.clone()));
    }
    kernel_event(KIND_BUDGET_GRANTED, data)
}

/// A `budget_reserved` / `budget_spent` / `budget_refused` event for
/// `amount`, written under `scope_id` and tagged with the grant's unit so
/// the ledger reads without a join.
///
/// The scope id is on every move of the balance, not only on the session's
/// opening: the ledger is the one part of the log that says what was
/// *allowed*, so each entry names the authority it was allowed under.
fn budget_move_event(
    kind: &str,
    amount: i64,
    tag: Option<&str>,
    scope_id: &str,
) -> Map<String, Value> {
    kernel_event(kind, budget_move_data(amount, tag, scope_id))
}

/// The `data` every `budget_*` entry carries: the scope it was allowed
/// under, how much, and the grant's unit if it named one.
fn budget_move_data(amount: i64, tag: Option<&str>, scope_id: &str) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert(
        FIELD_SCOPE_ID.to_string(),
        Value::from(scope_id.to_string()),
    );
    data.insert(FIELD_AMOUNT.to_string(), Value::from(amount));
    if let Some(tag) = tag {
        data.insert(FIELD_TAG.to_string(), Value::from(tag.to_string()));
    }
    data
}

/// A `budget_refused` event: a move that did not happen, carrying what was
/// asked for *and* the balance it was measured against.
///
/// The pair is what makes a refusal readable without folding the ledger, so
/// `remaining` is built into the entry rather than added to it afterwards.
fn refused_event(
    amount: i64,
    remaining: i64,
    tag: Option<&str>,
    scope_id: &str,
) -> Map<String, Value> {
    let mut data = budget_move_data(amount, tag, scope_id);
    data.insert(FIELD_REMAINING.to_string(), Value::from(remaining));
    kernel_event(KIND_BUDGET_REFUSED, data)
}

/// The parent's side of an allocation: the same `budget_reserved` a
/// [`Session::reserve`] writes, naming the child the units went to.
///
/// An allocation *is* a reservation from the parent's side — units left the
/// balance and are not coming back — so it is the same kind and folds the
/// same way.  What [`FIELD_CHILD`] adds is where they went, which is the one
/// thing a reservation for a call of its own has no answer to.
fn allocated_event(
    amount: i64,
    tag: Option<&str>,
    scope_id: &str,
    child: &str,
) -> Map<String, Value> {
    let mut data = budget_move_data(amount, tag, scope_id);
    data.insert(FIELD_CHILD.to_string(), Value::from(child.to_string()));
    kernel_event(KIND_BUDGET_RESERVED, data)
}

/// The parent's side of an allocation that did not happen: what was asked
/// for, the balance it was measured against, and the child that was not
/// opened.
fn allocation_refused_event(
    amount: i64,
    remaining: i64,
    tag: Option<&str>,
    scope_id: &str,
    child: &str,
) -> Map<String, Value> {
    let mut event = refused_event(amount, remaining, tag, scope_id);
    if let Some(Value::Object(data)) = event.get_mut(super::event::FIELD_DATA) {
        data.insert(FIELD_CHILD.to_string(), Value::from(child.to_string()));
    }
    event
}

/// A child's `session_opened`: the scope it opens under, and the stream it
/// was opened from.
///
/// The same event [`Session::open_on`] writes plus [`FIELD_PARENT`], and
/// written by the parent's store rather than the child's — the opening and
/// the reservation that paid for it are one transaction, so the child's first
/// event arrives before the child has a handle at all.
fn child_opened_event(owner: &str, scope_id: &str, parent: &str) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert(FIELD_OWNER.to_string(), Value::from(owner.to_string()));
    data.insert(
        FIELD_SCOPE_ID.to_string(),
        Value::from(scope_id.to_string()),
    );
    data.insert(FIELD_PARENT.to_string(), Value::from(parent.to_string()));
    kernel_event(KIND_SESSION_OPENED, data)
}

/// A child's `budget_granted`: the units the parent moved, naming where they
/// came from.
///
/// A grant on the child's ledger like any other — its balance is the fold of
/// its own stream, and this is the entry that starts it — with
/// [`FIELD_PARENT`] recording that an owner did not conjure it: it was paid
/// for by a `budget_reserved` on the stream named here, in the same write.
fn child_granted_event(
    amount: i64,
    tag: Option<&str>,
    scope_id: &str,
    parent: &str,
) -> Map<String, Value> {
    let mut data = budget_move_data(amount, tag, scope_id);
    data.insert(FIELD_PARENT.to_string(), Value::from(parent.to_string()));
    kernel_event(KIND_BUDGET_GRANTED, data)
}

/// The `session_closed` event a close records.
///
/// One builder for both close paths — the awaited [`Session::close_with`] and
/// the detached [`Session::close_detached`] — so a boundary written by the
/// backstop is the same event, with the same fields, as one a caller asked
/// for.  The reason defaults to [`DEFAULT_CLOSE_REASON`]; an absent `detail`
/// is an absent field rather than a null.
///
/// `children` are the sessions this one opened that had not ended when it
/// did, found by the store inside the same transaction that writes this
/// event.  An empty list is an absent field, not an empty array: "there were
/// none" and "nobody looked" then read the same way, which is the truth for
/// the detached path — it cannot scan, so it writes none.
fn closing_event(
    reason: Option<&str>,
    detail: Option<&str>,
    children: Vec<String>,
) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert(
        FIELD_REASON.to_string(),
        Value::from(reason.unwrap_or(DEFAULT_CLOSE_REASON)),
    );
    if let Some(detail) = detail {
        data.insert(FIELD_DETAIL.to_string(), Value::from(detail.to_string()));
    }
    if !children.is_empty() {
        data.insert(
            FIELD_OPEN_CHILDREN.to_string(),
            Value::from(children.into_iter().map(Value::from).collect::<Vec<_>>()),
        );
    }
    kernel_event(KIND_SESSION_CLOSED, data)
}

/// How the store recognises the children of this session: the two kernel
/// kinds that bracket a session, and the `data` field a child's opening names
/// its parent in.
///
/// Built here rather than in the store because the vocabulary is the
/// kernel's — [`EventStore::append_with_open_children`] walks a database and
/// knows nothing about what `session_opened` means.
fn child_scan() -> ChildScan {
    ChildScan {
        opened: KIND_SESSION_OPENED.to_string(),
        closed: KIND_SESSION_CLOSED.to_string(),
        parent_field: FIELD_PARENT.to_string(),
    }
}

/// What an allocation's decision is shown: the ledger it measures, and the
/// ending it must not open a child under.
///
/// [`BUDGET_KINDS`] plus `session_closed`, written out rather than
/// concatenated because a `const` cannot join two slices — and held against
/// the ledger's own list by a test below, so a kind added to the ledger and
/// missed here goes red instead of quietly falling out of the balance an
/// allocation is decided against.
const ALLOCATION_KINDS: &[&str] = &[
    KIND_BUDGET_GRANTED,
    KIND_BUDGET_RESERVED,
    KIND_BUDGET_REFUSED,
    KIND_BUDGET_SPENT,
    KIND_SESSION_CLOSED,
];

/// What a caller should have called instead of hand-appending `kind`.
///
/// The refusal names the method that legitimately writes the kind, so the
/// error is a redirection rather than a wall.
fn kernel_only_hint(kind: &str) -> &'static str {
    match kind {
        KIND_SESSION_OPENED => "a session records its own opening",
        KIND_SESSION_CLOSED => "use close",
        _ => "use reserve / spend",
    }
}

/// One session: a stream, and the scope it is written under.
pub struct Session {
    /// Correlation id, unique per session — the stream this session writes.
    /// Distinct from [`Scope::id`], which names the authority the stream is
    /// written under.
    id: String,
    /// The session's scope: its kernel-issued id, its owner, and the budget
    /// an owner granted it.  Held by value — a scope and its session begin
    /// and end together, so there is nothing to point at.
    scope: Scope,
    /// K1 append-only history, held behind the upcasting seam.
    ///
    /// A [`CurrentStore`] and never a bare `Box<dyn EventStore>`: the backend
    /// inside it can be the in-memory store or the durable SQLite one, and
    /// the seam is what makes every read of it — here and in the folds — an
    /// event at the current shape.
    store: CurrentStore,
    /// The last balance fold, and the store head it was taken at.
    ///
    /// Not a counter: nothing adds to it or subtracts from it.  A read of
    /// the balance compares the store's head against the `seq` recorded here
    /// and, if the log has moved on, refolds [`fold_balance`] over the
    /// stream and replaces both halves.  So the answer is the ledger's on a
    /// stream two handles write to, and costs one head read on a stream that
    /// has not moved.
    ///
    /// Behind a lock because reading a balance is a read —
    /// [`Session::remaining`] takes `&self`, and the cache it refreshes is
    /// derived state rather than a change to the session — and because that
    /// read is an `async fn` now, whose future has to be `Send`; a `Cell`
    /// would not be.  The guard is never held across an `.await`: the cached
    /// pair is copied out, the store is asked, and the answer written back.
    balance: Mutex<(u64, Option<i64>)>,
    /// Set by `close()`; blocks this handle's further `append` / `reserve` /
    /// `spend` / `close`.
    ///
    /// The handle's state, not the stream's: another handle on the same
    /// stream keeps its own flag and goes on writing, and the log records
    /// what it writes.  Nothing consults the log to set this.
    closed: bool,
}

impl std::fmt::Debug for Session {
    /// The store is a trait object (`dyn EventStore` is not `Debug`), so it
    /// is summarised rather than printed; the fields that identify the
    /// session are shown.
    ///
    /// The length is not among them: reading it is a call to the store now,
    /// and `Debug` cannot wait for one.  A caller that wants it asks
    /// [`Session::len`].
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("scope", &self.scope)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Open a session for `owner` with an optional budget grant, on an
    /// in-memory store.
    ///
    /// `owner` is total: pass a real principal id, or [`ANON`] / [`SYSTEM`]
    /// for the reserved ones.  The `session_opened` event is appended here,
    /// so a fresh session already has one event.
    ///
    /// "In-memory" is an in-memory *database*, not a different kind of store:
    /// the same SQLite backend a durable session uses, on a database that is
    /// reclaimed when this session lets go of it
    /// ([`SqliteEventStore::open_memory`]).  There is one backend, because
    /// the log is read with SQL and a log that cannot be queried would be a
    /// second, lesser kind of session.
    ///
    /// The stream is minted here and adopted as the session's id, so
    /// [`Session::id`] names the stream this session writes — the same
    /// identity a durable session has, and what `$stream` binds to in a
    /// [`Session::query`].
    pub async fn new(
        owner: String,
        grant: Option<BudgetGrant>,
        drivers: &IsleDrivers,
    ) -> KnlResult<Self> {
        let stream = uuid::Uuid::new_v4().to_string();
        let store = SqliteEventStore::open_memory(stream.clone(), drivers).await?;
        let mut session = Self::open_on(owner, grant, Box::new(store)).await?;
        session.adopt_id(stream);
        Ok(session)
    }

    /// Open a session for `owner` on a caller-chosen backend `store` (the
    /// in-memory store, or the durable SQLite one).
    ///
    /// Like [`Session::new`] but takes the backend, so the shell decides
    /// whether the log is ephemeral or persisted.  It appends the same
    /// `session_opened` boundary, recording the session's scope on it — the
    /// kernel-issued [`ScopeId`] and the `owner` — so a later
    /// [`Session::resume`] can recover the scope from the log alone, and
    /// the `grant`, so the log says what the owner allowed.
    /// `session_opened` is an open-shape reserved kind, so both extra fields
    /// are accepted without any change to the validator.
    ///
    /// The two are written as one batch ([`EventStore::append_many`]), so a
    /// durable stream either carries the opening *and* the quota it opened
    /// under, or carries nothing at all: an open that fails leaves no session
    /// behind to close.
    pub async fn open_on(
        owner: String,
        grant: Option<BudgetGrant>,
        store: Box<dyn EventStore>,
    ) -> KnlResult<Self> {
        // Wrap the chosen backend in the read-time upcasting seam, so every one
        // of this session's reads (view folds, `events`, the balance fold, the
        // decision a `reserve` takes inside the store) passes through it by
        // construction.  The chain is empty until the first release; a shape
        // change after it registers its step at that one site.
        let store = CurrentStore::new(store, kernel_upcasters());
        let mut session = Self {
            id: uuid::Uuid::new_v4().to_string(),
            // The scope is issued here, before the first event: the
            // `session_opened` below is already written under it.
            scope: Scope::new(owner, grant),
            store,
            // Nothing folded yet, over a stream with nothing in it: the first
            // read of the balance sees the head move and folds the ledger the
            // two appends below are about to write.
            balance: Mutex::new((0, None)),
            closed: false,
        };
        // The scope rides on the opening: the id the kernel just issued, next
        // to the owner.  Together they are the whole of what a resume needs to
        // restore the scope, so the boundary is in the log and not only in
        // this value — and they are the kind's own fields, so they go under
        // `data` where the validator requires them.
        let mut opened = Map::new();
        opened.insert(
            FIELD_OWNER.to_string(),
            Value::from(session.scope.owner().to_string()),
        );
        opened.insert(
            FIELD_SCOPE_ID.to_string(),
            Value::from(session.scope.id().to_string()),
        );
        let started = kernel_event(KIND_SESSION_OPENED, opened);

        // The grant is its own fact, right after the boundary: what the
        // owner allowed is the first entry of the ledger the balance folds
        // from, not a decoration on the run's opening.
        let mut opening = vec![started];
        if let Some(grant) = session.scope.grant().cloned() {
            opening.push(granted_event(&grant, session.scope.id()));
        }

        // One write for both.  It CAN fail on a durable backend (a busy
        // database exhausts its retries) — a session that could not record
        // its own opening must not exist, so the error surfaces — and because
        // the two events are one write, a failure leaves the stream *empty*
        // rather than opened-without-a-grant.  There is no half-opened stream
        // to close on the way out, which is what the earlier best-effort
        // `session_closed` here was patching over.
        //
        // The session is being built, so it cannot have been closed: the
        // guarded path `append_kernel` takes has nothing to check yet.
        session.store.append_many(opening).await?;
        Ok(session)
    }

    /// Continue an existing session by re-folding its persisted log.
    ///
    /// The `store` already holds a session's events (a reopened SQLite
    /// stream), so resume does *not* append a new `session_opened` — the
    /// session already opened.  It reads the whole log once and restores the
    /// state from it:
    ///
    /// - the scope, from the first `session_opened` event: its [`ScopeId`]
    ///   ([`FIELD_SCOPE_ID`]) and its `owner` ([`FIELD_OWNER`]).  An older
    ///   log written before either was recorded falls back — to a fresh
    ///   kernel-issued scope id, and to [`ANON`] — rather than failing the
    ///   resume;
    /// - the grant, from the last `budget_granted` the log carries, so a
    ///   reopened stream goes on keeping a ledger and a refusal still has a
    ///   `tag` to report.  The balance itself needs no restoring: it is
    ///   [`fold_balance`] over the stream, and the stream is right there.
    ///
    /// A `grant` passed here is the owner granting *again*: it is appended
    /// as a new `budget_granted` and raises the restored balance, rather
    /// than replacing it.  Omit it to continue on what is left.  Nothing is
    /// deducted for the earlier `llm_response` usage — an append never
    /// charged, and what was consumed is a query view's answer over the
    /// recorded payloads, not the quota's.
    ///
    /// A closed stream is not resumed.  A session is disposable: it opens
    /// once and ends once, so a log that already carries its `session_closed`
    /// is an ending, not a state to continue from — the caller opens a new
    /// session instead.
    ///
    /// Nothing is restored for the read side: a resumed session's reads —
    /// `events`, `tail`, a query — go to the reopened store, so they see the
    /// whole stream on the first call.
    pub async fn resume(grant: Option<BudgetGrant>, store: Box<dyn EventStore>) -> KnlResult<Self> {
        // The upcasting seam goes on first, so the restore below reads the
        // same projected shape every other read of this session gets: a log
        // written under an older shape resumes as what it means today, and the
        // stored bytes stay as they were written.
        Self::resume_on(grant, CurrentStore::new(store, kernel_upcasters())).await
    }

    /// [`Session::resume`] on a store that is already behind the seam.
    ///
    /// The body of the resume, split off so a test can hand it a chain of its
    /// own; the public entry wraps the backend in [`kernel_upcasters`] and
    /// calls this.
    async fn resume_on(grant: Option<BudgetGrant>, store: CurrentStore) -> KnlResult<Self> {
        // Fallible read: a transient busy read or an undecodable row surfaces
        // here rather than being silently folded into a wrong resumed state.
        //
        // Unfiltered, unlike the reads a running session takes: a resume is
        // restoring the whole of the state, and it is looking for the opening
        // *whatever it was written as* — the chain may have renamed the kind
        // on the way through, and a filter selects on the stored name
        // ([`EventStore::read_kinds`]).
        let log = store.read(0, usize::MAX).await?;

        // Resuming an empty or mistyped stream is a caller error, not an
        // anonymous zero session: a real session always opens with a
        // `session_opened`.
        let opened = log
            .iter()
            .find(|event| event.kind() == KIND_SESSION_OPENED)
            .ok_or_else(|| {
                // The caller pointed a resume at a stream that is not a
                // session — a bad argument, not a damaged log.
                KnlError::Validation(
                    "stream has no session to resume (no session_opened event)".to_string(),
                )
            })?;

        // …and an ended one is not resumed at all.  There is no reopening
        // kind, so any `session_closed` in the stream is the session's
        // ending: a handle that carried on past it would be appending to a
        // log whose readers were told nothing more was coming.
        if has_ended(&store).await? {
            return Err(KnlError::Closed(format!(
                "{CLOSED} (disposable; open a new session)"
            )));
        }

        // The scope rides on the `data` of `session_opened`, where the
        // validator requires both halves of it.  The fallbacks — the owner to
        // ANON, the scope id to a fresh kernel-issued one (`Scope::restore`
        // mints it) — are for a stream an upcaster could not bring all the
        // way: a session that arrives here missing a field is still a
        // session, and refusing to resume it would lose the log rather than
        // protect it.
        let owner = data_field(opened, FIELD_OWNER)
            .and_then(Value::as_str)
            .unwrap_or(ANON)
            .to_string();
        let scope_id: Option<ScopeId> = data_field(opened, FIELD_SCOPE_ID)
            .and_then(Value::as_str)
            .map(str::to_string);

        // The log was read once already, so seed the balance cache from it
        // rather than folding the same events again on the first read: the
        // head it was taken at is the last event's seq (`read` returns events
        // in seq order).  A resume errors above on an empty /
        // session_opened-less log, so this is a real event's seq; the `0`
        // fallback is unreachable but keeps it total.
        let head = log.last().map(Current::seq).unwrap_or(0);

        // The grant comes back off the log: a resumed session keeps having a
        // budget (and a tag to report) even when the caller grants nothing
        // new.  What is left of it is the fold, seeded just below — over the
        // whole log, which folds to the same balance as the ledger alone
        // because nothing else moves it.
        let mut session = Self {
            id: uuid::Uuid::new_v4().to_string(),
            scope: Scope::restore(scope_id, owner, last_grant(&log)),
            store,
            balance: Mutex::new((head, fold_balance(&log))),
            closed: false,
        };

        // A fresh grant is the owner allowing more, so it is recorded like
        // any other and *adds* to what was left — and only on a stream that
        // already keeps a ledger ([`Session::grant_on_resume`]).
        //
        // A caller that must vet the restored session *before* anything is
        // written for it — the Lua bridge, which refuses a reserved owner —
        // resumes with no grant and calls [`Session::grant_on_resume`] once
        // the stream has passed.
        if let Some(grant) = grant {
            session.grant_on_resume(grant).await?;
        }
        Ok(session)
    }

    /// The owner granting again *on a resume*: record `budget_granted`, but
    /// only on a stream whose ledger already carries one.
    ///
    /// **One session, one budget.**  Whether a session has a quota is settled
    /// when it opens: a stream that opened with a grant keeps a ledger for the
    /// whole of its life, and a stream that opened without one has no ledger
    /// and refuses nothing.  A resume may raise the first — that is the owner
    /// allowing more — and may not create the second, because a session that
    /// opened unbudgeted has handles that were told there is no quota, and a
    /// ledger appearing underneath them turns "refuses nothing" into "refuses"
    /// with nobody having asked for it.  So a `budget` on a resume of a stream
    /// with no `budget_granted` is a [`KnlError::Validation`]: the caller
    /// wanted [`Session::open_on`], and nothing is written.
    ///
    /// The question is decided **inside the transaction that would write the
    /// grant** ([`EventStore::append_if`]), not read beforehand: two resumes
    /// racing on one stream would otherwise both see an empty ledger and both
    /// create one.
    ///
    /// [`Session::grant_more`] is the other door and keeps no such rule: it is
    /// the owner acting through a handle it holds, on a session it opened,
    /// rather than a second handle changing what a first one was told.
    async fn grant_on_resume(&mut self, grant: BudgetGrant) -> KnlResult<()> {
        // Before anything is decided: an amount the ledger cannot take is the
        // caller's own bug, and the log should not carry the evidence twice.
        budget::check_amount(grant.amount)?;
        let scope_id = self.scope.id().to_string();
        let recorded = grant.clone();

        let committed = self
            .store
            .append_if(
                Some(BUDGET_KINDS),
                Box::new(move |events: Vec<Current>| {
                    last_grant(&events)?;
                    Some(granted_event(&recorded, &scope_id))
                }),
            )
            .await?;

        if committed.is_none() {
            return Err(KnlError::Validation(
                "this stream opened with no budget, and a resume does not give one: a session's \
                 quota is settled when it opens (open a new session with the grant, or resume \
                 without one)"
                    .to_string(),
            ));
        }
        self.scope.grant_more(grant)
    }

    /// The owner granting again: record `budget_granted` and raise the
    /// balance by it.
    ///
    /// The one way the balance rises, and it is a fact in the log before it
    /// is a number in the counter — a failed append leaves the balance
    /// exactly as the ledger describes it.  Refused on a closed session, like
    /// every other write: a run that has ended cannot be granted more.
    ///
    /// This is the owner acting through a handle it holds, so it takes the
    /// ledger as it finds it — including a stream that has none, which this
    /// grant then starts.  [`Session::grant_on_resume`] is the other door and
    /// refuses that case: a *resume* is a second handle arriving at a session
    /// that already exists, and it does not get to give one a quota it opened
    /// without.
    pub async fn grant_more(&mut self, grant: BudgetGrant) -> KnlResult<()> {
        let event = granted_event(&grant, self.scope.id());
        self.append_kernel(event).await?;
        self.scope.grant_more(grant)
    }

    /// Open a session *from* this one, paying for it out of this session's
    /// balance — one transaction, both ledgers.
    ///
    /// The kernel's whole part in a session tree.  It records two facts and
    /// performs one move:
    ///
    /// - the child's `session_opened` carries [`FIELD_PARENT`] — this
    ///   session's stream — and the `budget_granted` it opens with carries it
    ///   too, so where the units came from is in the log beside them;
    /// - this session's ledger gains a `budget_reserved` naming the child
    ///   ([`FIELD_CHILD`]).  An allocation is a *spend* from here: the
    ///   balance falls by exactly what the child's rises by, and nothing is
    ///   returned when the child closes.  A refund would be a balance rising
    ///   without an owner granting, which is the one thing the ledger does
    ///   not allow.
    ///
    /// All of it is decided and written inside one transaction on this
    /// session's store ([`EventStore::append_if_many`]), so two children
    /// asking at once cannot both be given what only one balance covers, and
    /// no reader ever meets a child that opened without the reservation that
    /// paid for it.
    ///
    /// **The child is on the parent's database.**  `child_store` must be a
    /// store on the same database ([`EventStore::database`]) opened on
    /// `child_stream`; anything else is a [`KnlError::Validation`] before a
    /// word is written.  A tree spread over two logs could be neither written
    /// atomically nor read back by one statement, so it is not a tree.
    ///
    /// **A refusal is an error here, not a `false`.**  When the balance does
    /// not cover the allocation, a `budget_refused` naming the child is
    /// recorded on this session, nothing is opened, and
    /// [`KnlError::Refused`] is raised: unlike [`Session::reserve`], which is
    /// asked in a loop that expects to be told no, an allocation either
    /// produced a session or it did not, and there is no half-opened one to
    /// hand back.
    ///
    /// **The parent must be open.**  This handle having closed is refused
    /// straight away, and a stream whose log already carries an ending is
    /// refused *inside the transaction* — the decision is shown
    /// `session_closed` along with the ledger — both as
    /// [`KnlError::Closed`], the same answer a resume of a closed stream
    /// gives.
    ///
    /// The child comes back as an ordinary session with its scope restored
    /// from the events just written ([`Session::resume`] over the two of
    /// them): its balance is the fold of its own ledger, its owner and scope
    /// are what the opening recorded, and nothing about it is special
    /// afterwards.  What it is *not* is a handle this session holds — the
    /// parent keeps no pointer, and a supervisor reads the structure back out
    /// of the log.
    pub async fn open_child(
        &mut self,
        child_stream: String,
        owner: String,
        allocation: Allocation,
        child_store: Box<dyn EventStore>,
    ) -> KnlResult<Self> {
        if self.closed {
            return Err(KnlError::Closed(format!(
                "{CLOSED} (a child is opened from an open parent)"
            )));
        }
        budget::check_amount(allocation.amount)?;

        // One log, checked before anything is written: the two halves of an
        // allocation land in one transaction, and a transaction covers one
        // database.
        match (self.store.database(), child_store.database()) {
            (Some(parent), Some(child)) if parent == child => {}
            (Some(parent), Some(child)) => {
                return Err(KnlError::Validation(format!(
                    "a child opens on its parent's database, and a tree is one log: the parent is \
                     on {parent:?} and the child was given {child:?}"
                )));
            }
            _ => {
                return Err(KnlError::Validation(
                    "a child opens on its parent's database, and one of the two stores keeps a \
                     single stream with no database to share"
                        .to_string(),
                ));
            }
        }

        let amount = allocation.amount;
        // The units come out of this ledger, so they are counted in its unit
        // unless the caller renamed them for the child.
        let parent_tag = self.scope.grant().and_then(|grant| grant.tag.clone());
        let child_tag = allocation.tag.clone().or_else(|| parent_tag.clone());
        let parent_scope = self.scope.id().to_string();
        let parent_id = self.id.clone();
        let child_id = child_stream.clone();

        // The child's scope is issued before its first event, exactly as
        // `open_on` issues one.  The value is not kept: the handle below is
        // built by a resume, which restores the scope from the very
        // `session_opened` this id is about to be written onto.
        let child_scope_id = Scope::new(owner.clone(), None).id().to_string();

        // What the decision saw, carried out of it: the decision runs on the
        // store's own thread, so the balance a refusal reports and the
        // ending it found come back through cells rather than through the
        // events it returns.
        let ended = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(AtomicBool::new(false));
        let measured = Arc::new(AtomicI64::new(0));
        let (found_ending, said_no, balance_seen) = (
            Arc::clone(&ended),
            Arc::clone(&refused),
            Arc::clone(&measured),
        );

        let committed = self
            .store
            .append_if_many(
                &child_stream,
                Some(ALLOCATION_KINDS),
                Box::new(move |events: Vec<Current>| {
                    // The ending is part of the invariant, not a check taken
                    // beforehand: a parent that closed between the read and
                    // the write would otherwise get a child anyway.
                    if events.iter().any(|e| e.kind() == KIND_SESSION_CLOSED) {
                        found_ending.store(true, Ordering::Relaxed);
                        return None;
                    }
                    // No grant on the parent is no ledger to measure against
                    // — the same rule `reserve` follows — so the allocation
                    // is allowed and recorded, and the fold ignores a
                    // reservation with nothing granted before it.
                    if let Some(balance) = fold_balance(&events) {
                        if balance < amount {
                            said_no.store(true, Ordering::Relaxed);
                            balance_seen.store(balance, Ordering::Relaxed);
                            return Some(Split::own(vec![allocation_refused_event(
                                amount,
                                balance,
                                parent_tag.as_deref(),
                                &parent_scope,
                                &child_id,
                            )]));
                        }
                    }
                    Some(Split {
                        own: vec![allocated_event(
                            amount,
                            parent_tag.as_deref(),
                            &parent_scope,
                            &child_id,
                        )],
                        other: vec![
                            child_opened_event(&owner, &child_scope_id, &parent_id),
                            child_granted_event(
                                amount,
                                child_tag.as_deref(),
                                &child_scope_id,
                                &parent_id,
                            ),
                        ],
                    })
                }),
            )
            .await?;

        if committed.is_none() || ended.load(Ordering::Relaxed) {
            return Err(KnlError::Closed(format!(
                "{CLOSED} (the parent's log already carries its ending)"
            )));
        }
        if refused.load(Ordering::Relaxed) {
            let balance = measured.load(Ordering::Relaxed);
            return Err(KnlError::Refused(format!(
                "the parent's balance is {balance}, which does not cover an allocation of \
                 {amount}; the refusal is in the log and no child was opened"
            )));
        }

        // The opening and the grant are committed, so the child's stream is a
        // session: resuming it restores the scope and folds the balance out
        // of the two events that were just written for it.
        let mut child = Self::resume(None, child_store).await?;
        child.adopt_id(child_stream);
        Ok(child)
    }

    /// The session-correlation id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Adopt `id` as the session-correlation id.
    ///
    /// Used so a session and the SQLite stream it writes to share one id:
    /// `open_on` / `resume` mint a fresh id, and the caller that opened the
    /// stream overrides it to that stream, so the id a caller resumes by *is*
    /// the stream — and so `$stream` in a [`Session::query`] means this
    /// session's own rows.  `session_opened` records the scope (its id and
    /// the `owner`), never the session id, so overriding the id does not
    /// desync the log.
    pub(crate) fn adopt_id(&mut self, id: impl Into<String>) {
        self.id = id.into();
    }

    /// The scope this session is written under.
    ///
    /// The scope and the session are two things with one lifetime: this is
    /// the authority half — the id, the owner, the granted quota — while the
    /// session is the stream.  [`Session::owner`] / [`Session::scope_id`]
    /// read through to it for the two call sites that want one field.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The scope's kernel-issued id, as recorded on `session_opened` and on
    /// every `budget_*` event.
    ///
    /// Not the session id: [`Session::id`] names the stream, this names the
    /// authority the stream is written under.
    pub fn scope_id(&self) -> &str {
        self.scope.id()
    }

    /// Whose scope this is (a principal id, or [`ANON`] / [`SYSTEM`]).
    pub fn owner(&self) -> &str {
        self.scope.owner()
    }

    /// The database this session's log lives in, or `None` for a backend that
    /// is not one ([`EventStore::database`]).
    ///
    /// Published for one caller: whoever opens a child has to open its store
    /// on the parent's database, and asking the parent is how it knows which
    /// that is ([`Session::open_child`] refuses any other).  It is an
    /// identity to pass along, not a location to take apart.
    pub fn database(&self) -> Option<&str> {
        self.store.database()
    }

    /// Record an event, returning its `seq`.  The one write path.
    ///
    /// Any kind is welcome, the reserved ones included, as long as it meets
    /// the shape its kind requires and is not one of the kernel's own
    /// ([`is_kernel_only`]).  The kernel-owned `seq` / `epoch_ms` are
    /// stamped here and overwrite any caller-supplied value; nothing else
    /// is added, and a `beat` the caller declared is recorded as given.
    ///
    /// No append moves the budget, this one included.  A deduction is asked
    /// for before a call ([`Session::reserve`]) or taken after it
    /// ([`Session::spend`]) by the layer that knows what a call costs; the
    /// history records what happened and says nothing about what was
    /// allowed.
    ///
    /// The session's own boundaries are not appendable: `session_opened` and
    /// `session_closed` are written by [`Session::open_on`] and
    /// [`Session::close`], and a caller asking for either is refused.
    ///
    /// The append lands.  The store assigns the `seq` and serializes the
    /// write per stream, so two handles on one stream both record and the log
    /// interleaves in the order the writes arrived.  A stale view of the head
    /// is not a reason to refuse a fact: [`Session::head`] is what this handle
    /// last saw, and nothing is compared against it.
    ///
    /// The one refusal is this handle having closed.  Another handle's close
    /// is not: it set *that* handle's flag, and a write landing after the
    /// `session_closed` it wrote is recorded, because that is what happened.
    pub async fn append(&mut self, event: Map<String, Value>) -> KnlResult<u64> {
        // The kernel's own kinds are refused before the closed check has
        // anything to say about it — the kind is wrong whatever state the
        // session is in.  The balance is a fold of the `budget_*` events, so
        // accepting one from a caller would be letting it grant itself the
        // quota its owner set; the two `session_*` events are the lifecycle
        // a resume and an audit read off the log.
        let kind = event.get(FIELD_KIND).and_then(Value::as_str).unwrap_or("");
        if is_kernel_only(kind) {
            return Err(KnlError::Validation(format!(
                "{kind:?} is written by the kernel only ({})",
                kernel_only_hint(kind)
            )));
        }
        self.append_kernel(event).await
    }

    /// [`Session::append`] without the kernel-only guard: the path the
    /// kernel's own writes take.
    ///
    /// Same validation, same stamping, same serialized write — the only
    /// difference is that this one may write the kernel-only kinds, which is
    /// what makes "the kernel wrote it" a property of the code path rather
    /// than of a field a caller could set.
    ///
    /// A plain append, with no invariant to decide: the only refusal is the
    /// handle's own `closed` flag, checked here before the store is touched.
    /// The store is not asked whether the stream has ended, because a write
    /// arriving after an ending is a fact — evidence of a bug or a misuse —
    /// and dropping it would hide the one thing an audit is reading for.
    async fn append_kernel(&mut self, event: Map<String, Value>) -> KnlResult<u64> {
        if self.closed {
            return Err(KnlError::Closed(CLOSED.to_string()));
        }

        // The store orders the write and hands back where it landed.
        let committed = self.store.append(event).await?;
        Ok(committed.seq)
    }

    /// Events with `seq >= from`, cloned, at the current shape.
    ///
    /// They come back as [`Current`]s: the read went through the upcaster
    /// seam, and the type says so, so a caller folding them cannot be folding
    /// a shape that has been superseded.  A caller that needs to own the
    /// underlying object — the Lua bridge, building tables — takes it with
    /// [`Current::into_inner`].
    ///
    /// Fallible: a durable backend can hit a transient busy read or a row it
    /// cannot decode, which surfaces here rather than being dropped silently.
    /// The in-memory backend is always `Ok`.
    pub async fn events(&self, from: u64) -> KnlResult<Vec<Current>> {
        self.store.read(from, usize::MAX).await
    }

    /// Number of recorded events.  Fallible like [`Session::events`].
    pub async fn len(&self) -> KnlResult<usize> {
        self.store.len().await
    }

    /// Whether the history is empty (only before `session_opened`, i.e.
    /// never for a session built by [`Session::new`]).
    pub async fn is_empty(&self) -> KnlResult<bool> {
        self.store.is_empty().await
    }

    /// Ask the budget to allow `amount`: `true` when it was deducted, `false`
    /// when the balance would not cover it (and nothing was deducted).
    ///
    /// The stop the budget exists for.  A caller asks before it spends, and
    /// a `false` is a planned halt with the balance untouched — not a
    /// failure, and not a state the run has to be rolled back out of.
    ///
    /// **Whether there is a budget at all is the log's answer, not this
    /// handle's.**  The decision is shown the stream's `budget_*` events, and
    /// a ledger with no `budget_granted` in it is a run with no quota: nothing
    /// is decided, nothing is recorded, and the answer is `true`.  A grant the
    /// log *does* carry is honoured even by a handle that was opened without
    /// one — two handles on one stream cannot disagree about whether it has a
    /// budget, because neither of them is asked.  The scope's cached grant is
    /// a hint about the words (§ [`Session::grant`]) and never the authority.
    ///
    /// **Both answers are recorded, by the same decision.**  A
    /// `budget_reserved` when the balance covered it, a `budget_refused`
    /// (carrying what was asked for and what there was) when it did not —
    /// whichever the decision built lands in the transaction that took it, so
    /// exactly one of the two is in the log and this call's answer is which
    /// one that was.  Recording the refusal afterwards, as a second append,
    /// made a refusal that could not be written indistinguishable from a
    /// storage failure with nothing decided.
    ///
    /// This is a *command with an invariant*, so the decision is taken inside
    /// the store ([`EventStore::append_if`]): the backend hands the ledger to
    /// [`fold_balance`] and writes the decision's event in the same serialized
    /// write, so two handles on one stream cannot both reserve the same
    /// allowance.  Nothing is set afterwards: the write moved the store's
    /// head, so the next read of [`Session::remaining`] refolds the ledger the
    /// entry is now part of.
    ///
    /// A handle that has closed refuses, like [`Session::spend`]; another
    /// handle's close is nothing to this one — the balance is the whole of the
    /// invariant.
    pub async fn reserve(&mut self, amount: i64) -> KnlResult<bool> {
        if self.closed {
            return Err(KnlError::Closed(CLOSED.to_string()));
        }
        budget::check_amount(amount)?;
        let scope_id = self.scope.id().to_string();

        // Which of the two entries the decision built, carried back out of
        // it: the decision runs wherever the store serializes its writes —
        // the connection's own thread — so what it decided comes back through
        // a cell rather than through the `Committed` it returns, exactly as
        // [`Session::open_child`] carries its own refusal back.  It is set at
        // the moment the refusal event is built, and that event is the one
        // the transaction writes, so the flag *is* which kind landed.
        let refused = Arc::new(AtomicBool::new(false));
        let said_no = Arc::clone(&refused);
        let decided_scope = scope_id.clone();
        // The whole of the decision: is there a ledger, and does it cover
        // what was asked.  Whether the stream carries an ending is not part
        // of it — a reservation past the boundary is a fact about a run that
        // overran its own close, and the log is where facts go.
        //
        // The decision names the kinds it folds, so the store hands it the
        // ledger and not the whole stream: the invariant is exact either way,
        // and this way it costs the size of the ledger.
        let committed = self
            .store
            .append_if(
                Some(BUDGET_KINDS),
                Box::new(move |events: Vec<Current>| {
                    // No `budget_granted` in the log is no ledger to move, so
                    // there is nothing to decide and nothing to record.  The
                    // grant is also where the unit comes from: the tag is read
                    // off the log's own last grant rather than off whatever
                    // this handle happens to remember.
                    let grant = last_grant(&events)?;
                    let balance = fold_balance(&events).unwrap_or(0);
                    if balance >= amount {
                        return Some(budget_move_event(
                            KIND_BUDGET_RESERVED,
                            amount,
                            grant.tag.as_deref(),
                            &decided_scope,
                        ));
                    }
                    said_no.store(true, Ordering::Relaxed);
                    Some(refused_event(
                        amount,
                        balance,
                        grant.tag.as_deref(),
                        &decided_scope,
                    ))
                }),
            )
            .await?;

        // A refusal that landed is the only `false`.  Nothing committed means
        // the log carries no ledger, which is the run with no quota: it is
        // allowed, and there is nothing to write about it.
        match (committed, refused.load(Ordering::Relaxed)) {
            (Some(_), true) => Ok(false),
            _ => Ok(true),
        }
    }

    /// Deduct `amount` from the budget without asking.
    ///
    /// The other half of [`Session::reserve`], and an independent one: a
    /// reserve is a deduction that *refuses* when the balance is short, a
    /// spend is a deduction that does not ask — it floors at `0` rather than
    /// refusing.  Neither holds anything for the other to release, so calling
    /// both for one beat deducts twice; the layer above decides which of them
    /// a beat uses.  It is recorded as a `budget_spent`, which is the whole of
    /// the move.
    ///
    /// **Whether there is a budget at all is the log's answer**, exactly as it
    /// is for [`Session::reserve`]: the decision is shown the ledger, and a
    /// stream with no `budget_granted` in it has no account to move, so
    /// nothing is written.  That question is inside the same transaction as
    /// the write for the same reason the balance is — a handle's memory of
    /// what it opened with is not what the ledger says.
    ///
    /// There is no *balance* invariant to hold, so a spend is never refused
    /// for what the account holds.  What the decision decides is only whether
    /// there is an account.
    ///
    /// **The write is the result.**  It used to hand back the balance it read
    /// afterwards, which made a `spend` that landed and then failed its
    /// read-back indistinguishable from one that never landed: the caller got
    /// an error either way and could not tell whether the deduction was in
    /// the log.  Two questions, two calls — this one says the move was
    /// recorded, and [`Session::remaining`] says what is left, failing on its
    /// own terms.
    ///
    /// A handle that has closed refuses before the store is reached; another
    /// handle's close does not, and a deduction landing after one is recorded
    /// as what it is.
    pub async fn spend(&mut self, amount: i64) -> KnlResult<()> {
        if self.closed {
            return Err(KnlError::Closed(CLOSED.to_string()));
        }
        budget::check_amount(amount)?;
        let scope_id = self.scope.id().to_string();

        self.store
            .append_if(
                Some(BUDGET_KINDS),
                Box::new(move |events: Vec<Current>| {
                    // No `budget_granted` in the log is no ledger to move, and
                    // the log's own last grant is where the unit comes from.
                    let grant = last_grant(&events)?;
                    Some(budget_move_event(
                        KIND_BUDGET_SPENT,
                        amount,
                        grant.tag.as_deref(),
                        &scope_id,
                    ))
                }),
            )
            .await?;
        Ok(())
    }

    /// The grant this run opened (or resumed) with, if any.
    ///
    /// Read for its words — the `tag` a caller reports when a reservation
    /// is refused — and for its presence, which is what says this session
    /// keeps a ledger at all.  The amount on it is the *last* grant, not
    /// what is left: that is [`Session::remaining`].
    pub fn grant(&self) -> Option<&BudgetGrant> {
        self.scope.grant()
    }

    /// The remaining balance: `Ok(None)` without a budget.
    ///
    /// The ledger's answer, not a counter's: [`fold_balance`] over the
    /// stream, so a handle that has written nothing still sees what another
    /// handle spent.  The fold is cached against the store's head and retaken
    /// only when the head has moved, so a read on a quiet stream costs one
    /// head query.
    ///
    /// Fallible, and deliberately so.  A store that cannot be read has *no*
    /// answer to give, and the two answers this call can otherwise hand back
    /// — the last fold, or `None` — both read as facts about the budget:
    /// "you have this much" and "there is no budget here".  Serving either
    /// off a failed read would fold a failure into a value, and the caller
    /// most likely to act on it is a loop deciding whether it may go on
    /// spending.  So the failure surfaces, and what to do about a transient
    /// busy read ([`KnlError::is_retryable`]) is the caller's to decide.
    pub async fn remaining(&self) -> KnlResult<Option<i64>> {
        // Copied out and the guard released: nothing below waits while it is
        // held.
        let (folded_head_seq, cached) =
            *self.balance.lock().unwrap_or_else(PoisonError::into_inner);

        let head = self.store.head().await?.unwrap_or(0);
        // The log has not moved since the fold, so neither has the balance.
        if head <= folded_head_seq {
            return Ok(cached);
        }

        // Only the ledger is folded — the balance is a fold of the `budget_*`
        // kinds and nothing else — while the *head* it is recorded against is
        // the whole stream's, so any write at all makes the next read refold.
        // Conservative in the safe direction: an event that moves no balance
        // costs one extra fold, never a stale answer.
        let ledger = self
            .store
            .read_kinds(Some(BUDGET_KINDS), 0, usize::MAX)
            .await?;
        let balance = fold_balance(&ledger);
        *self.balance.lock().unwrap_or_else(PoisonError::into_inner) = (head, balance);
        Ok(balance)
    }

    /// Whether the budget is used up (never true without a budget).
    ///
    /// The same fold [`Session::remaining`] reads, asked as a question — and
    /// fallible for the same reason: a `false` that meant "the store could
    /// not be read" is the one answer a run must never be given, because it
    /// reads as "carry on".
    pub async fn exhausted(&self) -> KnlResult<bool> {
        Ok(matches!(self.remaining().await?, Some(remaining) if remaining <= 0))
    }

    /// Whether the session has ended.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// End the session, recording `session_closed` with `reason`
    /// (defaulting to [`DEFAULT_CLOSE_REASON`]).
    ///
    /// Idempotent *per handle*: closing a session this handle already closed
    /// records nothing.  Another handle closing the same stream is a second
    /// ending in the log — the truthful record of two handles both believing
    /// they owned the session, and the shape an audit needs to see.
    ///
    /// **Open children are recorded, never a refusal.**  In the same write,
    /// the store looks for the streams that name this session as their parent
    /// and carry no ending of their own ([`Session::open_child`]); if it
    /// finds any, their ids go on the boundary as
    /// `data.open_children`.  The close still succeeds — the log turns no
    /// write away, and a run that ended while what it started was still going
    /// is exactly the fact worth having in it.
    ///
    /// Fallible on a durable backend: the `session_closed` append can fail on
    /// a database that stays contended past its retries, or a store that is
    /// gone.  On failure the session stays open (closed is not set), so the
    /// caller knows the boundary was NOT recorded and can retry — a close
    /// that reports success with no `session_closed` in the log would
    /// silently break resume/audit reads.
    pub async fn close(&mut self, reason: Option<&str>) -> KnlResult<()> {
        self.close_with(reason, None).await
    }

    /// [`Session::close`] with a free-text `detail` recorded beside the
    /// reason.
    ///
    /// The reason names *which kind of ending* this was, and stays a short
    /// vocabulary a reader can fold on; `detail` is the sentence that only
    /// this close can tell — the message of the error that ended the scope.
    /// Keeping them apart is what stops every distinct error message from
    /// becoming its own reason.
    ///
    /// Idempotent and fallible exactly like [`Session::close`].
    pub async fn close_with(
        &mut self,
        reason: Option<&str>,
        detail: Option<&str>,
    ) -> KnlResult<()> {
        if self.closed {
            return Ok(());
        }
        // Owned, because the event is built on the store's own thread: the
        // decision below travels there with the scan it is answered from.
        let reason = reason.map(str::to_string);
        let detail = detail.map(str::to_string);

        // The boundary and the scan that finds this session's open children
        // are one write.  A close is never refused for them — the log turns
        // nothing away, and a run that ended while what it started was still
        // going is the fact an audit is reading for — so what the scan
        // produces is recorded on the event rather than raised at the caller.
        //
        // Kernel-only kinds do not go through the guarded `append`, which
        // would refuse the very event that ends the session; this path is the
        // kernel's own, like `append_kernel`, and carries the same `closed`
        // check above.
        //
        // The flag moves only after the boundary landed, so a failed write
        // leaves this handle open and the caller free to retry — a close that
        // reported success with nothing in the log would break every later
        // read of it.
        self.store
            .append_with_open_children(
                &child_scan(),
                Box::new(move |children| {
                    closing_event(reason.as_deref(), detail.as_deref(), children)
                }),
            )
            .await?;
        self.closed = true;
        Ok(())
    }

    /// End the session by handing `session_closed` to the store and *not*
    /// waiting for it — the drop backstop.
    ///
    /// The one close path with nobody left to tell.  A handle that was
    /// collected without ever being closed is being dropped right now, on the
    /// VM's own thread, inside a Lua collection cycle: there is no task to
    /// suspend in and nothing that may block, so the event goes to the store's
    /// own writer ([`super::EventStore::detach_append`]) and whether it landed
    /// is reported to the log rather than to a caller.
    ///
    /// The connection thread outlives this handle — its driver belongs to the
    /// host, not to the session ([`super::IsleDrivers`]) — so the submitted
    /// event is still executed, and the host's shutdown drains it.
    ///
    /// Idempotent per handle, like [`Session::close`]: a session this handle
    /// already closed records nothing.
    pub fn close_detached(&mut self, reason: &str) {
        if self.closed {
            return;
        }
        self.store
            .detach_append(closing_event(Some(reason), None, Vec::new()));
        self.closed = true;
    }

    /// A named projection over the history.
    ///
    /// `tail` is the only name, and it reads `opts.n` (default
    /// [`projection::DEFAULT_TAIL_N`]) events from the end.  An unknown name
    /// is an error — the vocabulary is closed on purpose, and it is as short
    /// as it goes: a projection whose shape depends on what the caller does
    /// with it is built above the kernel, from [`Session::events`] or with
    /// SQL over the published schema ([`Session::query`]).  The token
    /// account is one of those now: it reads the `llm_response` payload,
    /// which is the shell's vocabulary and not the kernel's.
    ///
    /// `&mut self` because the signature belongs to the vocabulary rather
    /// than to today's members of it — a fold the kernel names again may
    /// keep a cache, and a caller should not have to be recompiled when one
    /// does.
    pub async fn view(
        &mut self,
        name: &str,
        opts: Option<&Map<String, Value>>,
    ) -> KnlResult<Value> {
        match name {
            VIEW_TAIL => {
                let n = tail_count(opts)?;
                let events = self.store.read(0, usize::MAX).await?;
                Ok(projection::tail_of(&events, n))
            }
            other => Err(KnlError::Validation(format!("unknown view {other:?}"))),
        }
    }

    /// Read the log with SQL.
    ///
    /// The other half of [`Session::view`], and the reason that list of names
    /// can stay short: a fold whose shape is the caller's — beats grouped,
    /// tool calls paired against their results, a ledger — is a `SELECT`
    /// against the table the log lives in, not a name the kernel had to be
    /// taught.  What the kernel keeps is the boundary around it
    /// ([`super::query`]): one statement, and it reads; a connection that
    /// cannot write; values bound rather than pasted; a deadline; a row cap.
    ///
    /// Two names are reserved.  `$stream` is this session's own stream, and
    /// `$sessions` is the set in `opts.sessions` — the session's own stream
    /// when that is omitted — expanded to one bound placeholder per id, so
    /// reading across a tree of sessions is one statement rather than a loop
    /// of them.  The kernel does not judge the set: which streams a caller
    /// may read is a question about who the caller is, and that lives above
    /// the kernel.
    ///
    /// Reads keep working after this handle closed, like every other read
    /// here: the record outlives the session.
    pub async fn query(
        &self,
        sql: &str,
        params: QueryParams,
        opts: &QueryOpts,
    ) -> KnlResult<QueryRows> {
        let plan = query::plan(sql, params, opts, &self.id)?;
        self.store.query(&plan).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::{kind_of, FIELD_BEAT, FIELD_DATA};
    // The `Vec`-backed test store: the SPI, the seam and the folds are worth
    // exercising without a database underneath, and the failure injection
    // below is easier to build on a `Vec` than on a connection.
    use crate::knl::event_store::MemEventStore;
    use serde_json::json;

    /// Object map for an event literal.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
    }

    /// A grant of `amount`, tagged the way the shell tags one.
    fn grant(amount: i64) -> BudgetGrant {
        BudgetGrant {
            amount,
            tag: Some("tokens".to_string()),
            desc: None,
        }
    }

    /// A session owned by the reserved anonymous principal.
    ///
    /// With a budget it opens with two events, not one: `session_opened` and
    /// the `budget_granted` that records what the owner allowed.
    ///
    /// The [`IsleDrivers`] it opens against is thrown away on the spot, and
    /// that is safe here: the connection thread lives while *any* handle on it
    /// does, and the store keeps one.  What the discarded driver costs is the
    /// join at the end — which a test process does not need and a host does.
    async fn new_session(budget: Option<i64>) -> Session {
        Session::new(ANON.to_string(), budget.map(grant), &IsleDrivers::new())
            .await
            .expect("open")
    }

    /// The balance the log implies, for checking the counter against it.
    async fn folded(s: &Session) -> Option<i64> {
        fold_balance(&s.events(0).await.expect("events"))
    }

    /// A raw backend read, as the folds take it.
    ///
    /// The tests that verify a durable stream reopen the backend directly —
    /// outside the seam, on purpose, to see what was really written — so
    /// they say where their `Current`s come from.
    fn as_current(events: Vec<Value>) -> Vec<Current> {
        events.into_iter().map(Current::assume_current).collect()
    }

    /// The kinds of `events`, in order.
    fn kinds(events: &[Current]) -> Vec<&str> {
        events.iter().map(Current::kind).collect()
    }

    /// The balance, as a test that is not about failure reads it.
    ///
    /// [`Session::remaining`] is fallible because a store that cannot be read
    /// has no balance to report; the stores these tests drive do not fail, so
    /// an error here is a broken fixture rather than an outcome to assert on.
    /// The tests that *are* about a failing store call the method directly.
    async fn remaining(session: &Session) -> Option<i64> {
        session.remaining().await.expect("the balance was readable")
    }

    /// [`Session::exhausted`], read the same way and for the same reason.
    async fn exhausted(session: &Session) -> bool {
        session.exhausted().await.expect("the balance was readable")
    }

    /// The `budget_*` events of a session, in seq order.
    async fn ledger(s: &Session) -> Vec<Current> {
        s.events(0)
            .await
            .expect("events")
            .into_iter()
            .filter(|e| e.kind().starts_with("budget_"))
            .collect()
    }

    /// An `llm_response` event charging `tokens`.
    ///
    /// A kind of the shell's, so its shape is the shell's too: the kernel
    /// takes the envelope and keeps whatever is under `data` verbatim.
    fn response(tokens: i64) -> Map<String, Value> {
        obj(json!({
            "kind": "llm_response",
            "data": {
                "content": [{ "type": "text", "text": "ok" }],
                "usage": { "input_tokens": tokens },
                "stop_reason": "end_turn"
            }
        }))
    }

    /// A `data` field of a recorded event, for the assertions below.
    fn field<'a>(event: &'a Current, name: &str) -> &'a Value {
        data_field(event, name).unwrap_or_else(|| panic!("data.{name} is missing: {event}"))
    }

    #[tokio::test]
    async fn a_new_session_already_carries_session_opened() {
        let s = new_session(None).await;
        assert_eq!(s.len().await.expect("len"), 1);
        let events = s.events(0).await.expect("events");
        assert_eq!(events[0].kind(), KIND_SESSION_OPENED);
        assert_eq!(events[0].seq(), 1);
        assert!(!s.is_closed());
        assert!(!s.id().is_empty());
    }

    /// The scope is issued when the session opens, and it is not the
    /// session: two ids, both real, and neither taken from a caller.  Two
    /// sessions are two scopes.
    #[tokio::test]
    async fn open_issues_a_scope_id_distinct_from_the_session_id() {
        let a = new_session(None).await;
        let b = new_session(None).await;

        assert!(!a.scope_id().is_empty(), "a session opens under a scope");
        assert!(!a.id().is_empty());
        assert_ne!(
            a.scope_id(),
            a.id(),
            "the scope id names the authority, the session id names the stream"
        );
        assert_eq!(a.scope().id(), a.scope_id(), "the delegate reads the scope");
        assert_eq!(a.scope().owner(), a.owner(), "and so does the owner");

        assert_ne!(a.scope_id(), b.scope_id(), "two sessions, two scopes");
        assert_ne!(a.id(), b.id());
    }

    /// The scope is in the log, not only in the value: it rides on the
    /// session's opening and on every entry of the ledger, so a reader can
    /// tell whose authority each move of the balance was made under.
    #[tokio::test]
    async fn session_opened_and_every_budget_event_carry_the_scope_id() {
        let mut s = new_session(Some(100)).await;
        assert_eq!(s.reserve(30).await, Ok(true));
        s.spend(10).await.expect("spend");
        assert_eq!(s.reserve(10_000).await, Ok(false));

        let scope_id = s.scope_id().to_string();
        let events = s.events(0).await.expect("events");

        let opened = &events[0];
        assert_eq!(opened.kind(), KIND_SESSION_OPENED);
        assert_eq!(
            field(opened, FIELD_SCOPE_ID).as_str(),
            Some(scope_id.as_str()),
            "the scope rides on session_opened: {opened}"
        );
        assert_eq!(
            field(opened, FIELD_OWNER).as_str(),
            Some(ANON),
            "beside the owner: {opened}"
        );

        let moves = ledger(&s).await;
        assert_eq!(
            kinds(&moves),
            vec![
                KIND_BUDGET_GRANTED,
                KIND_BUDGET_RESERVED,
                KIND_BUDGET_SPENT,
                KIND_BUDGET_REFUSED,
            ],
            "every kind of move is exercised"
        );
        for event in &moves {
            assert_eq!(
                field(event, FIELD_SCOPE_ID).as_str(),
                Some(scope_id.as_str()),
                "a ledger entry must name the scope it was allowed under: {event}"
            );
        }

        // An event a caller appends carries no scope id: the field is the
        // kernel's, on the kinds only the kernel writes.
        s.append(obj(json!({ "kind": "note" })))
            .await
            .expect("append");
        let note = s.events(0).await.expect("events").pop().expect("note");
        assert_eq!(data_field(&note, FIELD_SCOPE_ID), None, "{note}");
        assert_eq!(note[FIELD_DATA], json!({}), "and no data of its own");
    }

    #[tokio::test]
    async fn the_owner_is_total_and_read_back_verbatim() {
        assert_eq!(new_session(None).await.owner(), ANON);
        assert_eq!(
            Session::new(SYSTEM.to_string(), None, &IsleDrivers::new())
                .await
                .expect("open")
                .owner(),
            SYSTEM
        );
        assert_eq!(
            Session::new("user-42".to_string(), None, &IsleDrivers::new())
                .await
                .expect("open")
                .owner(),
            "user-42"
        );
    }

    #[tokio::test]
    async fn close_records_session_closed_once_with_the_given_reason() {
        let mut s = new_session(None).await;
        s.close(Some("budget_exhausted")).await.expect("close");
        s.close(Some("ignored")).await.expect("close (idempotent)");
        assert_eq!(s.len().await.expect("len"), 2, "close must be idempotent");

        let last = s
            .events(2)
            .await
            .expect("events")
            .pop()
            .expect("session_closed");
        assert_eq!(last.kind(), KIND_SESSION_CLOSED);
        assert_eq!(*field(&last, FIELD_REASON), json!("budget_exhausted"));
        assert!(s.is_closed());
    }

    #[tokio::test]
    async fn close_without_a_reason_records_the_default() {
        let mut s = new_session(None).await;
        s.close(None).await.expect("close");
        let last = s
            .events(2)
            .await
            .expect("events")
            .pop()
            .expect("session_closed");
        assert_eq!(*field(&last, FIELD_REASON), json!(DEFAULT_CLOSE_REASON));
    }

    /// The detached close is the same boundary, written without waiting: the
    /// backstop's path, exercised here on the store that can take it.
    #[tokio::test]
    async fn a_detached_close_records_the_same_boundary() {
        let mut s = new_session(None).await;
        s.close_detached(CLOSE_REASON_DROPPED);
        assert!(s.is_closed(), "the handle is closed straight away");
        // …and closing again writes nothing, exactly as the awaited path.
        s.close_detached(CLOSE_REASON_DROPPED);
        s.close(Some("ignored")).await.expect("close is a no-op");

        // Nothing was awaited above, so the read below is what waits: the
        // connection runs one job at a time in the order it took them, so a
        // read submitted after the detached write is answered after it.
        let last = s
            .events(0)
            .await
            .expect("events")
            .pop()
            .expect("session_closed");
        assert_eq!(last.kind(), KIND_SESSION_CLOSED);
        assert_eq!(
            *field(&last, FIELD_REASON),
            json!(CLOSE_REASON_DROPPED),
            "exactly one boundary, carrying the backstop's reason"
        );
        assert_eq!(s.len().await.expect("len"), 2, "session_opened + closed");
    }

    #[tokio::test]
    async fn a_closed_session_rejects_writes_but_keeps_serving_reads() {
        let mut s = new_session(Some(10)).await;
        s.append(obj(json!({ "kind": "note" })))
            .await
            .expect("append");
        s.spend(4).await.expect("spend");
        s.close(None).await.expect("close");

        let err = s
            .append(obj(json!({ "kind": "note" })))
            .await
            .expect_err("append after close");
        assert_eq!(err.reason(), "session is closed");
        let err = s.spend(1).await.expect_err("spend after close");
        assert_eq!(err.reason(), "session is closed");
        let err = s.reserve(1).await.expect_err("reserve after close");
        assert_eq!(err.reason(), "session is closed");

        assert_eq!(
            s.len().await.expect("len"),
            5,
            "session_opened + budget_granted + note + budget_spent + session_closed"
        );
        assert_eq!(remaining(&s).await, Some(6));
        assert_eq!(
            folded(&s).await,
            remaining(&s).await,
            "the ledger is the balance"
        );
        assert!(!exhausted(&s).await);
        assert_eq!(s.events(0).await.expect("events")[2].kind(), "note");
    }

    #[tokio::test]
    async fn two_sessions_share_nothing() {
        let mut a = new_session(Some(100)).await;
        let mut b = new_session(Some(100)).await;
        assert_ne!(a.id(), b.id());

        a.append(obj(json!({ "kind": "only_in_a" })))
            .await
            .expect("append");
        a.spend(60).await.expect("spend");

        assert_eq!(
            a.len().await.expect("len"),
            4,
            "session_opened + budget_granted + only_in_a + budget_spent"
        );
        assert_eq!(
            b.len().await.expect("len"),
            2,
            "session_opened + budget_granted"
        );
        assert_eq!(remaining(&a).await, Some(40));
        assert_eq!(remaining(&b).await, Some(100));
        // The ledgers are as separate as the histories.
        assert_eq!(folded(&a).await, Some(40));
        assert_eq!(folded(&b).await, Some(100));

        a.close(None).await.expect("close");
        assert!(b.append(obj(json!({ "kind": "still_open" }))).await.is_ok());
    }

    #[tokio::test]
    async fn view_serves_the_one_named_projection_and_rejects_anything_else() {
        let mut s = new_session(None).await;
        s.append(obj(
            json!({ "kind": "msg_user", "data": { "content": "hi" } }),
        ))
        .await
        .expect("append");
        s.append(response(9)).await.expect("recorded");

        let tail = s
            .view(VIEW_TAIL, Some(&obj(json!({ "n": 1 }))))
            .await
            .expect("tail");
        assert_eq!(tail.as_array().map(Vec::len), Some(1));

        let err = s.view("nope", None).await.expect_err("unknown view");
        assert_eq!(err.reason(), r#"unknown view "nope""#);
    }

    /// The token account is not a name the kernel answers to any more: it
    /// reads the `llm_response` payload, so it is a query view written over
    /// the published schema.  Asking the kernel for it is the same error as
    /// asking for any other name it does not have.
    #[tokio::test]
    async fn the_token_account_is_not_a_named_view() {
        let mut s = new_session(None).await;
        s.append(response(9)).await.expect("recorded");

        let err = s.view("usage", None).await.expect_err("usage was served");
        assert_eq!(err.reason(), r#"unknown view "usage""#);
        assert_eq!(err.kind(), KnlError::VALIDATION);

        // What it needs is in the log, verbatim, for a reader to sum.
        let recorded = s
            .events(2)
            .await
            .expect("events")
            .pop()
            .expect("llm_response");
        assert_eq!(*field(&recorded, "usage"), json!({ "input_tokens": 9 }));
    }

    /// The conversation is not one of the names: how a record becomes a
    /// request — which role each kind takes, whether a system message
    /// belongs in it, where to cut it off — is the shell's decision, and
    /// it builds it from `events` rather than asking the kernel for it.
    #[tokio::test]
    async fn the_conversation_is_not_a_named_view() {
        let mut s = new_session(None).await;
        s.append(obj(
            json!({ "kind": "msg_user", "data": { "content": "hi" } }),
        ))
        .await
        .expect("append");

        let err = s
            .view("dialogue", None)
            .await
            .expect_err("dialogue was served");
        assert_eq!(err.reason(), r#"unknown view "dialogue""#);

        let events = s.events(0).await.expect("events");
        assert_eq!(events[1].kind(), "msg_user");
        assert_eq!(*field(&events[1], "content"), json!("hi"));
    }

    /// Appending an `llm_response` records it — verbatim, beat included —
    /// and leaves the budget alone.  What a call was allowed to cost was
    /// decided before it happened; the record of it happening is not a
    /// second place where that is decided.
    #[tokio::test]
    async fn appending_an_llm_response_records_it_verbatim_without_charging() {
        let mut s = new_session(Some(100)).await;
        let seq = s
            .append(obj(json!({
                "kind": "llm_response",
                // The beat is the caller's word and the kernel keeps it.
                "beat": "beat-7",
                "data": {
                    "content": [{ "type": "text", "text": "hi" }],
                    "usage": { "input_tokens": 20, "output_tokens": 10 }
                }
            })))
            .await
            .expect("append");

        let recorded = s
            .events(seq)
            .await
            .expect("events")
            .pop()
            .expect("llm_response");
        assert_eq!(recorded.kind(), "llm_response");
        assert_eq!(
            recorded[FIELD_BEAT],
            json!("beat-7"),
            "the declared beat is recorded as given"
        );

        assert_eq!(remaining(&s).await, Some(100), "an append must not charge");
        assert_eq!(
            ledger(&s).await.len(),
            1,
            "only the opening grant is in the ledger"
        );
        assert_eq!(
            *field(&recorded, "usage"),
            json!({ "input_tokens": 20, "output_tokens": 10 }),
            "the counts are stored as they came: {recorded}"
        );
        assert_eq!(
            folded(&s).await,
            Some(100),
            "what was consumed and the balance are separate readings"
        );
    }

    /// The grant is the first thing the ledger says, right after the
    /// session's own boundary, with the owner's words on it.
    #[tokio::test]
    async fn opening_with_a_grant_records_it() {
        let s = Session::new(
            ANON.to_string(),
            Some(BudgetGrant {
                amount: 500,
                tag: Some("tokens".to_string()),
                desc: Some("one nightly run".to_string()),
            }),
            &IsleDrivers::new(),
        )
        .await
        .expect("open");

        let events = s.events(0).await.expect("events");
        assert_eq!(events.len(), 2, "session_opened + budget_granted");
        assert_eq!(events[0].kind(), KIND_SESSION_OPENED);
        assert_eq!(
            data_field(&events[0], "budget"),
            None,
            "the grant is its own event, not a field on session_opened"
        );

        let granted = &events[1];
        assert_eq!(granted.kind(), KIND_BUDGET_GRANTED);
        assert_eq!(*field(granted, FIELD_AMOUNT), json!(500));
        assert_eq!(*field(granted, FIELD_TAG), json!("tokens"));
        assert_eq!(*field(granted, FIELD_DESC), json!("one nightly run"));
        assert_eq!(remaining(&s).await, Some(500));
        assert_eq!(folded(&s).await, remaining(&s).await);

        // A session with no grant keeps no ledger at all.
        let bare = new_session(None).await;
        assert_eq!(bare.len().await.expect("len"), 1, "session_opened only");
        assert!(ledger(&bare).await.is_empty());
        assert_eq!(folded(&bare).await, None);
    }

    /// A reservation the balance covers is recorded once, deducts exactly
    /// what it asked for, and carries the grant's tag.
    #[tokio::test]
    async fn a_granted_reservation_is_one_event_and_one_deduction() {
        let mut s = new_session(Some(100)).await;
        assert_eq!(s.reserve(30).await, Ok(true));

        let moves = ledger(&s).await;
        assert_eq!(moves.len(), 2, "the grant and the reservation");
        assert_eq!(moves[1].kind(), KIND_BUDGET_RESERVED);
        assert_eq!(*field(&moves[1], FIELD_AMOUNT), json!(30));
        assert_eq!(*field(&moves[1], FIELD_TAG), json!("tokens"));
        assert_eq!(remaining(&s).await, Some(70));
        assert_eq!(
            folded(&s).await,
            remaining(&s).await,
            "the ledger is the balance"
        );
    }

    /// A refusal is a fact: it is recorded, with what was asked for and
    /// what there was, and it moves nothing.
    #[tokio::test]
    async fn a_refused_reservation_is_recorded_and_changes_no_balance() {
        let mut s = new_session(Some(10)).await;
        assert_eq!(s.reserve(11).await, Ok(false));

        let moves = ledger(&s).await;
        assert_eq!(moves.len(), 2, "the grant and the refusal");
        assert_eq!(moves[1].kind(), KIND_BUDGET_REFUSED);
        assert_eq!(*field(&moves[1], FIELD_AMOUNT), json!(11));
        assert_eq!(
            *field(&moves[1], FIELD_REMAINING),
            json!(10),
            "what there was"
        );
        assert_eq!(*field(&moves[1], FIELD_TAG), json!("tokens"));
        assert_eq!(remaining(&s).await, Some(10), "a refusal must not deduct");
        assert!(!exhausted(&s).await);
        assert_eq!(folded(&s).await, remaining(&s).await);

        // And the run can still spend what it has: nothing was consumed.
        assert_eq!(s.reserve(10).await, Ok(true));
        assert_eq!(remaining(&s).await, Some(0));
        assert_eq!(folded(&s).await, Some(0));
    }

    /// The settlement is recorded like everything else, and what the session
    /// reports is the fold of the ledger after any sequence of moves.
    #[tokio::test]
    async fn the_balance_is_the_fold_after_any_sequence_of_moves() {
        let mut s = new_session(Some(1000)).await;
        assert_eq!(s.reserve(200).await, Ok(true));
        s.append(response(40)).await.expect("recorded");
        s.spend(50).await.expect("spend");
        assert_eq!(s.reserve(10_000).await, Ok(false));
        assert_eq!(s.reserve(300).await, Ok(true));
        s.spend(0).await.expect("spend");

        let moves = ledger(&s).await;
        assert_eq!(
            kinds(&moves),
            vec![
                KIND_BUDGET_GRANTED,
                KIND_BUDGET_RESERVED,
                KIND_BUDGET_SPENT,
                KIND_BUDGET_REFUSED,
                KIND_BUDGET_RESERVED,
                KIND_BUDGET_SPENT,
            ],
            "every move left exactly one event"
        );
        assert_eq!(remaining(&s).await, Some(450), "1000 - 200 - 50 - 300");
        assert_eq!(folded(&s).await, remaining(&s).await);
    }

    /// The ledger is the kernel's to write: a caller cannot grant itself a
    /// budget, or drain one, by appending the events the balance folds
    /// from.
    #[tokio::test]
    async fn a_caller_cannot_append_the_budget_kinds() {
        let mut s = new_session(Some(10)).await;
        for event in [
            json!({ "kind": "budget_granted", "data": { "amount": 1_000_000 } }),
            json!({ "kind": "budget_reserved", "data": { "amount": 5 } }),
            json!({ "kind": "budget_refused", "data": { "amount": 5, "remaining": 0 } }),
            json!({ "kind": "budget_spent", "data": { "amount": 5 } }),
        ] {
            let err = s
                .append(obj(event.clone()))
                .await
                .expect_err("kernel-only kind");
            assert!(
                err.reason().contains("kernel only"),
                "{event}: {}",
                err.reason()
            );
        }

        assert_eq!(
            remaining(&s).await,
            Some(10),
            "no forged event moved the balance"
        );
        assert_eq!(ledger(&s).await.len(), 1, "nothing was recorded");
        assert_eq!(folded(&s).await, remaining(&s).await);
    }

    /// The session's boundaries are the kernel's alone: a caller cannot
    /// hand-append either one, so a stream cannot claim an opening it never
    /// had or an ending it never reached.  The refusal leaves the session
    /// open and the log untouched.
    #[tokio::test]
    async fn a_caller_cannot_append_the_session_boundary_kinds() {
        let mut s = new_session(Some(100)).await;
        for event in [
            json!({ "kind": "session_opened", "data": { "scope_id": "s", "owner": "me" } }),
            json!({ "kind": "session_closed", "data": { "reason": "carried over" } }),
        ] {
            let err = s
                .append(obj(event.clone()))
                .await
                .expect_err("kernel-only kind");
            assert!(
                err.reason().contains("kernel only"),
                "{event}: {}",
                err.reason()
            );
        }

        assert!(!s.is_closed(), "a refused append ended the session");
        assert_eq!(s.len().await.expect("len"), 2, "nothing was recorded");
        assert_eq!(s.append(obj(json!({ "kind": "note" }))).await, Ok(3));
        assert_eq!(s.spend(10).await, Ok(()));
        assert_eq!(remaining(&s).await, Some(90), "the settlement landed");
    }

    /// Only `close` records `session_closed`, and it records exactly one:
    /// the flag and the event move together, so the log and the state
    /// cannot disagree.
    #[tokio::test]
    async fn only_close_records_session_closed() {
        let mut s = new_session(Some(100)).await;
        s.append(obj(json!({ "kind": "note" })))
            .await
            .expect("append");
        assert!(
            !s.events(0)
                .await
                .expect("events")
                .iter()
                .any(|e| e.kind() == KIND_SESSION_CLOSED),
            "nothing but close writes the boundary"
        );

        s.close(Some("done")).await.expect("close");
        assert!(s.is_closed());

        let closed: Vec<Current> = s
            .events(0)
            .await
            .expect("events")
            .into_iter()
            .filter(|e| e.kind() == KIND_SESSION_CLOSED)
            .collect();
        assert_eq!(closed.len(), 1, "exactly one boundary: {closed:?}");
        assert_eq!(*field(&closed[0], FIELD_REASON), json!("done"));
        assert_eq!(
            s.append(obj(json!({ "kind": "note" })))
                .await
                .expect_err("append after close")
                .reason(),
            "session is closed"
        );
    }

    /// The record and the account are separate readings of the same
    /// session: the response is in the history, counts and all, and the
    /// balance is exactly what was granted, because nobody reserved
    /// anything.
    #[tokio::test]
    async fn a_recorded_response_is_in_the_history_without_being_charged() {
        let mut s = new_session(Some(100)).await;
        s.append(response(30)).await.expect("recorded");

        assert_eq!(remaining(&s).await, Some(100));
        assert!(!exhausted(&s).await);

        let recorded = s
            .events(3)
            .await
            .expect("events")
            .pop()
            .expect("llm_response");
        assert_eq!(recorded.kind(), "llm_response");
        assert_eq!(*field(&recorded, "stop_reason"), json!("end_turn"));
        assert_eq!(field(&recorded, "usage")["input_tokens"], json!(30));
        assert_eq!(
            folded(&s).await,
            Some(100),
            "the ledger recorded no consumption"
        );
    }

    /// The beat belongs to the layer above: the kernel never mints one, so
    /// an event that declares none carries none, and one that declares a
    /// beat carries exactly the string it was given — on any kind, and
    /// repeated across the facts of one beat without the kernel objecting.
    #[tokio::test]
    async fn beats_are_the_callers_word_and_the_kernel_adds_none() {
        let mut s = new_session(None).await;

        let seq = s.append(response(1)).await.expect("an undeclared beat");
        let bare = s
            .events(seq)
            .await
            .expect("events")
            .pop()
            .expect("response");
        assert_eq!(
            bare.get(FIELD_BEAT),
            None,
            "the kernel must not invent a beat: {bare}"
        );

        for event in [
            json!({
                "kind": "llm_response", "beat": "b-1",
                "data": { "content": [], "usage": { "input_tokens": 1 } }
            }),
            json!({
                "kind": "tool_call", "beat": "b-1",
                "data": { "call_id": "c1", "name": "sh", "args": {} }
            }),
            json!({
                "kind": "tool_result", "beat": "b-1",
                "data": { "call_id": "c1", "ok": true, "result": "ok" }
            }),
            json!({ "kind": "llm_call_failed", "beat": "b-1", "data": { "error": "boom" } }),
        ] {
            let seq = s.append(obj(event.clone())).await.expect("declared beat");
            let recorded = s
                .events(seq)
                .await
                .expect("events")
                .pop()
                .expect("recorded");
            assert_eq!(recorded[FIELD_BEAT], json!("b-1"), "{event}");
        }

        // A non-string beat is the one thing refused, on any kind.
        let err = s
            .append(obj(json!({ "kind": "note", "beat": 1 })))
            .await
            .expect_err("a numbered beat");
        assert!(err.reason().contains("beat must be a string"), "{err}");
    }

    #[tokio::test]
    async fn a_closed_session_records_nothing() {
        let mut s = new_session(Some(100)).await;
        s.close(None).await.expect("close");

        let err = s.append(response(10)).await.expect_err("closed session");
        assert_eq!(err.reason(), "session is closed");

        assert_eq!(
            s.len().await.expect("len"),
            3,
            "session_opened + budget_granted + session_closed only"
        );
        assert_eq!(remaining(&s).await, Some(100), "nothing was consumed");
    }

    /// The budget stops a session *before* it spends, not after: a
    /// reservation the balance cannot cover is refused, and the call it was
    /// for never happens.  This replaces the old contract, where the budget
    /// was a flag that only stood up once a recorded call had already used
    /// the allowance up — by which time the spending was done.
    #[tokio::test]
    async fn the_budget_refuses_before_the_call_rather_than_flagging_after_it() {
        let mut s = new_session(Some(10)).await;

        /// How many model responses the log holds.
        async fn responses(s: &Session) -> usize {
            s.events(0)
                .await
                .expect("events")
                .iter()
                .filter(|e| e.kind() == "llm_response")
                .count()
        }

        // The estimate fits, so the beat proceeds and records its response.
        assert_eq!(s.reserve(10).await, Ok(true));
        s.append(response(25)).await.expect("recorded");
        assert_eq!(remaining(&s).await, Some(0), "the reservation took it all");
        assert!(exhausted(&s).await);

        // The next beat asks first and is turned away, so no second
        // response is recorded: the caller never made the call.
        assert_eq!(s.reserve(1).await, Ok(false));
        assert_eq!(responses(&s).await, 1, "the refused beat made no call");
        assert_eq!(remaining(&s).await, Some(0));
        assert_eq!(folded(&s).await, remaining(&s).await);

        // The kernel still does not police it: a caller that ignores the
        // refusal can append anyway, and the history says that it did.
        s.append(response(5)).await.expect("recorded");
        assert_eq!(responses(&s).await, 2, "stopping is the caller's decision");
    }

    #[tokio::test]
    async fn without_a_budget_a_call_reports_no_remaining_and_is_never_exhausted() {
        let mut s = new_session(None).await;
        s.append(response(9_000)).await.expect("recorded");
        assert_eq!(remaining(&s).await, None);
        assert!(!exhausted(&s).await);

        // No budget, no ledger: reserve always grants, spend does nothing,
        // and neither leaves a trace.
        assert_eq!(s.reserve(1_000_000).await, Ok(true));
        assert_eq!(s.spend(1_000_000).await, Ok(()));
        assert!(
            ledger(&s).await.is_empty(),
            "a run with no quota keeps no ledger"
        );
        assert_eq!(remaining(&s).await, None);
        assert!(!exhausted(&s).await);
    }

    #[tokio::test]
    async fn views_stay_readable_and_correct_after_close() {
        let mut s = new_session(None).await;
        s.append(response(9)).await.expect("recorded");
        let before = s
            .view(VIEW_TAIL, Some(&obj(json!({ "n": 1 }))))
            .await
            .expect("tail");
        s.close(None).await.expect("close");
        let after = s
            .view(VIEW_TAIL, Some(&obj(json!({ "n": 1 }))))
            .await
            .expect("tail after close");

        // The read keeps working, and it reads the log as it now stands: the
        // ending this handle wrote is the last thing in it.
        assert_eq!(
            kind_of(&before.as_array().expect("array")[0]),
            "llm_response"
        );
        assert_eq!(
            kind_of(&after.as_array().expect("array")[0]),
            KIND_SESSION_CLOSED
        );

        assert_eq!(s.len().await.expect("len"), 3);
        assert_eq!(
            s.events(0).await.expect("events")[2].kind(),
            KIND_SESSION_CLOSED
        );
    }

    /// `open_on` on a durable backend records the session's owner on the
    /// `session_opened` boundary, so resume can recover it from the log
    /// alone.
    #[tokio::test]
    async fn open_on_records_the_owner_on_session_opened() {
        use crate::knl::SqliteEventStore;

        let store = SqliteEventStore::open_memory("owner-stream", &IsleDrivers::new())
            .await
            .expect("open");
        let s = Session::open_on("user-7".to_string(), Some(grant(100)), Box::new(store))
            .await
            .expect("open");

        let events = s.events(0).await.expect("events");
        let opened = events.first().expect("session_opened");
        assert_eq!(opened.kind(), KIND_SESSION_OPENED);
        assert_eq!(
            field(opened, FIELD_OWNER).as_str(),
            Some("user-7"),
            "owner rides on session_opened: {opened}"
        );
        assert_eq!(s.owner(), "user-7");

        // The grant is durable too, as its own event.
        assert_eq!(events[1].kind(), KIND_BUDGET_GRANTED);
        assert_eq!(*field(&events[1], FIELD_AMOUNT), json!(100));
    }

    /// Resume re-folds a persisted SQLite stream: the owner and the
    /// *balance* come back from the log, because every move of the balance
    /// is in it.
    #[tokio::test]
    async fn resume_restores_the_owner_and_the_folded_balance() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "resume-stream";
        let drivers = IsleDrivers::new();

        // A durable session: two beats that reserved and settled.
        let before_close = {
            let store = SqliteEventStore::open(&path, stream, &drivers)
                .await
                .expect("open");
            let mut s = Session::open_on("user-42".to_string(), Some(grant(100)), Box::new(store))
                .await
                .expect("open");
            assert_eq!(s.reserve(30).await, Ok(true));
            s.append(response(30)).await.expect("first response");
            s.append(obj(
                json!({ "kind": "msg_user", "data": { "content": "more" } }),
            ))
            .await
            .expect("msg_user");
            assert_eq!(s.reserve(15).await, Ok(true));
            s.append(response(20)).await.expect("second response");
            s.spend(5)
                .await
                .expect("the second call overran its estimate");
            assert_eq!(remaining(&s).await, Some(50), "100 - 30 - 15 - 5");
            assert_eq!(folded(&s).await, remaining(&s).await);
            remaining(&s).await
        }; // dropped: the handle goes, the log persists.

        // Reopen the same stream and resume — no new session_opened is
        // written, and no new grant either.
        let store = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen");
        let mut resumed = Session::resume(None, Box::new(store))
            .await
            .expect("resume");

        assert_eq!(
            resumed.owner(),
            "user-42",
            "owner restored from session_opened"
        );
        assert_eq!(
            remaining(&resumed).await,
            before_close,
            "the balance is what the ledger says it was"
        );
        assert_eq!(
            resumed.grant().and_then(|g| g.tag.as_deref()),
            Some("tokens"),
            "the grant's words came back with it"
        );
        // Resume appended nothing: the log is exactly what was persisted.
        assert_eq!(
            resumed.len().await.expect("len"),
            8,
            "session_opened + granted + reserved + response + msg_user \
             + reserved + response + spent — and nothing from resume itself"
        );

        // The record came back whole, so a reader that sums the counts —
        // the Lua query view, over the published schema — has both calls to
        // work from.
        let responses: Vec<Current> = resumed
            .events(0)
            .await
            .expect("events")
            .into_iter()
            .filter(|e| e.kind() == "llm_response")
            .collect();
        assert_eq!(responses.len(), 2, "{responses:?}");
        assert_eq!(field(&responses[0], "usage")["input_tokens"], json!(30));
        assert_eq!(field(&responses[1], "usage")["input_tokens"], json!(20));

        // The ledger continues: the next reservation comes off the restored
        // balance, and what the resumed session records is its own.
        assert_eq!(resumed.reserve(5).await, Ok(true));
        let seq = resumed.append(response(5)).await.expect("third response");
        let recorded = resumed
            .events(seq)
            .await
            .expect("events")
            .pop()
            .expect("llm_response");
        assert_eq!(recorded.kind(), "llm_response");
        assert_eq!(remaining(&resumed).await, Some(45), "5 reserved off the 50");
        assert_eq!(folded(&resumed).await, remaining(&resumed).await);
    }

    /// A `grant` on resume is the owner allowing *more*: it is recorded and
    /// added to what the log left, rather than replacing it.
    #[tokio::test]
    async fn resume_with_a_grant_records_it_and_raises_the_balance() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "regrant-stream";
        let drivers = IsleDrivers::new();

        {
            let store = SqliteEventStore::open(&path, stream, &drivers)
                .await
                .expect("open");
            let mut s = Session::open_on("user-9".to_string(), Some(grant(100)), Box::new(store))
                .await
                .expect("open");
            assert_eq!(s.reserve(80).await, Ok(true));
            assert_eq!(remaining(&s).await, Some(20));
        }

        let store = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen");
        let mut resumed = Session::resume(
            Some(BudgetGrant {
                amount: 50,
                tag: Some("tokens".to_string()),
                desc: Some("a little more".to_string()),
            }),
            Box::new(store),
        )
        .await
        .expect("resume");

        assert_eq!(remaining(&resumed).await, Some(70), "20 left + 50 granted");
        assert_eq!(folded(&resumed).await, remaining(&resumed).await);

        let moves = ledger(&resumed).await;
        assert_eq!(
            kinds(&moves),
            vec![
                KIND_BUDGET_GRANTED,
                KIND_BUDGET_RESERVED,
                KIND_BUDGET_GRANTED
            ],
            "the second grant is a recorded fact"
        );
        assert_eq!(*field(&moves[2], FIELD_AMOUNT), json!(50));
        assert_eq!(*field(&moves[2], FIELD_DESC), json!("a little more"));

        // And the resumed run spends against the raised balance.
        assert_eq!(resumed.reserve(70).await, Ok(true));
        assert_eq!(resumed.reserve(1).await, Ok(false));
        assert_eq!(remaining(&resumed).await, Some(0));
        assert_eq!(folded(&resumed).await, remaining(&resumed).await);
    }

    /// One session, one budget: a resume raises a ledger that exists and
    /// cannot start one that does not.
    ///
    /// The two-handle case this rules out: a session opens with no quota, so
    /// its handle refuses nothing and its caller was told there is nothing to
    /// refuse; a second handle resumes the same open stream with a grant, and
    /// from the next reservation on the first handle is bounded by an
    /// allowance nobody asked it about.  Whether a session has a budget is
    /// settled when it opens.
    #[tokio::test]
    async fn a_resume_does_not_give_a_stream_the_budget_it_opened_without() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "ungranted-stream";
        let drivers = IsleDrivers::new();

        // Opened with no budget, and still open: the handle is held for the
        // whole test, so nothing has written an ending.
        let store = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open");
        let first = Session::open_on("user-1".to_string(), None, Box::new(store))
            .await
            .expect("open");
        assert_eq!(
            first.len().await.expect("len"),
            1,
            "session_opened, and no grant beside it"
        );

        let reopened = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen");
        let err = Session::resume(Some(grant(100)), Box::new(reopened))
            .await
            .expect_err("a resume must not introduce a ledger");
        assert_eq!(
            err.kind(),
            KnlError::VALIDATION,
            "the caller's argument is what did not hold up: {err}"
        );
        assert!(
            err.reason().contains("opened with no budget"),
            "{}",
            err.reason()
        );

        // Nothing was written for it: the refusal is decided in the same
        // transaction that would have written the grant.
        assert_eq!(
            first.len().await.expect("len"),
            1,
            "the stream is untouched"
        );
        assert_eq!(remaining(&first).await, None, "and still has no ledger");

        // Resuming it *without* a grant is what a second handle does.
        let reopened = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen");
        let second = Session::resume(None, Box::new(reopened))
            .await
            .expect("a resume with no grant is the ordinary one");
        assert_eq!(remaining(&second).await, None);
        assert_eq!(second.len().await.expect("len"), 1);
    }

    /// Whether there is a budget is the *log's* answer, not the handle's.
    ///
    /// A handle opened without a grant used to short-circuit on its own
    /// cached scope: `reserve` answered `true` whatever the ledger said and
    /// `spend` wrote nothing, so a grant that reached the stream through
    /// another handle was invisible to it and the quota bounded nobody.  The
    /// question is inside the decision now, so the grant in the log binds
    /// every handle on the stream.
    #[tokio::test]
    async fn a_handle_opened_without_a_grant_is_bound_by_the_grant_the_log_carries() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "late-grant-stream";
        let drivers = IsleDrivers::new();

        let store = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open");
        let mut opened_without = Session::open_on("user-1".to_string(), None, Box::new(store))
            .await
            .expect("open");

        // The owner grants, through a handle it holds on the same stream.
        let reopened = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen");
        let mut owner_handle = Session::resume(None, Box::new(reopened))
            .await
            .expect("resume");
        owner_handle
            .grant_more(grant(100))
            .await
            .expect("the owner grants");

        // The first handle's own scope still says there is no budget…
        assert_eq!(
            opened_without.grant(),
            None,
            "the cached grant is a hint, and this handle never got one"
        );
        // …and the ledger it is measured against is the log's.
        assert_eq!(
            opened_without.reserve(500).await,
            Ok(false),
            "500 does not fit in the 100 the log carries"
        );
        assert_eq!(opened_without.reserve(40).await, Ok(true));
        assert_eq!(remaining(&opened_without).await, Some(60));
        opened_without
            .spend(60)
            .await
            .expect("a deduction on a ledger this handle did not open");
        assert_eq!(remaining(&opened_without).await, Some(0));
        assert_eq!(
            folded(&opened_without).await,
            remaining(&opened_without).await
        );

        // Every entry it wrote is tagged with the unit the *log's* grant
        // named, not with the nothing this handle was opened with.
        let moves = ledger(&opened_without).await;
        assert_eq!(
            kinds(&moves),
            vec![
                KIND_BUDGET_GRANTED,
                KIND_BUDGET_REFUSED,
                KIND_BUDGET_RESERVED,
                KIND_BUDGET_SPENT,
            ],
        );
        for event in &moves {
            assert_eq!(
                field(event, FIELD_TAG).as_str(),
                Some("tokens"),
                "the unit comes off the log's grant: {event}"
            );
        }
        assert_eq!(*field(&moves[1], FIELD_REMAINING), json!(100));
    }

    /// Seed `stream` with a `session_opened` whose `data` is empty.
    ///
    /// The validator requires the scope on that kind, so this cannot be
    /// written through the store: the row goes in behind it, which is what a
    /// stream an upcaster could not bring all the way would look like.  The
    /// resume fallbacks below are for exactly that, and this is the only way
    /// to reach them.
    async fn seed_an_opening_with_no_scope(path: &std::path::Path, stream: &str) {
        use crate::knl::SqliteEventStore;

        // Open once so the table is there, then write past the validator.
        // The collection is shut down rather than dropped, so the connection
        // has actually finished before the direct write below.
        let drivers = IsleDrivers::new();
        drop(
            SqliteEventStore::open(path, stream, &drivers)
                .await
                .expect("open"),
        );
        assert!(drivers.shutdown().await.is_empty(), "the writer joined");
        let conn = rusqlite::Connection::open(path).expect("open the database directly");
        conn.execute(
            "INSERT INTO events \
             (stream, seq, epoch_ms, kind, schema_version, beat, meta, data) \
             VALUES (?1, 1, 0, ?2, 1, NULL, '{}', '{}')",
            rusqlite::params![stream, KIND_SESSION_OPENED],
        )
        .expect("seed the opening");
    }

    /// A log whose `session_opened` carries no `owner` resumes as [`ANON`]
    /// rather than failing: a stream that arrives missing the field is still
    /// a session, and refusing it would lose the log rather than protect it.
    #[tokio::test]
    async fn resume_falls_back_to_anon_when_the_log_has_no_owner() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "legacy-stream";
        seed_an_opening_with_no_scope(&path, stream).await;

        let store = SqliteEventStore::open(&path, stream, &IsleDrivers::new())
            .await
            .expect("reopen");
        let resumed = Session::resume(None, Box::new(store))
            .await
            .expect("resume");
        assert_eq!(resumed.owner(), ANON);
        assert_eq!(
            remaining(&resumed).await,
            None,
            "resumed without a budget cap"
        );
    }

    /// Resume restores the *scope*, not just a fresh one: the id and the
    /// owner come back off `session_opened`, so the session continues under
    /// the authority the log says it opened with, and the ledger it goes on
    /// writing names that same scope.
    #[tokio::test]
    async fn resume_restores_the_scope_id_and_owner_from_the_log() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "scope-resume-stream";
        let drivers = IsleDrivers::new();

        let opened_scope = {
            let store = SqliteEventStore::open(&path, stream, &drivers)
                .await
                .expect("open");
            let mut s = Session::open_on("user-11".to_string(), Some(grant(100)), Box::new(store))
                .await
                .expect("open");
            assert_eq!(s.reserve(40).await, Ok(true));
            s.scope_id().to_string()
        };

        let store = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen");
        let mut resumed = Session::resume(None, Box::new(store))
            .await
            .expect("resume");
        assert_eq!(
            resumed.scope_id(),
            opened_scope,
            "the scope id is restored from session_opened, not re-issued"
        );
        assert_eq!(resumed.owner(), "user-11");
        assert_eq!(resumed.scope().owner(), "user-11");
        assert_eq!(
            remaining(&resumed).await,
            Some(60),
            "the balance is the fold's"
        );

        // What the resumed session records goes on naming the same scope.
        assert_eq!(resumed.reserve(10).await, Ok(true));
        let last = ledger(&resumed).await.pop().expect("budget_reserved");
        assert_eq!(last.kind(), KIND_BUDGET_RESERVED);
        assert_eq!(
            field(&last, FIELD_SCOPE_ID).as_str(),
            Some(opened_scope.as_str()),
            "{last}"
        );
    }

    /// A log whose `session_opened` carries no `scope_id` resumes under a
    /// fresh kernel-issued one rather than failing — the sibling of the
    /// `owner` fallback above, and for the same reason.
    #[tokio::test]
    async fn resume_issues_a_fresh_scope_id_when_the_log_records_none() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "legacy-scope-stream";
        seed_an_opening_with_no_scope(&path, stream).await;

        let store = SqliteEventStore::open(&path, stream, &IsleDrivers::new())
            .await
            .expect("reopen");
        // Resumed with no grant, because a resume cannot give a stream one it
        // opened without ([`Session::grant_on_resume`]); the owner grants
        // through the handle below, which is what puts a `budget_granted` in
        // the log for the scope id to be read off.
        let mut resumed = Session::resume(None, Box::new(store))
            .await
            .expect("resume");
        resumed
            .grant_more(grant(50))
            .await
            .expect("the owner grants");

        // The fallback is visible from both sides: the log says nothing…
        let opened = resumed.events(0).await.expect("events").remove(0);
        assert_eq!(opened.kind(), KIND_SESSION_OPENED);
        assert_eq!(data_field(&opened, FIELD_SCOPE_ID), None, "{opened}");
        assert_eq!(data_field(&opened, FIELD_OWNER), None, "{opened}");
        // …and the resumed session has a real scope all the same.
        assert!(
            !resumed.scope_id().is_empty(),
            "an older log must still resume under a scope"
        );
        assert_eq!(resumed.owner(), ANON, "the sibling fallback");

        // And it is the one everything written from here on names.
        let granted = ledger(&resumed).await.pop().expect("budget_granted");
        assert_eq!(granted.kind(), KIND_BUDGET_GRANTED);
        assert_eq!(
            field(&granted, FIELD_SCOPE_ID).as_str(),
            Some(resumed.scope_id()),
            "{granted}"
        );
    }

    /// (Fix 5) Resuming an empty log is a caller error — a mistyped or
    /// nonexistent stream must not fold into an anonymous zero session.
    #[tokio::test]
    async fn resume_of_an_empty_store_is_a_caller_error_not_an_anon_session() {
        let err = Session::resume(Some(grant(100)), Box::new(MemEventStore::new()))
            .await
            .expect_err("an empty store has no session to resume");
        assert!(
            err.reason().contains("no session to resume"),
            "{}",
            err.reason()
        );
    }

    /// (Fix 5) A log that has events but no opening the kernel recognises —
    /// under any shape it has ever been written in — is a caller error too:
    /// the ANON fallback is only for a real `session_opened`.
    #[tokio::test]
    async fn resume_of_a_store_without_an_opening_is_a_caller_error() {
        let mut store = MemEventStore::new();
        store
            .append(obj(json!({ "kind": "note" })))
            .await
            .expect("seed a non-opening event");
        let err = Session::resume(None, Box::new(store))
            .await
            .expect_err("a log with no opening has no session to resume");
        assert!(
            err.reason().contains("no session to resume"),
            "{}",
            err.reason()
        );
    }

    /// A session is disposable: once its ending is in the log, the stream is
    /// not continued.  What comes after an ending is a new session.
    #[tokio::test]
    async fn a_closed_stream_is_not_resumed() {
        let mut store = MemEventStore::new();
        store
            .append(obj(json!({
                "kind": "session_opened",
                "data": { "scope_id": "scope-5", "owner": "user-5" }
            })))
            .await
            .expect("seed the opening");
        store
            .append(obj(
                json!({ "kind": "budget_granted", "data": { "amount": 100 } }),
            ))
            .await
            .expect("seed the grant");
        store
            .append(obj(
                json!({ "kind": "session_closed", "data": { "reason": "done" } }),
            ))
            .await
            .expect("seed the ending");

        let err = Session::resume(None, Box::new(store))
            .await
            .expect_err("a closed session must not be resumed");
        assert!(
            err.reason().contains("session is closed"),
            "{}",
            err.reason()
        );
        assert!(err.reason().contains("disposable"), "{}", err.reason());
    }

    /// A competing writer lands an event between this session's writes, so
    /// the log has moved on at the moment this one appends.  The append still
    /// lands — a fact is not refused for what its writer had seen — and the
    /// `seq` it comes back with is where it really landed, after whatever got
    /// in first.
    struct BusyStore {
        inner: MemEventStore,
        injected: bool,
    }

    #[async_trait::async_trait]
    impl EventStore for BusyStore {
        /// A session's appends come through here, so this is where the
        /// competing writer gets in: once, just before the response this
        /// session is about to record.
        async fn append(&mut self, event: Map<String, Value>) -> KnlResult<crate::knl::Committed> {
            if !self.injected
                && event.get(FIELD_KIND).and_then(Value::as_str) == Some("llm_response")
            {
                self.injected = true;
                self.inner
                    .append(obj(json!({ "kind": "sneaked_in" })))
                    .await
                    .expect("injected concurrent write");
            }
            self.inner.append(event).await
        }

        async fn append_if(
            &mut self,
            kinds: Option<&[&str]>,
            decide: crate::knl::Decision,
        ) -> KnlResult<Option<crate::knl::Committed>> {
            self.inner.append_if(kinds, decide).await
        }

        async fn read_kinds(
            &self,
            kinds: Option<&[&str]>,
            from_seq: u64,
            limit: usize,
        ) -> KnlResult<Vec<Value>> {
            self.inner.read_kinds(kinds, from_seq, limit).await
        }

        async fn head(&self) -> KnlResult<Option<u64>> {
            self.inner.head().await
        }

        async fn len(&self) -> KnlResult<usize> {
            self.inner.len().await
        }
    }

    /// An append records a fact and the store orders it: another writer
    /// getting there first does not turn this session's append into a
    /// failure, it only decides where the two land.
    #[tokio::test]
    async fn an_append_lands_after_a_competing_write_rather_than_being_refused() {
        let store = BusyStore {
            inner: MemEventStore::new(),
            injected: false,
        };
        let mut s = Session::open_on("user".to_string(), Some(grant(1000)), Box::new(store))
            .await
            .expect("open");
        assert_eq!(
            s.len().await.expect("len"),
            2,
            "session_opened + budget_granted so far"
        );

        // The competing write lands at seq 3, so the response lands at 4 —
        // and it lands.
        let seq = s
            .append(response(10))
            .await
            .expect("an append is not refused");
        assert_eq!(seq, 4, "the seq is where the event really landed");
        assert_eq!(s.len().await.expect("len"), 4, "both writes are in the log");

        let log = s.events(0).await.expect("events");
        assert_eq!(
            kinds(&log),
            [
                KIND_SESSION_OPENED,
                KIND_BUDGET_GRANTED,
                "sneaked_in",
                "llm_response"
            ],
            "the log interleaves in arrival order"
        );
        assert_eq!(
            remaining(&s).await,
            Some(1000),
            "an append still charges nothing"
        );
    }

    /// A store that takes a decision's write and refuses a plain one, from
    /// the moment the test arms it.
    ///
    /// The two paths a `budget_*` event could reach the log by, told apart:
    /// what a decision returns goes in with the read it was decided against
    /// ([`EventStore::append_if`]), and anything else is a second write. A
    /// refusal that came out here as a plain append would fail on this store
    /// while the decision that produced it had already succeeded — which is
    /// precisely the state a caller cannot read back.
    struct DecidedWritesOnlyStore {
        inner: MemEventStore,
        armed: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl EventStore for DecidedWritesOnlyStore {
        async fn append(&mut self, event: Map<String, Value>) -> KnlResult<crate::knl::Committed> {
            if self.armed.load(Ordering::Relaxed) {
                return Err(KnlError::Storage(
                    "this store takes only what a decision wrote".to_string(),
                ));
            }
            self.inner.append(event).await
        }

        async fn append_if(
            &mut self,
            kinds: Option<&[&str]>,
            decide: crate::knl::Decision,
        ) -> KnlResult<Option<crate::knl::Committed>> {
            self.inner.append_if(kinds, decide).await
        }

        async fn read_kinds(
            &self,
            kinds: Option<&[&str]>,
            from_seq: u64,
            limit: usize,
        ) -> KnlResult<Vec<Value>> {
            self.inner.read_kinds(kinds, from_seq, limit).await
        }

        async fn head(&self) -> KnlResult<Option<u64>> {
            self.inner.head().await
        }

        async fn len(&self) -> KnlResult<usize> {
            self.inner.len().await
        }
    }

    /// A refusal is written by the decision that refused, not by a second
    /// append afterwards.
    ///
    /// It used to be the second append, and that made two different outcomes
    /// look the same: if the write of the `budget_refused` failed, the caller
    /// got a storage error with nothing in the log to say the reservation had
    /// been decided at all — indistinguishable from a decision that never
    /// happened. Now exactly one of `budget_reserved` / `budget_refused`
    /// lands, in the transaction that took the decision, and the answer this
    /// call gives is which of the two it was.
    #[tokio::test]
    async fn a_refusal_lands_in_the_transaction_that_decided_it() {
        let armed = Arc::new(AtomicBool::new(false));
        let store = DecidedWritesOnlyStore {
            inner: MemEventStore::new(),
            armed: Arc::clone(&armed),
        };
        let mut s = Session::open_on("user".to_string(), Some(grant(10)), Box::new(store))
            .await
            .expect("open");

        // From here on, a plain append fails: only a decision may write.
        armed.store(true, Ordering::Relaxed);

        assert_eq!(
            s.reserve(50).await,
            Ok(false),
            "the refusal is the decision's own write, so it lands"
        );
        assert_eq!(
            s.reserve(4).await,
            Ok(true),
            "and so is the reservation that fits"
        );

        let moves = ledger(&s).await;
        assert_eq!(
            kinds(&moves),
            vec![
                KIND_BUDGET_GRANTED,
                KIND_BUDGET_REFUSED,
                KIND_BUDGET_RESERVED
            ],
            "exactly one entry per decision"
        );
        assert_eq!(*field(&moves[1], FIELD_AMOUNT), json!(50), "what was asked");
        assert_eq!(
            *field(&moves[1], FIELD_REMAINING),
            json!(10),
            "and the balance the decision measured it against"
        );
        assert_eq!(remaining(&s).await, Some(6), "a refusal moved nothing");

        // The store really is refusing plain appends: an ordinary record is
        // the thing this session can no longer write.
        let err = s
            .append(obj(json!({ "kind": "note" })))
            .await
            .expect_err("a plain append");
        assert_eq!(err.kind(), KnlError::STORAGE);
    }

    /// A store whose `head` read is down: appends land, but nothing can ask
    /// the ledger where it stands.
    struct HeadlessStore {
        inner: MemEventStore,
    }

    #[async_trait::async_trait]
    impl EventStore for HeadlessStore {
        async fn append(&mut self, event: Map<String, Value>) -> KnlResult<crate::knl::Committed> {
            self.inner.append(event).await
        }

        async fn append_if(
            &mut self,
            kinds: Option<&[&str]>,
            decide: crate::knl::Decision,
        ) -> KnlResult<Option<crate::knl::Committed>> {
            self.inner.append_if(kinds, decide).await
        }

        async fn read_kinds(
            &self,
            kinds: Option<&[&str]>,
            from_seq: u64,
            limit: usize,
        ) -> KnlResult<Vec<Value>> {
            self.inner.read_kinds(kinds, from_seq, limit).await
        }

        async fn head(&self) -> KnlResult<Option<u64>> {
            Err(KnlError::Busy("the head read is contended".to_string()))
        }

        async fn len(&self) -> KnlResult<usize> {
            self.inner.len().await
        }
    }

    /// A store that cannot be read has no balance to report, and the kernel
    /// says so rather than serving the last fold.
    ///
    /// Both values this call can otherwise hand back read as facts about the
    /// budget — a number says "you have this much", a `false` from
    /// `exhausted` says "carry on" — and the caller acting on either is a
    /// loop deciding whether it may go on spending.  So the failure
    /// surfaces, classified, and what to do about a contended read is the
    /// caller's.
    #[tokio::test]
    async fn a_balance_that_cannot_be_read_is_an_error_not_a_stale_fold() {
        let store = HeadlessStore {
            inner: MemEventStore::new(),
        };
        let s = Session::open_on("user".to_string(), Some(grant(100)), Box::new(store))
            .await
            .expect("the appends land; only the head read is down");

        let err = s
            .remaining()
            .await
            .expect_err("a failed read must not fold into a number");
        assert_eq!(err.kind(), KnlError::BUSY, "the class travels out intact");
        assert!(err.is_retryable(), "contention is the one retryable class");

        let err = s.exhausted().await.expect_err("nor into a boolean");
        assert_eq!(err.kind(), KnlError::BUSY);

        // Only the reading failed: the record itself is exactly as written.
        assert_eq!(
            s.len().await.expect("len"),
            2,
            "session_opened + budget_granted landed"
        );
    }

    /// (Fix 5) Resuming a nonexistent SQLite stream is a caller error, not an
    /// anonymous empty session.
    #[tokio::test]
    async fn resume_of_a_nonexistent_sqlite_stream_is_a_caller_error() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        // A stream that was never opened as a session: its log is empty.
        let store = SqliteEventStore::open(&path, "ghost-stream", &IsleDrivers::new())
            .await
            .expect("open");
        let err = Session::resume(Some(grant(100)), Box::new(store))
            .await
            .expect_err("an empty stream has no session to resume");
        assert!(
            err.reason().contains("no session to resume"),
            "{}",
            err.reason()
        );
    }

    /// (Concurrency) Two `Session` handles on ONE durable stream, each with a
    /// view of the head from before the other wrote: both append, and the log
    /// holds both in the order they arrived.  This is the scenario that used
    /// to be a head conflict — an append records a fact, and a fact is not
    /// refused for what its writer had last seen.
    #[tokio::test]
    async fn two_sessions_on_one_stream_both_append_and_the_log_interleaves() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "interleave-stream";
        let drivers = IsleDrivers::new();

        // A opens the session on the shared stream: `session_opened` at seq 1
        // and its `budget_granted` at seq 2, so A has seen head 2.
        let store_a = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(1000)), Box::new(store_a))
            .await
            .expect("open A");

        // B resumes the SAME stream while it holds only those two, so both
        // handles have seen exactly head 2.  (It resumes before A closes: a
        // closed session is not resumable.)
        let store_b = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open B");
        let mut b = Session::resume(None, Box::new(store_b))
            .await
            .expect("resume B");
        assert_eq!(remaining(&b).await, Some(1000), "B resumed on A's ledger");
        assert_eq!(
            (a.len().await.expect("len"), b.len().await.expect("len")),
            (2, 2),
            "both see the same two events"
        );

        // A appends, so B's view is now out of date — and B appends anyway.
        assert_eq!(a.append(response(10)).await.expect("A appends"), 3);
        assert_eq!(
            b.append(response(20)).await.expect("B appends too"),
            4,
            "B's write lands after A's, rather than being refused"
        );
        // And A, now out of date in its turn, goes on writing.
        assert_eq!(a.append(response(30)).await.expect("A appends again"), 5);

        // The durable log holds all three, in arrival order.
        let verify = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen to verify");
        let log = as_current(verify.read(0, usize::MAX).await.expect("read log"));
        let responses: Vec<u64> = log
            .iter()
            .filter(|e| e.kind() == "llm_response")
            .map(Current::seq)
            .collect();
        assert_eq!(responses, [3, 4, 5], "every append landed, in order");
    }

    /// (Concurrency) The invariant that *is* a decision: two handles on one
    /// stream, ten granted, each asking for six.  The decision is taken inside
    /// the store, against the ledger as it stands there, so exactly one is
    /// allowed — and the fold says four, not minus two.
    #[tokio::test]
    async fn two_sessions_cannot_both_reserve_the_same_allowance() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "reserve-race-stream";
        let drivers = IsleDrivers::new();

        let store_a = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(10)), Box::new(store_a))
            .await
            .expect("open A");
        let store_b = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open B");
        let mut b = Session::resume(None, Box::new(store_b))
            .await
            .expect("resume B");
        assert_eq!(remaining(&b).await, Some(10), "both see the whole grant");
        assert_eq!(remaining(&a).await, Some(10));

        // A takes six.  B still believes it has ten — and is refused all the
        // same, because the balance it is measured against is the one in the
        // store, not the one it cached.
        assert_eq!(a.reserve(6).await, Ok(true), "the first reservation fits");
        assert_eq!(b.reserve(6).await, Ok(false), "the second does not");
        assert_eq!(remaining(&b).await, Some(4), "B's balance is the ledger's");
        assert_eq!(remaining(&a).await, Some(4), "and so is A's");

        // The ledger is the answer: 10 granted − 6 reserved = 4, with the
        // refusal recorded and moving nothing.
        let verify = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen to verify");
        let log = as_current(verify.read(0, usize::MAX).await.expect("read log"));
        assert_eq!(fold_balance(&log), Some(4), "no allowance was taken twice");
        let moves: Vec<&str> = kinds(&log)
            .into_iter()
            .filter(|k| k.starts_with("budget_"))
            .collect();
        assert_eq!(
            moves,
            [
                KIND_BUDGET_GRANTED,
                KIND_BUDGET_RESERVED,
                KIND_BUDGET_REFUSED
            ],
            "one grant, one reservation, one refusal"
        );
        let refused = log.last().expect("the refusal");
        assert_eq!(*field(refused, FIELD_AMOUNT), json!(6));
        assert_eq!(
            *field(refused, FIELD_REMAINING),
            json!(4),
            "what there really was"
        );
    }

    /// (Concurrency) "Closed" is the handle's, and the log records what
    /// arrives after it.  Three handles, one close: the two that never saw it
    /// go on writing, and their writes land *after* the `session_closed` —
    /// which is the fact an audit is reading for, and would be gone if the
    /// store had refused them.  A second handle closing writes a second
    /// ending, because that is what happened.
    #[tokio::test]
    async fn a_close_is_the_handles_and_the_log_records_what_arrives_after_it() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "close-race-stream";
        let drivers = IsleDrivers::new();

        let store_a = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(100)), Box::new(store_a))
            .await
            .expect("open A");
        // Both resume while the stream is open — a closed one is not
        // resumable — so both hold `closed = false` across A's close.
        let store_b = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open B");
        let mut b = Session::resume(None, Box::new(store_b))
            .await
            .expect("resume B");
        let store_c = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open C");
        let mut c = Session::resume(None, Box::new(store_c))
            .await
            .expect("resume C");

        a.close(Some("done")).await.expect("A closes");
        assert!(a.is_closed());
        assert!(!b.is_closed(), "B's flag is its own and has not moved");
        assert!(!c.is_closed());

        // B writes, and the write lands: the store serializes appends, it does
        // not adjudicate them.
        assert_eq!(
            b.append(obj(json!({ "kind": "note" }))).await,
            Ok(4),
            "an append after another handle's close is recorded"
        );
        assert!(!b.is_closed(), "landing a write closed nothing");

        // C's budget moves are decided on the balance alone — 100 granted,
        // nothing spent, so both go through.
        assert_eq!(c.reserve(5).await, Ok(true), "the ledger covers it");
        assert_eq!(c.spend(10).await, Ok(()), "the settlement lands");
        assert_eq!(
            remaining(&c).await,
            Some(85),
            "100 − 5 − 10, folded in the tx"
        );
        assert!(!c.is_closed());

        // B closing writes a *second* ending; A closing again writes nothing,
        // because A's own flag is set.
        b.close(Some("late")).await.expect("B closes");
        a.close(Some("again"))
            .await
            .expect("A is idempotent per handle");

        let verify = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen to verify");
        let log = as_current(verify.read(0, usize::MAX).await.expect("read log"));
        assert_eq!(
            kinds(&log),
            [
                KIND_SESSION_OPENED,
                KIND_BUDGET_GRANTED,
                KIND_SESSION_CLOSED,
                "note",
                KIND_BUDGET_RESERVED,
                KIND_BUDGET_SPENT,
                KIND_SESSION_CLOSED,
            ],
            "everything that happened, in the order it arrived"
        );

        let endings: Vec<&Current> = log
            .iter()
            .filter(|event| event.kind() == KIND_SESSION_CLOSED)
            .collect();
        assert_eq!(endings.len(), 2, "two handles closed, two endings recorded");
        assert_eq!(*field(endings[0], FIELD_REASON), json!("done"));
        assert_eq!(*field(endings[1], FIELD_REASON), json!("late"));
        assert_eq!(
            fold_balance(&log),
            Some(85),
            "the ledger is what the moves that landed add up to"
        );
    }

    /// (Concurrency) A settlement records the move and says nothing else; the
    /// balance afterwards is the ledger's, so it is exact on a stream two
    /// handles write to — B reads what the log says, not a number it was
    /// holding before A spent.
    #[tokio::test]
    async fn a_settlement_records_the_move_and_the_balance_is_the_ledgers() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "spend-race-stream";
        let drivers = IsleDrivers::new();

        let store_a = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(100)), Box::new(store_a))
            .await
            .expect("open A");
        let store_b = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open B");
        let mut b = Session::resume(None, Box::new(store_b))
            .await
            .expect("resume B");
        assert_eq!(
            (remaining(&a).await, remaining(&b).await),
            (Some(100), Some(100))
        );

        assert_eq!(a.spend(30).await, Ok(()), "A settles 30 of the 100");
        assert_eq!(
            remaining(&a).await,
            Some(70),
            "and reads the balance separately"
        );
        assert_eq!(
            remaining(&b).await,
            Some(70),
            "B wrote nothing and still reads A's settlement off the ledger"
        );

        // B's own settlement measures against the ledger — 100 − 30 − 20 —
        // rather than subtracting 20 from a number it was holding.
        assert_eq!(b.spend(20).await, Ok(()), "B settles 20");
        assert_eq!(remaining(&b).await, Some(50), "both settlements are in it");

        let verify = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen to verify");
        let log = as_current(verify.read(0, usize::MAX).await.expect("read log"));
        assert_eq!(fold_balance(&log), Some(50), "the balance is the fold");
        let moves: Vec<&str> = kinds(&log)
            .into_iter()
            .filter(|kind| kind.starts_with("budget_"))
            .collect();
        assert_eq!(
            moves,
            [KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT, KIND_BUDGET_SPENT],
            "one grant and two settlements"
        );

        // A settlement never refuses: it floors at zero, as the fold does.
        assert_eq!(a.spend(1_000).await, Ok(()));
        assert_eq!(b.spend(1).await, Ok(()));
        assert_eq!(remaining(&a).await, Some(0));
        assert_eq!(remaining(&b).await, Some(0));
        let verify = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen to verify");
        assert_eq!(
            fold_balance(&as_current(
                verify.read(0, usize::MAX).await.expect("read log")
            )),
            Some(0),
            "the ledger floors at zero rather than going into debt"
        );
    }

    /// (Concurrency) The balance is the ledger and nothing else, so a handle
    /// that has written nothing at all still reports what the other one
    /// spent: `B` never calls a write in this test, and every answer it gives
    /// comes from folding the stream it shares with `A`.
    #[tokio::test]
    async fn a_handle_that_wrote_nothing_reports_what_the_other_spent() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "shared-balance-stream";
        let drivers = IsleDrivers::new();

        let store_a = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(100)), Box::new(store_a))
            .await
            .expect("open A");
        let store_b = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("open B");
        // Not `mut`: reading a balance is a read, and B does nothing else.
        let b = Session::resume(None, Box::new(store_b))
            .await
            .expect("resume B");
        assert_eq!(
            remaining(&b).await,
            Some(100),
            "both start on the same ledger"
        );

        assert_eq!(a.spend(30).await, Ok(()), "A settles 30");
        assert_eq!(
            remaining(&b).await,
            Some(70),
            "B sees the settlement it did not make"
        );

        assert_eq!(a.reserve(20).await, Ok(true), "A reserves 20");
        assert_eq!(remaining(&b).await, Some(50), "and the reservation too");
        assert!(!exhausted(&b).await);

        // Reading twice over a stream that has not moved repeats the fold's
        // answer rather than drifting from it.
        assert_eq!(
            remaining(&b).await,
            Some(50),
            "a second read is the same read"
        );

        assert_eq!(a.spend(1_000).await, Ok(()), "A overspends");
        assert_eq!(remaining(&b).await, Some(0), "the floor is the ledger's");
        assert!(exhausted(&b).await);

        // And the log is the whole of the story: nothing B holds was needed.
        let verify = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen to verify");
        assert_eq!(
            fold_balance(&as_current(
                verify.read(0, usize::MAX).await.expect("read log")
            )),
            remaining(&b).await
        );
    }

    /// A test-local step, standing in for a real one: it renames the two kinds
    /// a hypothetical earlier shape used.  The kernel chain is empty until the
    /// first release, so the seam is exercised with a chain the test owns.
    ///
    /// It leaves the version alone.  The version a step *produces* is
    /// [`CURRENT_SCHEMA_VERSION`], which is still `1` — the same one these
    /// rows were written under — and a `Current` is asserted to be at it, so
    /// a fixture that stamped `2` would be claiming a version the kernel does
    /// not have.
    struct RenameLegacyKinds;

    impl crate::knl::Upcaster for RenameLegacyKinds {
        fn upcast(&self, mut event: Value) -> Value {
            // Not an object at all: unchanged.  An upcaster is total and
            // infallible.
            let Some(map) = event.as_object_mut() else {
                return event;
            };
            let renamed = match map.get(FIELD_KIND).and_then(Value::as_str) {
                Some("legacy_opened") => Some(KIND_SESSION_OPENED),
                Some("legacy_response") => Some("llm_response"),
                _ => None,
            };
            if let Some(kind) = renamed {
                map.insert(FIELD_KIND.to_string(), Value::from(kind));
            }
            event
        }
    }

    /// (Upcasting seam) Every read a session makes goes through the chain
    /// wrapped round its backend — the restore fold a resume takes, `events`,
    /// the `tail` view and the balance fold alike — while the rows on disk
    /// keep the shape they were written in.
    #[tokio::test]
    async fn a_session_reads_every_path_through_the_upcaster_seam() {
        use crate::knl::{
            SqliteEventStore, Upcaster, CURRENT_SCHEMA_VERSION, SCHEMA_VERSION_FIELD,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "seam-stream";
        let drivers = IsleDrivers::new();

        // Seeded under the older kind names, through the store itself: the
        // rows are ordinary appends, so they carry the version they were
        // written under.
        {
            let mut store = SqliteEventStore::open(&path, stream, &drivers)
                .await
                .expect("open");
            store
                .append(obj(json!({
                    "kind": "legacy_opened",
                    "data": { "owner": "user-3", "scope_id": "scope-from-the-log" }
                })))
                .await
                .expect("the opening");
            store
                .append(obj(json!({
                    "kind": "budget_granted",
                    "data": { "amount": 100, "tag": "tokens" }
                })))
                .await
                .expect("the grant");
            store
                .append(obj(json!({
                    "kind": "legacy_response", "beat": "b-1",
                    "data": {
                        "content": [{ "type": "text", "text": "ok" }],
                        "usage": { "input_tokens": 7 }
                    }
                })))
                .await
                .expect("the response");
        }

        // The seam the session reads through, carrying the test's own chain:
        // `resume_on` is the body of `resume` for exactly this, so the
        // session is otherwise the one production builds.
        let chain: Vec<Arc<dyn Upcaster>> = vec![Arc::new(RenameLegacyKinds)];
        let seamed = CurrentStore::new(
            Box::new(
                SqliteEventStore::open(&path, stream, &drivers)
                    .await
                    .expect("reopen"),
            ),
            chain,
        );
        let mut resumed = Session::resume_on(None, seamed)
            .await
            .expect("resume through the seam");

        // The restore read went through the chain: the opening was only a
        // `session_opened` after the step, and the scope came off it.
        assert_eq!(resumed.owner(), "user-3", "the owner the step revealed");
        assert_eq!(resumed.scope_id(), "scope-from-the-log");
        assert_eq!(
            resumed.grant().and_then(|g| g.tag.as_deref()),
            Some("tokens"),
            "and the grant with it"
        );

        // …and so do `events`, the `tail` view and the balance fold.
        let log = resumed.events(0).await.expect("events");
        assert_eq!(
            kinds(&log),
            [KIND_SESSION_OPENED, KIND_BUDGET_GRANTED, "llm_response"],
            "every read is projected"
        );
        let tail = resumed
            .view(VIEW_TAIL, Some(&obj(json!({ "n": 1 }))))
            .await
            .expect("tail");
        let last = &tail.as_array().expect("array")[0];
        assert_eq!(
            kind_of(last),
            "llm_response",
            "the view read the projected kind: {last}"
        );
        assert_eq!(last[FIELD_DATA]["usage"]["input_tokens"], json!(7));
        assert_eq!(
            remaining(&resumed).await,
            Some(100),
            "the balance folds too"
        );

        // The stored rows were not rewritten: read them without the seam and
        // the old names are still there, under the version they were written
        // with.
        let raw = SqliteEventStore::open(&path, stream, &drivers)
            .await
            .expect("reopen raw");
        let stored = raw.read(0, usize::MAX).await.expect("read raw");
        assert_eq!(kind_of(&stored[0]), "legacy_opened", "{}", stored[0]);
        assert_eq!(kind_of(&stored[2]), "legacy_response", "{}", stored[2]);
        // And a filtered read of the *stored* stream selects on those old
        // names, which is the obligation a renaming step takes on: the
        // projected name finds nothing until the rows are rewritten, and they
        // never are.
        assert!(
            raw.read_kinds(Some(&[KIND_SESSION_OPENED]), 0, usize::MAX)
                .await
                .expect("read raw")
                .is_empty(),
            "the kind filter selects on what is stored"
        );
        assert_eq!(
            stored[0].get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION),
            "an untouched row keeps the version it was written under: {}",
            stored[0]
        );
    }

    /// (Upcasting seam) A stream whose ending is only visible *after* the
    /// step is still an ending: the disposable rule reads the projected log,
    /// not the stored one.
    #[tokio::test]
    async fn a_closed_stream_seen_through_the_seam_is_still_refused() {
        use crate::knl::{SqliteEventStore, Upcaster};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "seam-closed-stream";
        let drivers = IsleDrivers::new();

        {
            let mut store = SqliteEventStore::open(&path, stream, &drivers)
                .await
                .expect("open");
            store
                .append(obj(json!({
                    "kind": "legacy_opened",
                    "data": { "owner": "user-3", "scope_id": "scope-from-the-log" }
                })))
                .await
                .expect("the opening");
            store
                .append(obj(json!({
                    "kind": "session_closed", "data": { "reason": "done" }
                })))
                .await
                .expect("the ending");
        }

        let chain: Vec<Arc<dyn Upcaster>> = vec![Arc::new(RenameLegacyKinds)];
        let seamed = CurrentStore::new(
            Box::new(
                SqliteEventStore::open(&path, stream, &drivers)
                    .await
                    .expect("reopen"),
            ),
            chain,
        );
        let err = Session::resume_on(None, seamed)
            .await
            .expect_err("a stream that ended must not be resumed");
        assert!(
            err.reason().contains("session is closed"),
            "{}",
            err.reason()
        );
    }

    // -- the in-memory database, and reading the log with SQL ---------------

    /// An in-memory session is a session, not a lesser one: it is a stream in
    /// a real database, so a second handle on its name finds the same log and
    /// resuming it restores the state.  What it cannot do is outlive the
    /// process — the database is reclaimed when the last handle on it goes —
    /// and nothing here pretends otherwise.
    #[tokio::test]
    async fn an_in_memory_stream_is_resumable_while_it_is_open() {
        let mut s = new_session(Some(100)).await;
        assert_eq!(s.reserve(30).await, Ok(true));
        s.append(obj(json!({ "kind": "note", "data": { "text": "hi" } })))
            .await
            .expect("append");

        // The session id *is* the stream, so it is what a resume names.
        let store = SqliteEventStore::open_memory(s.id(), &IsleDrivers::new())
            .await
            .expect("reopen the stream");
        let resumed = Session::resume(None, Box::new(store))
            .await
            .expect("resume");
        assert_eq!(resumed.owner(), ANON);
        assert_eq!(remaining(&resumed).await, Some(70), "the ledger came back");
        assert_eq!(
            kinds(&resumed.events(0).await.expect("events")),
            vec![
                KIND_SESSION_OPENED,
                KIND_BUDGET_GRANTED,
                KIND_BUDGET_RESERVED,
                "note"
            ]
        );

        // Two sessions are two databases: neither name is the other's.
        let other = new_session(Some(100)).await;
        assert_ne!(other.id(), s.id());
        assert_eq!(
            other.len().await.expect("len"),
            2,
            "opened + granted, and no note"
        );
    }

    /// A session reads its own log with SQL, and `$stream` is what makes
    /// "its own" true without the caller having to know its id.
    #[tokio::test]
    async fn a_session_reads_its_own_log_with_sql() {
        let mut s = new_session(None).await;
        s.append(obj(
            json!({ "kind": "msg_user", "data": { "content": "hi" } }),
        ))
        .await
        .expect("append");
        s.append(response(9)).await.expect("recorded");

        let found = s
            .query(
                "SELECT kind, seq FROM events WHERE stream = $stream ORDER BY seq",
                QueryParams::None,
                &QueryOpts::default(),
            )
            .await
            .expect("query");
        assert!(!found.truncated);
        let kinds: Vec<&str> = found
            .rows
            .iter()
            .map(|row| row["kind"].as_str().expect("a kind"))
            .collect();
        assert_eq!(kinds, [KIND_SESSION_OPENED, "msg_user", "llm_response"]);

        // A fold the kernel does not name — how many events of each kind —
        // is a query rather than a view it had to be taught.
        let counted = s
            .query(
                "SELECT kind, COUNT(*) AS n FROM events WHERE stream = $stream \
                 GROUP BY kind ORDER BY kind",
                QueryParams::None,
                &QueryOpts::default(),
            )
            .await
            .expect("query");
        assert_eq!(counted.rows.len(), 3);

        // Another session's stream is not this one's, even in the same
        // process: the set a query reads is the set it named.
        let mut other = new_session(None).await;
        other
            .append(obj(json!({ "kind": "only_theirs" })))
            .await
            .expect("append");
        let mine = s
            .query(
                "SELECT kind FROM events WHERE stream IN $sessions",
                QueryParams::None,
                &QueryOpts::default(),
            )
            .await
            .expect("query");
        assert!(
            !mine.rows.iter().any(|row| row["kind"] == "only_theirs"),
            "{:?}",
            mine.rows
        );

        // Reads keep working after the handle closed, like every other read.
        s.close(None).await.expect("close");
        assert!(s
            .query("SELECT 1 AS one", QueryParams::None, &QueryOpts::default())
            .await
            .is_ok());
    }

    // -- children: the parent link, and the allocation that paid for it ------

    /// A store for `stream` on the database `parent` is already on.
    ///
    /// What [`Session::open_child`] requires, built the way the bridge builds
    /// it: the parent is asked where it is, and the child's store is opened
    /// there.  The drivers are thrown away on the spot for the same reason
    /// the rest of these tests throw them away — the connection thread lives
    /// as long as the store holding its handle does.
    async fn store_beside(parent: &Session, stream: &str) -> Box<dyn EventStore> {
        let db = parent.database().expect("the parent is on a database");
        Box::new(
            SqliteEventStore::open(std::path::Path::new(db), stream, &IsleDrivers::new())
                .await
                .expect("a store on the parent's database"),
        )
    }

    /// A fresh stream id, as the layer that opens a child mints one.
    fn stream_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// A second handle on `of`'s stream: another store on the same database,
    /// resumed.
    ///
    /// Two handles on one stream is a supported shape — the store serializes
    /// their writes — and it is the only way to have two callers allocating
    /// from one parent at the same time.
    async fn another_handle(of: &Session) -> Session {
        let db = of.database().expect("a database");
        let store = SqliteEventStore::open(std::path::Path::new(db), of.id(), &IsleDrivers::new())
            .await
            .expect("reopen the stream");
        let mut handle = Session::resume(None, Box::new(store))
            .await
            .expect("resume");
        handle.adopt_id(of.id().to_string());
        handle
    }

    /// A session with `budget` on the file at `path`, so two handles can
    /// contend for one balance through two real connections.
    async fn file_session(path: &std::path::Path, budget: i64, drivers: &IsleDrivers) -> Session {
        let stream = stream_id();
        let store = SqliteEventStore::open(path, stream.clone(), drivers)
            .await
            .expect("open the stream");
        let mut session = Session::open_on(ANON.to_string(), Some(grant(budget)), Box::new(store))
            .await
            .expect("open");
        session.adopt_id(stream);
        session
    }

    /// (2a) The allocation lands on both sides: the child opens with the
    /// units and with its parent named, and the parent's ledger carries the
    /// reservation that paid for them — one transaction, two streams.
    ///
    /// And nothing comes back.  A child closing is not a refund: the balance
    /// only rises when an owner grants.
    #[tokio::test]
    async fn an_allocation_opens_the_child_and_moves_the_units() {
        let mut parent = new_session(Some(100)).await;
        let stream = stream_id();
        let store = store_beside(&parent, &stream).await;

        let mut child = parent
            .open_child(
                stream.clone(),
                "user-42".to_string(),
                Allocation::new(40),
                store,
            )
            .await
            .expect("the parent's balance covers it");

        assert_eq!(child.id(), stream, "the child is the stream it was given");
        assert_eq!(child.owner(), "user-42");
        assert_eq!(remaining(&parent).await, Some(60), "the parent paid");
        assert_eq!(remaining(&child).await, Some(40), "and the child holds it");

        // The child's log: it opened, and it opened with the grant. Both name
        // the parent, and the scope the handle reports is the one the opening
        // recorded — the child is a resumed session over what was written for
        // it, not a value built beside the log.
        let opened = child.events(0).await.expect("events");
        assert_eq!(
            kinds(&opened),
            vec![KIND_SESSION_OPENED, KIND_BUDGET_GRANTED]
        );
        assert_eq!(
            field(&opened[0], FIELD_PARENT).as_str(),
            Some(parent.id()),
            "{}",
            opened[0]
        );
        assert_eq!(
            field(&opened[0], FIELD_SCOPE_ID).as_str(),
            Some(child.scope_id())
        );
        assert_eq!(*field(&opened[1], FIELD_AMOUNT), json!(40));
        assert_eq!(field(&opened[1], FIELD_PARENT).as_str(), Some(parent.id()));
        assert_eq!(
            field(&opened[1], FIELD_TAG).as_str(),
            Some("tokens"),
            "the child counts in the parent's unit unless it was renamed"
        );

        // The parent's ledger: an ordinary reservation, naming where the
        // units went.
        let moves = ledger(&parent).await;
        assert_eq!(
            kinds(&moves),
            vec![KIND_BUDGET_GRANTED, KIND_BUDGET_RESERVED]
        );
        assert_eq!(*field(&moves[1], FIELD_AMOUNT), json!(40));
        assert_eq!(
            field(&moves[1], FIELD_CHILD).as_str(),
            Some(stream.as_str())
        );

        // A child that closes gives nothing back.
        child.close(Some("done")).await.expect("close the child");
        assert_eq!(
            remaining(&parent).await,
            Some(60),
            "an allocation is a spend"
        );
    }

    /// A child may count in a unit of its own, and the parent's own ledger
    /// entry stays in the parent's.
    #[tokio::test]
    async fn an_allocation_may_rename_the_unit_for_the_child() {
        let mut parent = new_session(Some(100)).await;
        let stream = stream_id();
        let store = store_beside(&parent, &stream).await;
        let child = parent
            .open_child(
                stream,
                "user-42".to_string(),
                Allocation {
                    amount: 10,
                    tag: Some("turns".to_string()),
                },
                store,
            )
            .await
            .expect("the allocation");

        let opened = child.events(0).await.expect("events");
        assert_eq!(field(&opened[1], FIELD_TAG).as_str(), Some("turns"));
        let moves = ledger(&parent).await;
        assert_eq!(
            field(&moves[1], FIELD_TAG).as_str(),
            Some("tokens"),
            "the parent's entry counts what the parent counts"
        );
    }

    /// (2b) A balance that will not cover it: the refusal is recorded on the
    /// parent, nothing is opened, and the caller is told with the one class
    /// that reports a decision rather than a fault.
    #[tokio::test]
    async fn an_allocation_the_balance_cannot_cover_is_refused_and_recorded() {
        let mut parent = new_session(Some(10)).await;
        let stream = stream_id();
        let store = store_beside(&parent, &stream).await;

        let err = parent
            .open_child(
                stream.clone(),
                "user-42".to_string(),
                Allocation::new(40),
                store,
            )
            .await
            .expect_err("10 does not cover 40");
        assert_eq!(err.kind(), KnlError::REFUSED, "{err}");
        assert!(
            !err.is_retryable(),
            "the same balance answers the same: {err}"
        );
        assert!(err.reason().contains("40"), "{err}");

        // The balance did not move, and the refusal says what it was measured
        // against and which child it was for.
        assert_eq!(remaining(&parent).await, Some(10));
        let moves = ledger(&parent).await;
        assert_eq!(
            kinds(&moves),
            vec![KIND_BUDGET_GRANTED, KIND_BUDGET_REFUSED]
        );
        assert_eq!(*field(&moves[1], FIELD_AMOUNT), json!(40));
        assert_eq!(*field(&moves[1], FIELD_REMAINING), json!(10));
        assert_eq!(
            field(&moves[1], FIELD_CHILD).as_str(),
            Some(stream.as_str())
        );

        // …and the child's stream was never written: a refused allocation
        // leaves no half-opened session behind.
        let unused = store_beside(&parent, &stream).await;
        assert_eq!(unused.len().await.expect("len"), 0);
    }

    /// A parent with no budget has no ledger to measure against, so the
    /// allocation is allowed — the same rule `reserve` follows.  The child
    /// gets a ledger of its own all the same, because a grant is what starts
    /// one.
    #[tokio::test]
    async fn a_parent_with_no_budget_allocates_without_a_balance_to_measure() {
        let mut parent = new_session(None).await;
        let stream = stream_id();
        let store = store_beside(&parent, &stream).await;
        let child = parent
            .open_child(stream, "user-42".to_string(), Allocation::new(7), store)
            .await
            .expect("there is no balance to refuse against");

        assert_eq!(remaining(&parent).await, None, "still no budget here");
        assert_eq!(remaining(&child).await, Some(7));
    }

    /// The tree is one log.  A child store on another database is refused
    /// before anything is written, because the two halves of an allocation
    /// share a transaction and a transaction covers one database.
    #[tokio::test]
    async fn a_child_on_another_database_is_refused() {
        let mut parent = new_session(Some(100)).await;

        let stranger = stream_id();
        let elsewhere = SqliteEventStore::open_memory(stranger.clone(), &IsleDrivers::new())
            .await
            .expect("another in-memory database");
        let err = parent
            .open_child(
                stranger,
                "user-42".to_string(),
                Allocation::new(10),
                Box::new(elsewhere),
            )
            .await
            .expect_err("that is a different log");
        assert_eq!(err.kind(), KnlError::VALIDATION, "{err}");
        assert!(err.reason().contains("one log"), "{err}");
        assert_eq!(
            parent.len().await.expect("len"),
            2,
            "opened + granted, and nothing else"
        );

        // A store that is not a database at all has no database to share, and
        // says so rather than being taken as "the same one".
        let mut single = Session::open_on(
            ANON.to_string(),
            Some(grant(50)),
            Box::new(MemEventStore::new()),
        )
        .await
        .expect("open");
        let err = single
            .open_child(
                stream_id(),
                "user-42".to_string(),
                Allocation::new(1),
                Box::new(MemEventStore::new()),
            )
            .await
            .expect_err("no database to open a child on");
        assert_eq!(err.kind(), KnlError::VALIDATION, "{err}");
    }

    /// A closed parent opens nothing — whether this handle knows it closed,
    /// or whether the ending is only in the log.  The second is decided
    /// *inside* the write, so a parent that closes between the read and the
    /// insert cannot get a child anyway.
    #[tokio::test]
    async fn a_closed_parent_opens_no_child() {
        let mut parent = new_session(Some(100)).await;
        // Taken while the stream is open: a resume refuses a closed one.
        let mut other = another_handle(&parent).await;
        parent.close(Some("done")).await.expect("close");

        let stream = stream_id();
        let store = store_beside(&parent, &stream).await;
        let err = parent
            .open_child(stream, "user-42".to_string(), Allocation::new(1), store)
            .await
            .expect_err("this handle closed");
        assert_eq!(err.kind(), KnlError::CLOSED, "{err}");

        // The other handle never saw the ending; the decision does.
        let stream = stream_id();
        let store = store_beside(&other, &stream).await;
        let err = other
            .open_child(
                stream.clone(),
                "user-42".to_string(),
                Allocation::new(1),
                store,
            )
            .await
            .expect_err("the log carries an ending");
        assert_eq!(err.kind(), KnlError::CLOSED, "{err}");
        let unused = store_beside(&other, &stream).await;
        assert_eq!(unused.len().await.expect("len"), 0, "nothing was opened");
    }

    /// A close records the children that had not ended, and lands anyway: the
    /// log never refuses a write, and "this ended while what it started was
    /// still going" is the fact worth having.
    #[tokio::test]
    async fn a_close_records_the_children_that_had_not_ended() {
        let mut parent = new_session(Some(100)).await;

        let still_open = stream_id();
        let store = store_beside(&parent, &still_open).await;
        let _running = parent
            .open_child(
                still_open.clone(),
                "user-42".to_string(),
                Allocation::new(10),
                store,
            )
            .await
            .expect("the allocation");

        let ended = stream_id();
        let store = store_beside(&parent, &ended).await;
        let mut done = parent
            .open_child(ended, "user-42".to_string(), Allocation::new(10), store)
            .await
            .expect("the allocation");
        done.close(Some("done")).await.expect("close the child");

        parent.close(Some("done")).await.expect("close");
        let boundary = parent
            .events(0)
            .await
            .expect("events")
            .pop()
            .expect("the boundary");
        assert_eq!(boundary.kind(), KIND_SESSION_CLOSED);
        assert_eq!(
            *field(&boundary, FIELD_OPEN_CHILDREN),
            json!([still_open]),
            "the child that had ended is not among them: {boundary}"
        );
    }

    /// A session with no children says nothing about them: an absent field,
    /// not an empty list, so "there were none" reads the same as it always
    /// did.
    #[tokio::test]
    async fn a_close_with_no_open_children_records_no_such_field() {
        let mut s = new_session(None).await;
        s.close(Some("done")).await.expect("close");
        let boundary = s
            .events(0)
            .await
            .expect("events")
            .pop()
            .expect("the boundary");
        assert_eq!(
            data_field(&boundary, FIELD_OPEN_CHILDREN),
            None,
            "{boundary}"
        );
    }

    /// Two callers allocating from one parent at the same time cannot both be
    /// paid: the decision and the write share a transaction, so the second
    /// measures a balance the first has already spent from.
    ///
    /// On a file, through two connections, because that is where the
    /// contention is real — the loser waits out the winner's `IMMEDIATE`
    /// transaction and then decides against what it committed.
    #[tokio::test]
    async fn two_children_allocating_at_once_never_over_allocate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        let mut one = file_session(&path, 100, &drivers).await;
        let mut two = another_handle(&one).await;

        let first = stream_id();
        let second = stream_id();
        let first_store = store_beside(&one, &first).await;
        let second_store = store_beside(&two, &second).await;

        let (a, b) = tokio::join!(
            one.open_child(
                first,
                "child-a".to_string(),
                Allocation::new(60),
                first_store
            ),
            two.open_child(
                second,
                "child-b".to_string(),
                Allocation::new(60),
                second_store
            ),
        );

        // 60 + 60 is more than the parent had, so exactly one of them was
        // paid for and the sum of what was granted is within the balance.
        let granted: i64 = [&a, &b].iter().filter(|outcome| outcome.is_ok()).count() as i64 * 60;
        assert_eq!(granted, 60, "exactly one allocation may land");
        assert!(
            granted <= 100,
            "the sum of the grants is within the balance"
        );

        let refused = match (&a, &b) {
            (Err(e), Ok(_)) | (Ok(_), Err(e)) => e,
            _ => panic!("one grant and one refusal, got {a:?} / {b:?}"),
        };
        assert_eq!(refused.kind(), KnlError::REFUSED, "{refused}");

        // The parent's log tells the same story: it paid once and turned the
        // other down, and the balance is what is left after the one it paid.
        assert_eq!(remaining(&one).await, Some(40));
        assert_eq!(
            kinds(&ledger(&one).await),
            vec![
                KIND_BUDGET_GRANTED,
                KIND_BUDGET_RESERVED,
                KIND_BUDGET_REFUSED
            ]
        );
    }

    /// An allocation is decided against the whole ledger and against the
    /// ending, so the kinds it asks the store for have to be exactly those.
    /// A kind added to the ledger and missed here would silently fall out of
    /// the balance an allocation measures.
    #[test]
    fn an_allocation_folds_the_ledger_and_looks_for_the_ending() {
        for kind in BUDGET_KINDS {
            assert!(
                ALLOCATION_KINDS.contains(kind),
                "the ledger's {kind} must reach an allocation's decision"
            );
        }
        assert!(ALLOCATION_KINDS.contains(&KIND_SESSION_CLOSED));
        assert_eq!(ALLOCATION_KINDS.len(), BUDGET_KINDS.len() + 1);
    }
}
