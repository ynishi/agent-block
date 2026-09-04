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
//! same allowance twice.  The kernel has exactly one:
//! [`Session::reserve`] writes only if the ledger covers what was asked.
//! [`Session::spend`] is not one of them — a settlement has nothing to
//! decide, so it is a plain append, and the balance it reports is read back
//! off the ledger afterwards.  Neither asks whether the session ended — see
//! below.
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
//! An append does not charge.  It is a record of something that happened,
//! and the budget is a permission asked for *before* something happens —
//! [`Session::reserve`], which the layer that knows what a call costs
//! calls, then settles with [`Session::spend`].  Folding the two together
//! is what turned the budget into a flag that only stands up once the
//! allowance is already gone; the balance and the `usage` projection are
//! independent readings and neither is the other's ledger.
//!
//! # The budget is in the log, and nowhere else
//!
//! Every move of the balance is an event — `budget_granted` when an owner
//! allows, `budget_reserved` / `budget_refused` at the decision point,
//! `budget_spent` at the settlement — written through the same append.  The
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

use std::cell::Cell;

use serde_json::{Map, Value};

use super::budget::{self, fold_balance, last_grant, BudgetGrant};
use super::event::{
    is_kernel_only, kernel_event, kind_of, seq_of, FIELD_AMOUNT, FIELD_DESC, FIELD_DETAIL,
    FIELD_KIND, FIELD_REASON, FIELD_REMAINING, FIELD_SCOPE_ID, FIELD_TAG, KIND_BUDGET_GRANTED,
    KIND_BUDGET_REFUSED, KIND_BUDGET_RESERVED, KIND_BUDGET_SPENT, KIND_SESSION_CLOSED,
    KIND_SESSION_OPENED,
};
use super::event_store::{kernel_upcasters, EventStore, MemEventStore, UpcastingEventStore};
use super::projection::{tail_count, Views, VIEW_TAIL, VIEW_USAGE};
use super::scope::{Scope, ScopeId};
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

/// Whether `events` already carry the stream's ending.
///
/// Read in exactly one place, [`Session::resume`], because a session is
/// disposable: there is no reopening kind, so a single `session_closed`
/// anywhere in the log means the stream is not a state to continue from.
/// No *write* asks this — a write records what happened, and something
/// writing after an ending is the fact an audit most wants recorded.
fn has_ended(events: &[Value]) -> bool {
    events
        .iter()
        .any(|event| kind_of(event) == KIND_SESSION_CLOSED)
}

/// Reserved owner: no principal was named when the session opened.
pub const ANON: &str = "anon";
/// Reserved owner: the session belongs to the system itself.
pub const SYSTEM: &str = "system";

/// Payload field the kernel records on `session_opened`, beside
/// [`FIELD_SCOPE_ID`], so a later [`Session::resume`] can recover the
/// session's scope — who it belonged to — from the log.
///
/// `session_opened` is an open-shape reserved kind (no required fields), so
/// carrying this extra field needs no change to the event validator.
const FIELD_OWNER: &str = "owner";

/// A `budget_granted` event for `grant`, written under `scope_id`.
///
/// Only what the owner said is written — an absent `tag` is an absent
/// field, not a null — so the record carries the grant and nothing more.
fn granted_event(grant: &BudgetGrant, scope_id: &str) -> Map<String, Value> {
    let mut event = kernel_event(KIND_BUDGET_GRANTED);
    event.insert(
        FIELD_SCOPE_ID.to_string(),
        Value::from(scope_id.to_string()),
    );
    event.insert(FIELD_AMOUNT.to_string(), Value::from(grant.amount));
    if let Some(tag) = grant.tag.as_ref() {
        event.insert(FIELD_TAG.to_string(), Value::from(tag.clone()));
    }
    if let Some(desc) = grant.desc.as_ref() {
        event.insert(FIELD_DESC.to_string(), Value::from(desc.clone()));
    }
    event
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
    let mut event = kernel_event(kind);
    event.insert(
        FIELD_SCOPE_ID.to_string(),
        Value::from(scope_id.to_string()),
    );
    event.insert(FIELD_AMOUNT.to_string(), Value::from(amount));
    if let Some(tag) = tag {
        event.insert(FIELD_TAG.to_string(), Value::from(tag.to_string()));
    }
    event
}

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
    /// K1 append-only history, held behind the event-store SPI so the
    /// backend can be the in-memory store or the durable SQLite one.
    store: Box<dyn EventStore>,
    /// Cached projection folds (derived, never authoritative).
    views: Views,
    /// The last balance fold, and the store head it was taken at.
    ///
    /// Not a counter: nothing adds to it or subtracts from it.  A read of
    /// the balance compares the store's head against the `seq` recorded here
    /// and, if the log has moved on, refolds [`fold_balance`] over the
    /// stream and replaces both halves.  So the answer is the ledger's on a
    /// stream two handles write to, and costs one head read on a stream that
    /// has not moved.
    ///
    /// A [`Cell`] because reading a balance is a read: [`Session::remaining`]
    /// takes `&self`, and the cache it refreshes is derived state, not a
    /// change to the session.
    balance: Cell<(u64, Option<i64>)>,
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
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("scope", &self.scope)
            .field("closed", &self.closed)
            .field("len", &self.store.len())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Open a session for `owner` with an optional budget grant, on the
    /// in-memory store.
    ///
    /// `owner` is total: pass a real principal id, or [`ANON`] / [`SYSTEM`]
    /// for the reserved ones.  The `session_opened` event is appended here,
    /// so a fresh session already has one event.
    pub fn new(owner: String, grant: Option<BudgetGrant>) -> KnlResult<Self> {
        Self::open_on(owner, grant, Box::new(MemEventStore::new()))
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
    pub fn open_on(
        owner: String,
        grant: Option<BudgetGrant>,
        store: Box<dyn EventStore>,
    ) -> KnlResult<Self> {
        // Wrap the chosen backend in the read-time upcasting seam, so every one
        // of this session's reads (view folds, `events`, the balance fold, the
        // decision a `reserve` takes inside the store) passes through it by
        // construction.  The chain is empty until the first release; a shape
        // change after it registers its step at that one site.
        let store: Box<dyn EventStore> =
            Box::new(UpcastingEventStore::new(store, kernel_upcasters()));
        let mut session = Self {
            id: uuid::Uuid::new_v4().to_string(),
            // The scope is issued here, before the first event: the
            // `session_opened` below is already written under it.
            scope: Scope::new(owner, grant),
            store,
            views: Views::default(),
            // Nothing folded yet, over a stream with nothing in it: the first
            // read of the balance sees the head move and folds the ledger the
            // two appends below are about to write.
            balance: Cell::new((0, None)),
            closed: false,
        };
        // The same one path records `session_opened` as records everything
        // else, and it CAN fail on a durable backend (a busy database
        // exhausts its retries) — a session that could not record its own
        // opening must not exist, so the error surfaces instead of leaving a
        // stream with no `session_opened`.
        //
        // The scope rides on it: the id the kernel just issued, next to the
        // owner.  Together they are the whole of what a resume needs to
        // restore the scope, so the boundary is in the log and not only in
        // this value.
        let mut started = kernel_event(KIND_SESSION_OPENED);
        started.insert(
            FIELD_OWNER.to_string(),
            Value::from(session.scope.owner().to_string()),
        );
        started.insert(
            FIELD_SCOPE_ID.to_string(),
            Value::from(session.scope.id().to_string()),
        );
        session.append_kernel(started)?;

        // The grant is its own fact, right after the boundary: what the
        // owner allowed is the first entry of the ledger the balance folds
        // from, not a decoration on the run's opening.  A session that
        // could not record its own grant must not exist either — the error
        // surfaces rather than leaving a counter no event accounts for.
        //
        // But the opening is already in the log by then, so returning the
        // error alone would leave a stream that opened and never ended.  The
        // boundary is written first, best effort: a close that fails too is
        // logged, not raised, because the error that matters to the caller is
        // the one that stopped the open.
        if let Some(grant) = session.scope.grant().cloned() {
            let event = granted_event(&grant, session.scope.id());
            if let Err(failed) = session.append_kernel(event) {
                if let Err(unclosed) = session.close_with(
                    Some(CLOSE_REASON_ERROR),
                    Some(&format!("open: budget_granted: {failed}")),
                ) {
                    tracing::warn!(
                        error = %unclosed,
                        cause = %failed,
                        "knl: a failed open could not record session_closed; \
                         the stream is left open"
                    );
                }
                return Err(failed);
            }
        }
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
    /// charged, and what was consumed is the `usage` projection's answer,
    /// not the quota's.
    ///
    /// A closed stream is not resumed.  A session is disposable: it opens
    /// once and ends once, so a log that already carries its `session_closed`
    /// is an ending, not a state to continue from — the caller opens a new
    /// session instead.
    ///
    /// The projection caches start empty and re-fold lazily from the store,
    /// so a resumed session's `usage` view is correct on first read.
    pub fn resume(grant: Option<BudgetGrant>, store: Box<dyn EventStore>) -> KnlResult<Self> {
        // The upcasting seam goes on first, so the restore below reads the
        // same projected shape every other read of this session gets: a log
        // written under an older shape resumes as what it means today, and the
        // stored bytes stay as they were written.
        let store: Box<dyn EventStore> =
            Box::new(UpcastingEventStore::new(store, kernel_upcasters()));

        // Fallible read: a transient busy read or an undecodable row surfaces
        // here rather than being silently folded into a wrong resumed state.
        let log = store.read(0, usize::MAX)?;

        // Resuming an empty or mistyped stream is a caller error, not an
        // anonymous zero session: a real session always opens with a
        // `session_opened`.
        let opened = log
            .iter()
            .find(|event| kind_of(event) == KIND_SESSION_OPENED)
            .ok_or_else(|| {
                KnlError::new("stream has no session to resume (no session_opened event)")
            })?;

        // …and an ended one is not resumed at all.  There is no reopening
        // kind, so any `session_closed` in the stream is the session's
        // ending: a handle that carried on past it would be appending to a
        // log whose readers were told nothing more was coming.
        if has_ended(&log) {
            return Err(KnlError::new(format!(
                "{CLOSED} (disposable; open a new session)"
            )));
        }

        // The scope rides on `session_opened`; an older log written before
        // either field existed falls back — the owner to ANON, the scope id
        // to a fresh kernel-issued one (`Scope::restore` mints it) — but
        // only for a real `session_opened`, never for an absent one.  A
        // session that predates a field is still a session, and refusing to
        // resume it would lose the log rather than protect it.
        let owner = opened
            .get(FIELD_OWNER)
            .and_then(Value::as_str)
            .unwrap_or(ANON)
            .to_string();
        let scope_id: Option<ScopeId> = opened
            .get(FIELD_SCOPE_ID)
            .and_then(Value::as_str)
            .map(str::to_string);

        // The log was read once already, so seed the balance cache from it
        // rather than folding the same events again on the first read: the
        // head it was taken at is the last event's seq (`read` returns events
        // in seq order).  A resume errors above on an empty /
        // session_opened-less log, so this is a real event's seq; the `0`
        // fallback is unreachable but keeps it total.
        let head = log.last().map(seq_of).unwrap_or(0);

        // The grant comes back off the log: a resumed session keeps having a
        // budget (and a tag to report) even when the caller grants nothing
        // new.  What is left of it is the fold, seeded just below.
        let mut session = Self {
            id: uuid::Uuid::new_v4().to_string(),
            scope: Scope::restore(scope_id, owner, last_grant(&log)),
            store,
            views: Views::default(),
            balance: Cell::new((head, fold_balance(&log))),
            closed: false,
        };

        // A fresh grant is the owner allowing more, so it is recorded like
        // any other and *adds* to what was left.  Write-ahead: the event
        // lands first, and a failed append leaves the restored balance
        // exactly as the log describes it.
        //
        // A caller that must vet the restored session *before* anything is
        // written for it — the Lua bridge, which refuses a reserved owner —
        // resumes with no grant and calls [`Session::grant_more`] once the
        // stream has passed.
        if let Some(grant) = grant {
            session.grant_more(grant)?;
        }
        Ok(session)
    }

    /// The owner granting again: record `budget_granted` and raise the
    /// balance by it.
    ///
    /// The one way the balance rises, and it is a fact in the log before it
    /// is a number in the counter — a failed append leaves the balance
    /// exactly as the ledger describes it.  Refused on a closed session, like
    /// every other write: a run that has ended cannot be granted more.
    pub fn grant_more(&mut self, grant: BudgetGrant) -> KnlResult<()> {
        let event = granted_event(&grant, self.scope.id());
        self.append_kernel(event)?;
        self.scope.grant_more(grant)
    }

    /// The session-correlation id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Adopt `id` as the session-correlation id.
    ///
    /// Used only by the durable Lua bridge so a session and the SQLite
    /// stream it writes to share one id: `open_on` / `resume` mint a fresh
    /// id like `new`, and the bridge overrides it to the stream it opened,
    /// so the id a caller resumes by *is* the stream.  `session_opened`
    /// records the scope (its id and the `owner`), never the session id, so
    /// overriding the id does not desync the log.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
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

    /// Record an event, returning its `seq`.  The one write path.
    ///
    /// Any kind is welcome, the reserved ones included, as long as it meets
    /// the shape its kind requires and is not one of the kernel's own
    /// ([`is_kernel_only`]).  The kernel-owned `seq` / `epoch_ms` are
    /// stamped here and overwrite any caller-supplied value; nothing else
    /// is added, and a `beat` the caller declared is recorded as given.
    ///
    /// No append moves the budget, this one included.  What a call may
    /// consume is decided before it happens ([`Session::reserve`]) and
    /// settled after ([`Session::spend`]) by the layer that knows what a
    /// call costs; the history records what happened and says nothing about
    /// what was allowed.
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
    pub fn append(&mut self, event: Map<String, Value>) -> KnlResult<u64> {
        // The kernel's own kinds are refused before the closed check has
        // anything to say about it — the kind is wrong whatever state the
        // session is in.  The balance is a fold of the `budget_*` events, so
        // accepting one from a caller would be letting it grant itself the
        // quota its owner set; the two `session_*` events are the lifecycle
        // a resume and an audit read off the log.
        let kind = event.get(FIELD_KIND).and_then(Value::as_str).unwrap_or("");
        if is_kernel_only(kind) {
            return Err(KnlError::new(format!(
                "{kind:?} is written by the kernel only ({})",
                kernel_only_hint(kind)
            )));
        }
        self.append_kernel(event)
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
    fn append_kernel(&mut self, event: Map<String, Value>) -> KnlResult<u64> {
        if self.closed {
            return Err(KnlError::new(CLOSED));
        }

        // The store orders the write and hands back where it landed.
        let committed = self.store.append(event)?;
        Ok(committed.seq)
    }

    /// Events with `seq >= from`, cloned.
    ///
    /// Fallible: a durable backend can hit a transient busy read or a row it
    /// cannot decode, which surfaces here rather than being dropped silently.
    /// The in-memory backend is always `Ok`.
    pub fn events(&self, from: u64) -> KnlResult<Vec<Value>> {
        self.store.read(from, usize::MAX)
    }

    /// Number of recorded events.  Fallible like [`Session::events`].
    pub fn len(&self) -> KnlResult<usize> {
        self.store.len()
    }

    /// Whether the history is empty (only before `session_opened`, i.e.
    /// never for a session built by [`Session::new`]).
    pub fn is_empty(&self) -> KnlResult<bool> {
        self.store.is_empty()
    }

    /// Ask the budget to allow `amount`: `true` when it was taken, `false`
    /// when the balance would not cover it (and nothing was deducted).
    ///
    /// The stop the budget exists for.  A caller asks before it spends, and
    /// a `false` is a planned halt with the balance untouched — not a
    /// failure, and not a state the run has to be rolled back out of.
    ///
    /// Both answers are recorded: a `budget_reserved` when it was allowed,
    /// a `budget_refused` (carrying what was asked for and what there was)
    /// when it was not.  A refusal is where a run stops, which is the one
    /// thing a log must not be silent about.
    ///
    /// This is the one *command with an invariant* the kernel has, so the
    /// decision is taken inside the store ([`EventStore::append_if`]): the
    /// backend hands the ledger to a fold of [`fold_balance`] and writes the
    /// `budget_reserved` in the same serialized write, so two handles on one
    /// stream cannot both reserve the same allowance.  Nothing is set
    /// afterwards: the write moved the store's head, so the next read of
    /// [`Session::remaining`] refolds the ledger the reservation is now part
    /// of.  A refusal needs no such guard: it is a fact about a decision
    /// already taken, so it is an ordinary append.
    ///
    /// Always `true`, and recorded nowhere, without a budget: a run with no
    /// quota has no ledger to keep.  A handle that has closed refuses, like
    /// [`Session::spend`]; another handle's close is nothing to this one —
    /// the balance is the whole of the invariant.
    pub fn reserve(&mut self, amount: i64) -> KnlResult<bool> {
        if self.closed {
            return Err(KnlError::new(CLOSED));
        }
        budget::check_amount(amount)?;
        // No budget, no ledger: nothing to decide and nothing to record.
        let Some(tag) = self.scope.grant().map(|g| g.tag.clone()) else {
            return Ok(true);
        };
        let scope_id = self.scope.id().to_string();

        // The balance the decision saw, as the ledger read inside the store
        // folds to.  Written by `decide`, which the backend may run more than
        // once (a retried transaction), so the last run is the one that
        // counts — and it is the run whose answer was committed.
        let mut balance = 0_i64;
        // The whole of the decision: does the ledger cover what was asked.
        // Whether the stream carries an ending is not part of it — a
        // reservation past the boundary is a fact about a run that overran
        // its own close, and the log is where facts go.
        let committed = self.store.append_if(&mut |events| {
            balance = fold_balance(events).unwrap_or(0);
            (balance >= amount)
                .then(|| budget_move_event(KIND_BUDGET_RESERVED, amount, tag.as_deref(), &scope_id))
        })?;

        if committed.is_none() {
            // Refused: nothing was written by the decision, so the refusal is
            // recorded here as the ordinary fact it is, carrying what was
            // asked for and what there was.  It moves no balance, and a later
            // read folds the ledger including it and finds the same number.
            let mut event =
                budget_move_event(KIND_BUDGET_REFUSED, amount, tag.as_deref(), &scope_id);
            event.insert(FIELD_REMAINING.to_string(), Value::from(balance));
            self.append_kernel(event)?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Settle `amount` against the budget, returning the new balance.
    ///
    /// The after-the-fact half of [`Session::reserve`]: what a call really
    /// cost, beyond what was reserved for it.  It is recorded as a
    /// `budget_spent`, which is the whole of the move — and recorded
    /// nowhere, moving nothing, when there is no budget.
    ///
    /// A settlement has no invariant to hold — it floors at `0` rather than
    /// refusing — so it is a plain serialized [`Session::append`], not a
    /// command: there is nothing to decide inside the store.  What it hands
    /// back is the balance read through [`Session::remaining`] *after* the
    /// event landed, which is [`fold_balance`] over the whole ledger — so the
    /// answer is exact even when another handle settled in between, where
    /// arithmetic on a number this handle was holding would not be.
    ///
    /// It always writes.  A handle that has closed refuses before the store
    /// is reached; another handle's close does not, and a settlement landing
    /// after one is recorded as what it is.
    pub fn spend(&mut self, amount: i64) -> KnlResult<Option<i64>> {
        if self.closed {
            return Err(KnlError::new(CLOSED));
        }
        budget::check_amount(amount)?;
        let Some(tag) = self.scope.grant().map(|g| g.tag.clone()) else {
            return Ok(None);
        };
        let scope_id = self.scope.id().to_string();

        let event = budget_move_event(KIND_BUDGET_SPENT, amount, tag.as_deref(), &scope_id);
        self.append_kernel(event)?;

        // Read back, do not compute: the settlement is in the log now, and
        // so is everything anyone else wrote before it.
        Ok(self.remaining())
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

    /// The remaining balance (`None` without a budget).
    ///
    /// The ledger's answer, not a counter's: [`fold_balance`] over the
    /// stream, so a handle that has written nothing still sees what another
    /// handle spent.  The fold is cached against the store's head and retaken
    /// only when the head has moved, so a read on a quiet stream costs one
    /// head query.
    ///
    /// A store that cannot be read serves the last fold and says so in a
    /// warning: the balance is a report, and a transient busy read is not a
    /// reason to claim there is no budget.  The two writes that turn on the
    /// balance — [`Session::reserve`] and [`Session::grant_more`] — are
    /// fallible and surface such a failure themselves.
    pub fn remaining(&self) -> Option<i64> {
        let (folded_head_seq, cached) = self.balance.get();

        let head = match self.store.head() {
            Ok(head) => head.unwrap_or(0),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "knl: the stream head could not be read; \
                     the balance is served from the last fold"
                );
                return cached;
            }
        };
        // The log has not moved since the fold, so neither has the balance.
        if head <= folded_head_seq {
            return cached;
        }

        match self.store.read(0, usize::MAX) {
            Ok(events) => {
                let balance = fold_balance(&events);
                self.balance.set((head, balance));
                balance
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "knl: the ledger could not be read; \
                     the balance is served from the last fold"
                );
                cached
            }
        }
    }

    /// Whether the budget is used up (never true without a budget).
    ///
    /// The same fold [`Session::remaining`] reads, asked as a question.
    pub fn exhausted(&self) -> bool {
        matches!(self.remaining(), Some(remaining) if remaining <= 0)
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
    /// Fallible on a durable backend: the `session_closed` append can fail on
    /// a database that stays contended past its retries, or a store that is
    /// gone.  On failure the session stays open (closed is not set), so the
    /// caller knows the boundary was NOT recorded and can retry — a close
    /// that reports success with no `session_closed` in the log would
    /// silently break resume/audit reads.
    pub fn close(&mut self, reason: Option<&str>) -> KnlResult<()> {
        self.close_with(reason, None)
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
    pub fn close_with(&mut self, reason: Option<&str>, detail: Option<&str>) -> KnlResult<()> {
        if self.closed {
            return Ok(());
        }
        let mut event = kernel_event(KIND_SESSION_CLOSED);
        event.insert(
            FIELD_REASON.to_string(),
            Value::from(reason.unwrap_or(DEFAULT_CLOSE_REASON)),
        );
        if let Some(detail) = detail {
            event.insert(FIELD_DETAIL.to_string(), Value::from(detail.to_string()));
        }

        // A plain append, taken through the kernel's own path because
        // `session_closed` is kernel-only and the guarded `append` would
        // refuse the very event that ends the session.  The store is not
        // asked whether an ending is there already: two handles closing one
        // stream write two `session_closed` events, which is what happened
        // and therefore what the log says.
        //
        // The flag moves only after the boundary landed, so a failed append
        // leaves this handle open and the caller free to retry — a close that
        // reported success with nothing in the log would break every later
        // read of it.
        self.append_kernel(event)?;
        self.closed = true;
        Ok(())
    }

    /// A named projection over the history.
    ///
    /// `usage` is served from the incremental cache and reports `at_seq`,
    /// the position it folded to; `tail` reads `opts.n` (default
    /// [`projection::DEFAULT_TAIL_N`]).  An unknown name is an error —
    /// the vocabulary is closed on purpose, and short: a projection whose
    /// shape depends on what the caller does with it (a conversation
    /// rendered for a provider, say) is built from [`Session::events`] on
    /// the shell side rather than named here.
    pub fn view(&mut self, name: &str, opts: Option<&Map<String, Value>>) -> KnlResult<Value> {
        match name {
            VIEW_USAGE => {
                // Fold only what the cache has not seen: read on from the
                // position it folded to, so the view is amortised in new
                // events and Session works against the `EventStore` trait
                // alone — no reach into a concrete `History`.
                let from = self.views.usage_folded_seq().saturating_add(1);
                let fresh = self.store.read(from, usize::MAX)?;
                Ok(self.views.usage(&fresh))
            }
            VIEW_TAIL => {
                let n = tail_count(opts)?;
                let events = self.store.read(0, usize::MAX)?;
                Ok(projection::tail_of(&events, n))
            }
            other => Err(KnlError::new(format!("unknown view {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::{kind_of, seq_of, FIELD_BEAT, KIND_LLM_RESPONSE};
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
    fn new_session(budget: Option<i64>) -> Session {
        Session::new(ANON.to_string(), budget.map(grant)).expect("open")
    }

    /// The balance the log implies, for checking the counter against it.
    fn folded(s: &Session) -> Option<i64> {
        fold_balance(&s.events(0).expect("events"))
    }

    /// The `budget_*` events of a session, in seq order.
    fn ledger(s: &Session) -> Vec<Value> {
        s.events(0)
            .expect("events")
            .into_iter()
            .filter(|e| kind_of(e).starts_with("budget_"))
            .collect()
    }

    /// An `llm_response` event charging `tokens`, as the kernel accepts it.
    fn response(tokens: i64) -> Map<String, Value> {
        obj(json!({
            "kind": "llm_response",
            "content": [{ "type": "text", "text": "ok" }],
            "usage": { "input_tokens": tokens },
            "stop_reason": "end_turn"
        }))
    }

    #[test]
    fn a_new_session_already_carries_session_opened() {
        let s = new_session(None);
        assert_eq!(s.len().expect("len"), 1);
        let events = s.events(0).expect("events");
        assert_eq!(kind_of(&events[0]), KIND_SESSION_OPENED);
        assert_eq!(seq_of(&events[0]), 1);
        assert!(!s.is_closed());
        assert!(!s.id().is_empty());
    }

    /// The scope is issued when the session opens, and it is not the
    /// session: two ids, both real, and neither taken from a caller.  Two
    /// sessions are two scopes.
    #[test]
    fn open_issues_a_scope_id_distinct_from_the_session_id() {
        let a = new_session(None);
        let b = new_session(None);

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
    #[test]
    fn session_opened_and_every_budget_event_carry_the_scope_id() {
        let mut s = new_session(Some(100));
        assert_eq!(s.reserve(30), Ok(true));
        s.spend(10).expect("spend");
        assert_eq!(s.reserve(10_000), Ok(false));

        let scope_id = s.scope_id().to_string();
        let events = s.events(0).expect("events");

        let opened = &events[0];
        assert_eq!(kind_of(opened), KIND_SESSION_OPENED);
        assert_eq!(
            opened.get(FIELD_SCOPE_ID).and_then(Value::as_str),
            Some(scope_id.as_str()),
            "the scope rides on session_opened: {opened}"
        );
        assert_eq!(
            opened.get("owner").and_then(Value::as_str),
            Some(ANON),
            "beside the owner: {opened}"
        );

        let moves = ledger(&s);
        let kinds: Vec<&str> = moves.iter().map(kind_of).collect();
        assert_eq!(
            kinds,
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
                event.get(FIELD_SCOPE_ID).and_then(Value::as_str),
                Some(scope_id.as_str()),
                "a ledger entry must name the scope it was allowed under: {event}"
            );
        }

        // An event a caller appends carries no scope id: the field is the
        // kernel's, on the kinds only the kernel writes.
        s.append(obj(json!({ "kind": "note" }))).expect("append");
        let note = s.events(0).expect("events").pop().expect("note");
        assert_eq!(note.get(FIELD_SCOPE_ID), None, "{note}");
    }

    #[test]
    fn the_owner_is_total_and_read_back_verbatim() {
        assert_eq!(new_session(None).owner(), ANON);
        assert_eq!(
            Session::new(SYSTEM.to_string(), None)
                .expect("open")
                .owner(),
            SYSTEM
        );
        assert_eq!(
            Session::new("user-42".to_string(), None)
                .expect("open")
                .owner(),
            "user-42"
        );
    }

    #[test]
    fn close_records_session_closed_once_with_the_given_reason() {
        let mut s = new_session(None);
        s.close(Some("budget_exhausted")).expect("close");
        s.close(Some("ignored")).expect("close (idempotent)");
        assert_eq!(s.len().expect("len"), 2, "close must be idempotent");

        let last = s.events(2).expect("events").pop().expect("session_closed");
        assert_eq!(kind_of(&last), KIND_SESSION_CLOSED);
        assert_eq!(last["reason"], json!("budget_exhausted"));
        assert!(s.is_closed());
    }

    #[test]
    fn close_without_a_reason_records_the_default() {
        let mut s = new_session(None);
        s.close(None).expect("close");
        let last = s.events(2).expect("events").pop().expect("session_closed");
        assert_eq!(last["reason"], json!(DEFAULT_CLOSE_REASON));
    }

    #[test]
    fn a_closed_session_rejects_writes_but_keeps_serving_reads() {
        let mut s = new_session(Some(10));
        s.append(obj(json!({ "kind": "note" }))).expect("append");
        s.spend(4).expect("spend");
        s.close(None).expect("close");

        let err = s
            .append(obj(json!({ "kind": "note" })))
            .expect_err("append after close");
        assert_eq!(err.reason(), "session is closed");
        let err = s.spend(1).expect_err("spend after close");
        assert_eq!(err.reason(), "session is closed");
        let err = s.reserve(1).expect_err("reserve after close");
        assert_eq!(err.reason(), "session is closed");

        assert_eq!(
            s.len().expect("len"),
            5,
            "session_opened + budget_granted + note + budget_spent + session_closed"
        );
        assert_eq!(s.remaining(), Some(6));
        assert_eq!(folded(&s), s.remaining(), "the ledger is the balance");
        assert!(!s.exhausted());
        assert_eq!(kind_of(&s.events(0).expect("events")[2]), "note");
    }

    #[test]
    fn two_sessions_share_nothing() {
        let mut a = new_session(Some(100));
        let mut b = new_session(Some(100));
        assert_ne!(a.id(), b.id());

        a.append(obj(json!({ "kind": "only_in_a" })))
            .expect("append");
        a.spend(60).expect("spend");

        assert_eq!(
            a.len().expect("len"),
            4,
            "session_opened + budget_granted + only_in_a + budget_spent"
        );
        assert_eq!(b.len().expect("len"), 2, "session_opened + budget_granted");
        assert_eq!(a.remaining(), Some(40));
        assert_eq!(b.remaining(), Some(100));
        // The ledgers are as separate as the histories.
        assert_eq!(folded(&a), Some(40));
        assert_eq!(folded(&b), Some(100));

        a.close(None).expect("close");
        assert!(b.append(obj(json!({ "kind": "still_open" }))).is_ok());
    }

    #[test]
    fn view_serves_the_named_projections_and_rejects_anything_else() {
        let mut s = new_session(None);
        s.append(obj(json!({ "kind": "msg_user", "content": "hi" })))
            .expect("append");
        s.append(response(9)).expect("recorded");

        let usage = s.view(VIEW_USAGE, None).expect("usage");
        assert_eq!(usage["input_tokens"], json!(9));
        assert_eq!(usage["model_calls"], json!(1));
        assert_eq!(
            usage["at_seq"],
            json!(3),
            "session_opened + msg_user + response"
        );

        let tail = s
            .view(VIEW_TAIL, Some(&obj(json!({ "n": 1 }))))
            .expect("tail");
        assert_eq!(tail.as_array().map(Vec::len), Some(1));

        let err = s.view("nope", None).expect_err("unknown view");
        assert_eq!(err.reason(), r#"unknown view "nope""#);
    }

    /// The conversation is not one of the names: how a record becomes a
    /// request — which role each kind takes, whether a system message
    /// belongs in it, where to cut it off — is the shell's decision, and
    /// it builds it from `events` rather than asking the kernel for it.
    #[test]
    fn the_conversation_is_not_a_named_view() {
        let mut s = new_session(None);
        s.append(obj(json!({ "kind": "msg_user", "content": "hi" })))
            .expect("append");

        let err = s.view("dialogue", None).expect_err("dialogue was served");
        assert_eq!(err.reason(), r#"unknown view "dialogue""#);

        let events = s.events(0).expect("events");
        assert_eq!(kind_of(&events[1]), "msg_user");
        assert_eq!(events[1]["content"], json!("hi"));
    }

    /// Appending an `llm_response` records it — verbatim, beat included —
    /// and leaves the budget alone.  What a call was allowed to cost was
    /// decided before it happened; the record of it happening is not a
    /// second place where that is decided.
    #[test]
    fn appending_an_llm_response_records_it_verbatim_without_charging() {
        let mut s = new_session(Some(100));
        let seq = s
            .append(obj(json!({
                "kind": "llm_response",
                // The beat is the caller's word and the kernel keeps it.
                "beat": "beat-7",
                "content": [{ "type": "text", "text": "hi" }],
                "usage": { "input_tokens": 20, "output_tokens": 10 }
            })))
            .expect("append");

        let recorded = s.events(seq).expect("events").pop().expect("llm_response");
        assert_eq!(kind_of(&recorded), KIND_LLM_RESPONSE);
        assert_eq!(
            recorded[FIELD_BEAT],
            json!("beat-7"),
            "the declared beat is recorded as given"
        );

        assert_eq!(s.remaining(), Some(100), "an append must not charge");
        assert_eq!(
            ledger(&s).len(),
            1,
            "only the opening grant is in the ledger"
        );
        let usage = s.view(VIEW_USAGE, None).expect("usage");
        assert_eq!(usage["model_calls"], json!(1));
        assert_eq!(usage["input_tokens"], json!(20));
        assert_eq!(usage["output_tokens"], json!(10));
        assert_eq!(
            folded(&s),
            Some(100),
            "usage and the balance are separate readings"
        );
    }

    /// The grant is the first thing the ledger says, right after the
    /// session's own boundary, with the owner's words on it.
    #[test]
    fn opening_with_a_grant_records_it() {
        let s = Session::new(
            ANON.to_string(),
            Some(BudgetGrant {
                amount: 500,
                tag: Some("tokens".to_string()),
                desc: Some("one nightly run".to_string()),
            }),
        )
        .expect("open");

        let events = s.events(0).expect("events");
        assert_eq!(events.len(), 2, "session_opened + budget_granted");
        assert_eq!(kind_of(&events[0]), KIND_SESSION_OPENED);
        assert_eq!(
            events[0].get("budget"),
            None,
            "the grant is its own event, not a field on session_opened"
        );

        let granted = &events[1];
        assert_eq!(kind_of(granted), KIND_BUDGET_GRANTED);
        assert_eq!(granted["amount"], json!(500));
        assert_eq!(granted["tag"], json!("tokens"));
        assert_eq!(granted["desc"], json!("one nightly run"));
        assert_eq!(s.remaining(), Some(500));
        assert_eq!(folded(&s), s.remaining());

        // A session with no grant keeps no ledger at all.
        let bare = new_session(None);
        assert_eq!(bare.len().expect("len"), 1, "session_opened only");
        assert!(ledger(&bare).is_empty());
        assert_eq!(folded(&bare), None);
    }

    /// A reservation the balance covers is recorded once, deducts exactly
    /// what it asked for, and carries the grant's tag.
    #[test]
    fn a_granted_reservation_is_one_event_and_one_deduction() {
        let mut s = new_session(Some(100));
        assert_eq!(s.reserve(30), Ok(true));

        let moves = ledger(&s);
        assert_eq!(moves.len(), 2, "the grant and the reservation");
        assert_eq!(kind_of(&moves[1]), KIND_BUDGET_RESERVED);
        assert_eq!(moves[1]["amount"], json!(30));
        assert_eq!(moves[1]["tag"], json!("tokens"));
        assert_eq!(s.remaining(), Some(70));
        assert_eq!(folded(&s), s.remaining(), "the ledger is the balance");
    }

    /// A refusal is a fact: it is recorded, with what was asked for and
    /// what there was, and it moves nothing.
    #[test]
    fn a_refused_reservation_is_recorded_and_changes_no_balance() {
        let mut s = new_session(Some(10));
        assert_eq!(s.reserve(11), Ok(false));

        let moves = ledger(&s);
        assert_eq!(moves.len(), 2, "the grant and the refusal");
        assert_eq!(kind_of(&moves[1]), KIND_BUDGET_REFUSED);
        assert_eq!(moves[1]["amount"], json!(11));
        assert_eq!(moves[1]["remaining"], json!(10), "what there was");
        assert_eq!(moves[1]["tag"], json!("tokens"));
        assert_eq!(s.remaining(), Some(10), "a refusal must not deduct");
        assert!(!s.exhausted());
        assert_eq!(folded(&s), s.remaining());

        // And the run can still spend what it has: nothing was consumed.
        assert_eq!(s.reserve(10), Ok(true));
        assert_eq!(s.remaining(), Some(0));
        assert_eq!(folded(&s), Some(0));
    }

    /// The settlement is recorded like everything else, and what the session
    /// reports is the fold of the ledger after any sequence of moves.
    #[test]
    fn the_balance_is_the_fold_after_any_sequence_of_moves() {
        let mut s = new_session(Some(1000));
        assert_eq!(s.reserve(200), Ok(true));
        s.append(response(40)).expect("recorded");
        s.spend(50).expect("spend");
        assert_eq!(s.reserve(10_000), Ok(false));
        assert_eq!(s.reserve(300), Ok(true));
        s.spend(0).expect("spend");

        let moves = ledger(&s);
        let kinds: Vec<&str> = moves.iter().map(kind_of).collect();
        assert_eq!(
            kinds,
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
        assert_eq!(s.remaining(), Some(450), "1000 - 200 - 50 - 300");
        assert_eq!(folded(&s), s.remaining());
    }

    /// The ledger is the kernel's to write: a caller cannot grant itself a
    /// budget, or drain one, by appending the events the balance folds
    /// from.
    #[test]
    fn a_caller_cannot_append_the_budget_kinds() {
        let mut s = new_session(Some(10));
        for event in [
            json!({ "kind": "budget_granted", "amount": 1_000_000 }),
            json!({ "kind": "budget_reserved", "amount": 5 }),
            json!({ "kind": "budget_refused", "amount": 5, "remaining": 0 }),
            json!({ "kind": "budget_spent", "amount": 5 }),
        ] {
            let err = s.append(obj(event.clone())).expect_err("kernel-only kind");
            assert!(
                err.reason().contains("kernel only"),
                "{event}: {}",
                err.reason()
            );
        }

        assert_eq!(s.remaining(), Some(10), "no forged event moved the balance");
        assert_eq!(ledger(&s).len(), 1, "nothing was recorded");
        assert_eq!(folded(&s), s.remaining());
    }

    /// The session's boundaries are the kernel's alone: a caller cannot
    /// hand-append either one, so a stream cannot claim an opening it never
    /// had or an ending it never reached.  The refusal leaves the session
    /// open and the log untouched.
    #[test]
    fn a_caller_cannot_append_the_session_boundary_kinds() {
        let mut s = new_session(Some(100));
        for event in [
            json!({ "kind": "session_opened" }),
            json!({ "kind": "session_closed", "reason": "carried over" }),
        ] {
            let err = s.append(obj(event.clone())).expect_err("kernel-only kind");
            assert!(
                err.reason().contains("kernel only"),
                "{event}: {}",
                err.reason()
            );
        }

        assert!(!s.is_closed(), "a refused append ended the session");
        assert_eq!(s.len().expect("len"), 2, "nothing was recorded");
        assert_eq!(s.append(obj(json!({ "kind": "note" }))), Ok(3));
        assert_eq!(s.spend(10), Ok(Some(90)));
    }

    /// Only `close` records `session_closed`, and it records exactly one:
    /// the flag and the event move together, so the log and the state
    /// cannot disagree.
    #[test]
    fn only_close_records_session_closed() {
        let mut s = new_session(Some(100));
        s.append(obj(json!({ "kind": "note" }))).expect("append");
        assert!(
            !s.events(0)
                .expect("events")
                .iter()
                .any(|e| kind_of(e) == KIND_SESSION_CLOSED),
            "nothing but close writes the boundary"
        );

        s.close(Some("done")).expect("close");
        assert!(s.is_closed());

        let closed: Vec<Value> = s
            .events(0)
            .expect("events")
            .into_iter()
            .filter(|e| kind_of(e) == KIND_SESSION_CLOSED)
            .collect();
        assert_eq!(closed.len(), 1, "exactly one boundary: {closed:?}");
        assert_eq!(closed[0]["reason"], json!("done"));
        assert_eq!(
            s.append(obj(json!({ "kind": "note" })))
                .expect_err("append after close")
                .reason(),
            "session is closed"
        );
    }

    /// The record and the account are separate readings of the same
    /// session: the response is in the history and in `usage`, and the
    /// balance is exactly what was granted, because nobody reserved
    /// anything.
    #[test]
    fn a_recorded_response_is_in_the_history_without_being_charged() {
        let mut s = new_session(Some(100));
        s.append(response(30)).expect("recorded");

        assert_eq!(s.remaining(), Some(100));
        assert!(!s.exhausted());

        let recorded = s.events(3).expect("events").pop().expect("llm_response");
        assert_eq!(kind_of(&recorded), "llm_response");
        assert_eq!(recorded["stop_reason"], json!("end_turn"));
        assert_eq!(s.view(VIEW_USAGE, None).expect("usage")["input_tokens"], 30);
        assert_eq!(folded(&s), Some(100), "the ledger recorded no consumption");
    }

    /// The beat belongs to the layer above: the kernel never mints one, so
    /// an event that declares none carries none, and one that declares a
    /// beat carries exactly the string it was given — on any kind, and
    /// repeated across the facts of one beat without the kernel objecting.
    #[test]
    fn beats_are_the_callers_word_and_the_kernel_adds_none() {
        let mut s = new_session(None);

        let seq = s.append(response(1)).expect("an undeclared beat");
        let bare = s.events(seq).expect("events").pop().expect("response");
        assert_eq!(
            bare.get(FIELD_BEAT),
            None,
            "the kernel must not invent a beat: {bare}"
        );

        for event in [
            json!({
                "kind": "llm_response", "beat": "b-1", "content": [],
                "usage": { "input_tokens": 1 }
            }),
            json!({
                "kind": "tool_call", "beat": "b-1", "call_id": "c1",
                "name": "sh", "args": {}
            }),
            json!({
                "kind": "tool_result", "beat": "b-1", "call_id": "c1",
                "ok": true, "result": "ok"
            }),
            json!({ "kind": "llm_call_failed", "beat": "b-1", "error": "boom" }),
        ] {
            let seq = s.append(obj(event.clone())).expect("declared beat");
            let recorded = s.events(seq).expect("events").pop().expect("recorded");
            assert_eq!(recorded[FIELD_BEAT], json!("b-1"), "{event}");
        }

        // A non-string beat is the one thing refused, on any kind.
        let err = s
            .append(obj(json!({ "kind": "note", "beat": 1 })))
            .expect_err("a numbered beat");
        assert!(err.reason().contains("beat must be a string"), "{err}");
    }

    #[test]
    fn a_closed_session_records_nothing() {
        let mut s = new_session(Some(100));
        s.close(None).expect("close");

        let err = s.append(response(10)).expect_err("closed session");
        assert_eq!(err.reason(), "session is closed");

        assert_eq!(
            s.len().expect("len"),
            3,
            "session_opened + budget_granted + session_closed only"
        );
        assert_eq!(s.remaining(), Some(100), "nothing was consumed");
    }

    /// The budget stops a session *before* it spends, not after: a
    /// reservation the balance cannot cover is refused, and the call it was
    /// for never happens.  This replaces the old contract, where the budget
    /// was a flag that only stood up once a recorded call had already used
    /// the allowance up — by which time the spending was done.
    #[test]
    fn the_budget_refuses_before_the_call_rather_than_flagging_after_it() {
        let mut s = new_session(Some(10));

        /// How many model responses the log holds.
        fn responses(s: &Session) -> usize {
            s.events(0)
                .expect("events")
                .iter()
                .filter(|e| kind_of(e) == KIND_LLM_RESPONSE)
                .count()
        }

        // The estimate fits, so the beat proceeds and records its response.
        assert_eq!(s.reserve(10), Ok(true));
        s.append(response(25)).expect("recorded");
        assert_eq!(s.remaining(), Some(0), "the reservation took it all");
        assert!(s.exhausted());

        // The next beat asks first and is turned away, so no second
        // response is recorded: the caller never made the call.
        assert_eq!(s.reserve(1), Ok(false));
        assert_eq!(responses(&s), 1, "the refused beat made no call");
        assert_eq!(s.remaining(), Some(0));
        assert_eq!(folded(&s), s.remaining());

        // The kernel still does not police it: a caller that ignores the
        // refusal can append anyway, and the history says that it did.
        s.append(response(5)).expect("recorded");
        assert_eq!(responses(&s), 2, "stopping is the caller's decision");
    }

    #[test]
    fn without_a_budget_a_call_reports_no_remaining_and_is_never_exhausted() {
        let mut s = new_session(None);
        s.append(response(9_000)).expect("recorded");
        assert_eq!(s.remaining(), None);
        assert!(!s.exhausted());

        // No budget, no ledger: reserve always grants, spend does nothing,
        // and neither leaves a trace.
        assert_eq!(s.reserve(1_000_000), Ok(true));
        assert_eq!(s.spend(1_000_000), Ok(None));
        assert!(ledger(&s).is_empty(), "a run with no quota keeps no ledger");
        assert_eq!(s.remaining(), None);
        assert!(!s.exhausted());
    }

    #[test]
    fn views_stay_readable_and_correct_after_close() {
        let mut s = new_session(None);
        s.append(response(9)).expect("recorded");
        let before = s.view(VIEW_USAGE, None).expect("usage");
        s.close(None).expect("close");
        let after = s.view(VIEW_USAGE, None).expect("usage after close");

        // `session_closed` costs nothing, so the totals are unchanged even
        // though the history grew — and `at_seq` says the fold saw it.
        assert_eq!(before["input_tokens"], after["input_tokens"]);
        assert_eq!(before["model_calls"], after["model_calls"]);
        assert_eq!(before["at_seq"], json!(2));
        assert_eq!(after["at_seq"], json!(3));

        assert_eq!(s.len().expect("len"), 3);
        assert_eq!(
            kind_of(&s.events(0).expect("events")[2]),
            KIND_SESSION_CLOSED
        );
    }

    /// `open_on` on a durable backend records the session's owner on the
    /// `session_opened` boundary, so resume can recover it from the log
    /// alone.
    #[cfg(feature = "sqlite")]
    #[test]
    fn open_on_records_the_owner_on_session_opened() {
        use crate::knl::SqliteEventStore;

        let store = SqliteEventStore::open_in_memory("owner-stream").expect("open");
        let s = Session::open_on("user-7".to_string(), Some(grant(100)), Box::new(store))
            .expect("open");

        let events = s.events(0).expect("events");
        let opened = events.first().expect("session_opened");
        assert_eq!(kind_of(opened), KIND_SESSION_OPENED);
        assert_eq!(
            opened.get("owner").and_then(Value::as_str),
            Some("user-7"),
            "owner rides on session_opened: {opened}"
        );
        assert_eq!(s.owner(), "user-7");

        // The grant is durable too, as its own event.
        assert_eq!(kind_of(&events[1]), KIND_BUDGET_GRANTED);
        assert_eq!(events[1]["amount"], json!(100));
    }

    /// Resume re-folds a persisted SQLite stream: the owner and the
    /// *balance* come back from the log, because every move of the balance
    /// is in it.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_restores_the_owner_and_the_folded_balance() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "resume-stream";

        // A durable session: two beats that reserved and settled.
        let before_close = {
            let store = SqliteEventStore::open(&path, stream).expect("open");
            let mut s = Session::open_on("user-42".to_string(), Some(grant(100)), Box::new(store))
                .expect("open");
            assert_eq!(s.reserve(30), Ok(true));
            s.append(response(30)).expect("first response");
            s.append(obj(json!({ "kind": "msg_user", "content": "more" })))
                .expect("msg_user");
            assert_eq!(s.reserve(15), Ok(true));
            s.append(response(20)).expect("second response");
            s.spend(5).expect("the second call overran its estimate");
            assert_eq!(s.remaining(), Some(50), "100 - 30 - 15 - 5");
            assert_eq!(folded(&s), s.remaining());
            s.remaining()
        }; // dropped: the connection closes, the log persists.

        // Reopen the same stream and resume — no new session_opened is
        // written, and no new grant either.
        let store = SqliteEventStore::open(&path, stream).expect("reopen");
        let mut resumed = Session::resume(None, Box::new(store)).expect("resume");

        assert_eq!(
            resumed.owner(),
            "user-42",
            "owner restored from session_opened"
        );
        assert_eq!(
            resumed.remaining(),
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
            resumed.len().expect("len"),
            8,
            "session_opened + granted + reserved + response + msg_user \
             + reserved + response + spent — and nothing from resume itself"
        );

        // The usage view re-folds correctly from the reopened store.
        let usage = resumed.view(VIEW_USAGE, None).expect("usage");
        assert_eq!(usage["model_calls"], json!(2));
        assert_eq!(usage["input_tokens"], json!(50));

        // The ledger continues: the next reservation comes off the restored
        // balance, and what the resumed session records is its own.
        assert_eq!(resumed.reserve(5), Ok(true));
        let seq = resumed.append(response(5)).expect("third response");
        let recorded = resumed
            .events(seq)
            .expect("events")
            .pop()
            .expect("llm_response");
        assert_eq!(kind_of(&recorded), KIND_LLM_RESPONSE);
        assert_eq!(resumed.remaining(), Some(45), "5 reserved off the 50");
        assert_eq!(folded(&resumed), resumed.remaining());
    }

    /// A `grant` on resume is the owner allowing *more*: it is recorded and
    /// added to what the log left, rather than replacing it.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_with_a_grant_records_it_and_raises_the_balance() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "regrant-stream";

        {
            let store = SqliteEventStore::open(&path, stream).expect("open");
            let mut s = Session::open_on("user-9".to_string(), Some(grant(100)), Box::new(store))
                .expect("open");
            assert_eq!(s.reserve(80), Ok(true));
            assert_eq!(s.remaining(), Some(20));
        }

        let store = SqliteEventStore::open(&path, stream).expect("reopen");
        let mut resumed = Session::resume(
            Some(BudgetGrant {
                amount: 50,
                tag: Some("tokens".to_string()),
                desc: Some("a little more".to_string()),
            }),
            Box::new(store),
        )
        .expect("resume");

        assert_eq!(resumed.remaining(), Some(70), "20 left + 50 granted");
        assert_eq!(folded(&resumed), resumed.remaining());

        let moves = ledger(&resumed);
        let kinds: Vec<&str> = moves.iter().map(kind_of).collect();
        assert_eq!(
            kinds,
            vec![
                KIND_BUDGET_GRANTED,
                KIND_BUDGET_RESERVED,
                KIND_BUDGET_GRANTED
            ],
            "the second grant is a recorded fact"
        );
        assert_eq!(moves[2]["amount"], json!(50));
        assert_eq!(moves[2]["desc"], json!("a little more"));

        // And the resumed run spends against the raised balance.
        assert_eq!(resumed.reserve(70), Ok(true));
        assert_eq!(resumed.reserve(1), Ok(false));
        assert_eq!(resumed.remaining(), Some(0));
        assert_eq!(folded(&resumed), resumed.remaining());
    }

    /// An older log with no `owner` on `session_opened` resumes as [`ANON`]
    /// rather than failing — the field is a later addition.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_falls_back_to_anon_when_the_log_has_no_owner() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "legacy-stream";

        // Write a session_opened with no owner field, as an older build
        // would.
        {
            let mut store = SqliteEventStore::open(&path, stream).expect("open");
            store
                .append(kernel_event(KIND_SESSION_OPENED))
                .expect("session_opened");
        }

        let store = SqliteEventStore::open(&path, stream).expect("reopen");
        let resumed = Session::resume(None, Box::new(store)).expect("resume");
        assert_eq!(resumed.owner(), ANON);
        assert_eq!(resumed.remaining(), None, "resumed without a budget cap");
    }

    /// Resume restores the *scope*, not just a fresh one: the id and the
    /// owner come back off `session_opened`, so the session continues under
    /// the authority the log says it opened with, and the ledger it goes on
    /// writing names that same scope.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_restores_the_scope_id_and_owner_from_the_log() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "scope-resume-stream";

        let opened_scope = {
            let store = SqliteEventStore::open(&path, stream).expect("open");
            let mut s = Session::open_on("user-11".to_string(), Some(grant(100)), Box::new(store))
                .expect("open");
            assert_eq!(s.reserve(40), Ok(true));
            s.scope_id().to_string()
        };

        let store = SqliteEventStore::open(&path, stream).expect("reopen");
        let mut resumed = Session::resume(None, Box::new(store)).expect("resume");
        assert_eq!(
            resumed.scope_id(),
            opened_scope,
            "the scope id is restored from session_opened, not re-issued"
        );
        assert_eq!(resumed.owner(), "user-11");
        assert_eq!(resumed.scope().owner(), "user-11");
        assert_eq!(resumed.remaining(), Some(60), "the balance is the fold's");

        // What the resumed session records goes on naming the same scope.
        assert_eq!(resumed.reserve(10), Ok(true));
        let last = ledger(&resumed).pop().expect("budget_reserved");
        assert_eq!(kind_of(&last), KIND_BUDGET_RESERVED);
        assert_eq!(
            last.get(FIELD_SCOPE_ID).and_then(Value::as_str),
            Some(opened_scope.as_str()),
            "{last}"
        );
    }

    /// An older log with no `scope_id` on `session_opened` resumes under a
    /// fresh kernel-issued one rather than failing — the field is a later
    /// addition, exactly like `owner` above, and a session that predates it
    /// is still a session.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_issues_a_fresh_scope_id_when_the_log_records_none() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "legacy-scope-stream";

        // An old-style opening: a session_opened carrying neither field.
        {
            let mut store = SqliteEventStore::open(&path, stream).expect("open");
            store
                .append(kernel_event(KIND_SESSION_OPENED))
                .expect("session_opened");
        }

        let store = SqliteEventStore::open(&path, stream).expect("reopen");
        let resumed = Session::resume(Some(grant(50)), Box::new(store)).expect("resume");

        // The fallback is visible from both sides: the log says nothing…
        let opened = resumed.events(0).expect("events").remove(0);
        assert_eq!(kind_of(&opened), KIND_SESSION_OPENED);
        assert_eq!(opened.get(FIELD_SCOPE_ID), None, "{opened}");
        assert_eq!(opened.get("owner"), None, "{opened}");
        // …and the resumed session has a real scope all the same.
        assert!(
            !resumed.scope_id().is_empty(),
            "an older log must still resume under a scope"
        );
        assert_eq!(resumed.owner(), ANON, "the sibling fallback");

        // And it is the one everything written from here on names.
        let granted = ledger(&resumed).pop().expect("budget_granted");
        assert_eq!(kind_of(&granted), KIND_BUDGET_GRANTED);
        assert_eq!(
            granted.get(FIELD_SCOPE_ID).and_then(Value::as_str),
            Some(resumed.scope_id()),
            "{granted}"
        );
    }

    /// (Fix 5) Resuming an empty log is a caller error — a mistyped or
    /// nonexistent stream must not fold into an anonymous zero session.
    #[test]
    fn resume_of_an_empty_store_is_a_caller_error_not_an_anon_session() {
        let err = Session::resume(Some(grant(100)), Box::new(MemEventStore::new()))
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
    #[test]
    fn resume_of_a_store_without_an_opening_is_a_caller_error() {
        let mut store = MemEventStore::new();
        store
            .append(obj(json!({ "kind": "note" })))
            .expect("seed a non-opening event");
        let err = Session::resume(None, Box::new(store))
            .expect_err("a log with no opening has no session to resume");
        assert!(
            err.reason().contains("no session to resume"),
            "{}",
            err.reason()
        );
    }

    /// A session is disposable: once its ending is in the log, the stream is
    /// not continued.  What comes after an ending is a new session.
    #[test]
    fn a_closed_stream_is_not_resumed() {
        let mut store = MemEventStore::new();
        store
            .append(obj(json!({ "kind": "session_opened", "owner": "user-5" })))
            .expect("seed the opening");
        store
            .append(obj(json!({ "kind": "budget_granted", "amount": 100 })))
            .expect("seed the grant");
        store
            .append(obj(json!({ "kind": "session_closed", "reason": "done" })))
            .expect("seed the ending");

        let err = Session::resume(None, Box::new(store))
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

    impl EventStore for BusyStore {
        /// A session's appends come through here, so this is where the
        /// competing writer gets in: once, just before the response this
        /// session is about to record.
        fn append(&mut self, event: Map<String, Value>) -> KnlResult<crate::knl::Committed> {
            if !self.injected
                && event.get(FIELD_KIND).and_then(Value::as_str) == Some(KIND_LLM_RESPONSE)
            {
                self.injected = true;
                self.inner
                    .append(obj(json!({ "kind": "sneaked_in" })))
                    .expect("injected concurrent write");
            }
            self.inner.append(event)
        }

        fn append_if(
            &mut self,
            decide: &mut crate::knl::Decision<'_>,
        ) -> KnlResult<Option<crate::knl::Committed>> {
            self.inner.append_if(decide)
        }

        fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Value>> {
            self.inner.read(from_seq, limit)
        }

        fn head(&self) -> KnlResult<Option<u64>> {
            self.inner.head()
        }

        fn len(&self) -> KnlResult<usize> {
            self.inner.len()
        }
    }

    /// An append records a fact and the store orders it: another writer
    /// getting there first does not turn this session's append into a
    /// failure, it only decides where the two land.
    #[test]
    fn an_append_lands_after_a_competing_write_rather_than_being_refused() {
        let store = BusyStore {
            inner: MemEventStore::new(),
            injected: false,
        };
        let mut s =
            Session::open_on("user".to_string(), Some(grant(1000)), Box::new(store)).expect("open");
        assert_eq!(
            s.len().expect("len"),
            2,
            "session_opened + budget_granted so far"
        );

        // The competing write lands at seq 3, so the response lands at 4 —
        // and it lands.
        let seq = s.append(response(10)).expect("an append is not refused");
        assert_eq!(seq, 4, "the seq is where the event really landed");
        assert_eq!(s.len().expect("len"), 4, "both writes are in the log");

        let log = s.events(0).expect("events");
        let kinds: Vec<&str> = log.iter().map(kind_of).collect();
        assert_eq!(
            kinds,
            [
                KIND_SESSION_OPENED,
                KIND_BUDGET_GRANTED,
                "sneaked_in",
                KIND_LLM_RESPONSE
            ],
            "the log interleaves in arrival order"
        );
        assert_eq!(s.remaining(), Some(1000), "an append still charges nothing");
    }

    /// (Fix 5) Resuming a nonexistent SQLite stream is a caller error, not an
    /// anonymous empty session.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_of_a_nonexistent_sqlite_stream_is_a_caller_error() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        // A stream that was never opened as a session: its log is empty.
        let store = SqliteEventStore::open(&path, "ghost-stream").expect("open");
        let err = Session::resume(Some(grant(100)), Box::new(store))
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
    #[cfg(feature = "sqlite")]
    #[test]
    fn two_sessions_on_one_stream_both_append_and_the_log_interleaves() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "interleave-stream";

        // A opens the session on the shared stream: `session_opened` at seq 1
        // and its `budget_granted` at seq 2, so A has seen head 2.
        let store_a = SqliteEventStore::open(&path, stream).expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(1000)), Box::new(store_a))
            .expect("open A");

        // B resumes the SAME stream while it holds only those two, so both
        // handles have seen exactly head 2.  (It resumes before A closes: a
        // closed session is not resumable.)
        let store_b = SqliteEventStore::open(&path, stream).expect("open B");
        let mut b = Session::resume(None, Box::new(store_b)).expect("resume B");
        assert_eq!(b.remaining(), Some(1000), "B resumed on A's ledger");
        assert_eq!(
            (a.len().expect("len"), b.len().expect("len")),
            (2, 2),
            "both see the same two events"
        );

        // A appends, so B's view is now out of date — and B appends anyway.
        assert_eq!(a.append(response(10)).expect("A appends"), 3);
        assert_eq!(
            b.append(response(20)).expect("B appends too"),
            4,
            "B's write lands after A's, rather than being refused"
        );
        // And A, now out of date in its turn, goes on writing.
        assert_eq!(a.append(response(30)).expect("A appends again"), 5);

        // The durable log holds all three, in arrival order.
        let verify = SqliteEventStore::open(&path, stream).expect("reopen to verify");
        let log = verify.read(0, usize::MAX).expect("read log");
        let responses: Vec<u64> = log
            .iter()
            .filter(|e| kind_of(e) == KIND_LLM_RESPONSE)
            .map(seq_of)
            .collect();
        assert_eq!(responses, [3, 4, 5], "every append landed, in order");
    }

    /// (Concurrency) The invariant that *is* a decision: two handles on one
    /// stream, ten granted, each asking for six.  The decision is taken inside
    /// the store, against the ledger as it stands there, so exactly one is
    /// allowed — and the fold says four, not minus two.
    #[cfg(feature = "sqlite")]
    #[test]
    fn two_sessions_cannot_both_reserve_the_same_allowance() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "reserve-race-stream";

        let store_a = SqliteEventStore::open(&path, stream).expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(10)), Box::new(store_a))
            .expect("open A");
        let store_b = SqliteEventStore::open(&path, stream).expect("open B");
        let mut b = Session::resume(None, Box::new(store_b)).expect("resume B");
        assert_eq!(b.remaining(), Some(10), "both see the whole grant");
        assert_eq!(a.remaining(), Some(10));

        // A takes six.  B still believes it has ten — and is refused all the
        // same, because the balance it is measured against is the one in the
        // store, not the one it cached.
        assert_eq!(a.reserve(6), Ok(true), "the first reservation fits");
        assert_eq!(b.reserve(6), Ok(false), "the second does not");
        assert_eq!(b.remaining(), Some(4), "B's balance is the ledger's");
        assert_eq!(a.remaining(), Some(4), "and so is A's");

        // The ledger is the answer: 10 granted − 6 reserved = 4, with the
        // refusal recorded and moving nothing.
        let verify = SqliteEventStore::open(&path, stream).expect("reopen to verify");
        let log = verify.read(0, usize::MAX).expect("read log");
        assert_eq!(fold_balance(&log), Some(4), "no allowance was taken twice");
        let moves: Vec<&str> = log
            .iter()
            .map(kind_of)
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
        assert_eq!(refused["amount"], json!(6));
        assert_eq!(refused["remaining"], json!(4), "what there really was");
    }

    /// (Concurrency) "Closed" is the handle's, and the log records what
    /// arrives after it.  Three handles, one close: the two that never saw it
    /// go on writing, and their writes land *after* the `session_closed` —
    /// which is the fact an audit is reading for, and would be gone if the
    /// store had refused them.  A second handle closing writes a second
    /// ending, because that is what happened.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_close_is_the_handles_and_the_log_records_what_arrives_after_it() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "close-race-stream";

        let store_a = SqliteEventStore::open(&path, stream).expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(100)), Box::new(store_a))
            .expect("open A");
        // Both resume while the stream is open — a closed one is not
        // resumable — so both hold `closed = false` across A's close.
        let store_b = SqliteEventStore::open(&path, stream).expect("open B");
        let mut b = Session::resume(None, Box::new(store_b)).expect("resume B");
        let store_c = SqliteEventStore::open(&path, stream).expect("open C");
        let mut c = Session::resume(None, Box::new(store_c)).expect("resume C");

        a.close(Some("done")).expect("A closes");
        assert!(a.is_closed());
        assert!(!b.is_closed(), "B's flag is its own and has not moved");
        assert!(!c.is_closed());

        // B writes, and the write lands: the store serializes appends, it does
        // not adjudicate them.
        assert_eq!(
            b.append(obj(json!({ "kind": "note" }))),
            Ok(4),
            "an append after another handle's close is recorded"
        );
        assert!(!b.is_closed(), "landing a write closed nothing");

        // C's budget moves are decided on the balance alone — 100 granted,
        // nothing spent, so both go through.
        assert_eq!(c.reserve(5), Ok(true), "the ledger covers it");
        assert_eq!(c.spend(10), Ok(Some(85)), "100 − 5 − 10, folded in the tx");
        assert!(!c.is_closed());

        // B closing writes a *second* ending; A closing again writes nothing,
        // because A's own flag is set.
        b.close(Some("late")).expect("B closes");
        a.close(Some("again")).expect("A is idempotent per handle");

        let verify = SqliteEventStore::open(&path, stream).expect("reopen to verify");
        let log = verify.read(0, usize::MAX).expect("read log");
        let kinds: Vec<&str> = log.iter().map(kind_of).collect();
        assert_eq!(
            kinds,
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

        let endings: Vec<&Value> = log
            .iter()
            .filter(|event| kind_of(event) == KIND_SESSION_CLOSED)
            .collect();
        assert_eq!(endings.len(), 2, "two handles closed, two endings recorded");
        assert_eq!(endings[0]["reason"], json!("done"));
        assert_eq!(endings[1]["reason"], json!("late"));
        assert_eq!(
            fold_balance(&log),
            Some(85),
            "the ledger is what the moves that landed add up to"
        );
    }

    /// (Concurrency) A settlement reads its answer back off the ledger, so it
    /// is exact on a stream two handles write to: B settles against what the
    /// log says, not against a number it was holding before A spent.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_settlement_reads_its_answer_back_off_the_ledger() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "spend-race-stream";

        let store_a = SqliteEventStore::open(&path, stream).expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(100)), Box::new(store_a))
            .expect("open A");
        let store_b = SqliteEventStore::open(&path, stream).expect("open B");
        let mut b = Session::resume(None, Box::new(store_b)).expect("resume B");
        assert_eq!((a.remaining(), b.remaining()), (Some(100), Some(100)));

        assert_eq!(a.spend(30), Ok(Some(70)), "A settles 30 of the 100");
        assert_eq!(
            b.remaining(),
            Some(70),
            "B wrote nothing and still reads A's settlement off the ledger"
        );

        // B's own settlement measures against the ledger — 100 − 30 − 20 —
        // rather than subtracting 20 from a number it was holding.
        assert_eq!(b.spend(20), Ok(Some(50)), "both settlements are in it");
        assert_eq!(b.remaining(), Some(50));

        let verify = SqliteEventStore::open(&path, stream).expect("reopen to verify");
        let log = verify.read(0, usize::MAX).expect("read log");
        assert_eq!(fold_balance(&log), Some(50), "the balance is the fold");
        let moves: Vec<&str> = log
            .iter()
            .map(kind_of)
            .filter(|kind| kind.starts_with("budget_"))
            .collect();
        assert_eq!(
            moves,
            [KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT, KIND_BUDGET_SPENT],
            "one grant and two settlements"
        );

        // A settlement never refuses: it floors at zero, as the fold does.
        assert_eq!(a.spend(1_000), Ok(Some(0)));
        assert_eq!(b.spend(1), Ok(Some(0)));
        let verify = SqliteEventStore::open(&path, stream).expect("reopen to verify");
        assert_eq!(
            fold_balance(&verify.read(0, usize::MAX).expect("read log")),
            Some(0),
            "the ledger floors at zero rather than going into debt"
        );
    }

    /// (Concurrency) The balance is the ledger and nothing else, so a handle
    /// that has written nothing at all still reports what the other one
    /// spent: `B` never calls a write in this test, and every answer it gives
    /// comes from folding the stream it shares with `A`.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_handle_that_wrote_nothing_reports_what_the_other_spent() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "shared-balance-stream";

        let store_a = SqliteEventStore::open(&path, stream).expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(100)), Box::new(store_a))
            .expect("open A");
        let store_b = SqliteEventStore::open(&path, stream).expect("open B");
        // Not `mut`: reading a balance is a read, and B does nothing else.
        let b = Session::resume(None, Box::new(store_b)).expect("resume B");
        assert_eq!(b.remaining(), Some(100), "both start on the same ledger");

        assert_eq!(a.spend(30), Ok(Some(70)), "A settles 30");
        assert_eq!(
            b.remaining(),
            Some(70),
            "B sees the settlement it did not make"
        );

        assert_eq!(a.reserve(20), Ok(true), "A reserves 20");
        assert_eq!(b.remaining(), Some(50), "and the reservation too");
        assert!(!b.exhausted());

        // Reading twice over a stream that has not moved repeats the fold's
        // answer rather than drifting from it.
        assert_eq!(b.remaining(), Some(50), "a second read is the same read");

        assert_eq!(a.spend(1_000), Ok(Some(0)), "A overspends");
        assert_eq!(b.remaining(), Some(0), "the floor is the ledger's");
        assert!(b.exhausted());

        // And the log is the whole of the story: nothing B holds was needed.
        let verify = SqliteEventStore::open(&path, stream).expect("reopen to verify");
        assert_eq!(
            fold_balance(&verify.read(0, usize::MAX).expect("read log")),
            b.remaining()
        );
    }

    /// A test-local `1 → 2` step, standing in for a real one: it renames the
    /// two kinds a hypothetical earlier shape used and marks what it
    /// produced.  The kernel chain is empty until the first release, so the
    /// seam is exercised with a chain the test owns — wrapped round the
    /// backend before the session is handed it, so the session's own (empty)
    /// wrap sits outside it as an identity.
    #[cfg(feature = "sqlite")]
    struct RenameLegacyKinds;

    #[cfg(feature = "sqlite")]
    impl crate::knl::Upcaster for RenameLegacyKinds {
        fn upcast(&self, mut event: Value) -> Value {
            use crate::knl::SCHEMA_VERSION_FIELD;

            // Already at the shape this step produces, or not an object at
            // all: unchanged.  An upcaster is total and infallible.
            let version = event
                .get(SCHEMA_VERSION_FIELD)
                .and_then(Value::as_u64)
                .unwrap_or(1);
            if version >= 2 {
                return event;
            }
            let Some(map) = event.as_object_mut() else {
                return event;
            };
            let renamed = match map.get(FIELD_KIND).and_then(Value::as_str) {
                Some("legacy_opened") => Some(KIND_SESSION_OPENED),
                Some("legacy_response") => Some(KIND_LLM_RESPONSE),
                _ => None,
            };
            if let Some(kind) = renamed {
                map.insert(FIELD_KIND.to_string(), Value::from(kind));
            }
            map.insert(SCHEMA_VERSION_FIELD.to_string(), Value::from(2_u64));
            event
        }
    }

    /// (Upcasting seam) Every read a session makes goes through the chain
    /// wrapped round its backend — the restore fold a resume takes, `events`,
    /// the `usage` view and the balance fold alike — while the rows on disk
    /// keep the shape they were written in.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_session_reads_every_path_through_the_upcaster_seam() {
        use crate::knl::{
            SqliteEventStore, Upcaster, CURRENT_SCHEMA_VERSION, SCHEMA_VERSION_FIELD,
        };
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "seam-stream";

        // Seeded under the older kind names, through the store itself: the
        // rows are ordinary appends, so they carry the version they were
        // written under.
        {
            let mut store = SqliteEventStore::open(&path, stream).expect("open");
            let mut opened = kernel_event("legacy_opened");
            opened.insert(FIELD_OWNER.to_string(), Value::from("user-3"));
            opened.insert(
                FIELD_SCOPE_ID.to_string(),
                Value::from("scope-from-the-log"),
            );
            store.append(opened).expect("the opening");
            store
                .append(obj(
                    json!({ "kind": "budget_granted", "amount": 100, "tag": "tokens" }),
                ))
                .expect("the grant");
            store
                .append(obj(json!({
                    "kind": "legacy_response", "beat": "b-1",
                    "content": [{ "type": "text", "text": "ok" }],
                    "usage": { "input_tokens": 7 }
                })))
                .expect("the response");
        }

        let chain: Vec<Arc<dyn Upcaster>> = vec![Arc::new(RenameLegacyKinds)];
        let seamed = UpcastingEventStore::new(
            Box::new(SqliteEventStore::open(&path, stream).expect("reopen")),
            chain,
        );
        let mut resumed = Session::resume(None, Box::new(seamed)).expect("resume through the seam");

        // The restore read went through the chain: the opening was only a
        // `session_opened` after the step, and the scope came off it.
        assert_eq!(resumed.owner(), "user-3", "the owner the step revealed");
        assert_eq!(resumed.scope_id(), "scope-from-the-log");
        assert_eq!(
            resumed.grant().and_then(|g| g.tag.as_deref()),
            Some("tokens"),
            "and the grant with it"
        );

        // …and so do `events`, the view fold and the balance fold.
        let log = resumed.events(0).expect("events");
        let kinds: Vec<&str> = log.iter().map(kind_of).collect();
        assert_eq!(
            kinds,
            [KIND_SESSION_OPENED, KIND_BUDGET_GRANTED, KIND_LLM_RESPONSE],
            "every read is projected"
        );
        let usage = resumed.view(VIEW_USAGE, None).expect("usage");
        assert_eq!(
            usage["model_calls"],
            json!(1),
            "the view folded the projected kind: {usage}"
        );
        assert_eq!(usage["input_tokens"], json!(7));
        assert_eq!(resumed.remaining(), Some(100), "the balance folds too");

        // The stored rows were not rewritten: read them without the seam and
        // the old names are still there, under the version they were written
        // with.
        let raw = SqliteEventStore::open(&path, stream).expect("reopen raw");
        let stored = raw.read(0, usize::MAX).expect("read raw");
        assert_eq!(kind_of(&stored[0]), "legacy_opened", "{}", stored[0]);
        assert_eq!(kind_of(&stored[2]), "legacy_response", "{}", stored[2]);
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
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_closed_stream_seen_through_the_seam_is_still_refused() {
        use crate::knl::{SqliteEventStore, Upcaster};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "seam-closed-stream";

        {
            let mut store = SqliteEventStore::open(&path, stream).expect("open");
            store
                .append(kernel_event("legacy_opened"))
                .expect("the opening");
            store
                .append(obj(json!({ "kind": "session_closed", "reason": "done" })))
                .expect("the ending");
        }

        let chain: Vec<Arc<dyn Upcaster>> = vec![Arc::new(RenameLegacyKinds)];
        let seamed = UpcastingEventStore::new(
            Box::new(SqliteEventStore::open(&path, stream).expect("reopen")),
            chain,
        );
        let err = Session::resume(None, Box::new(seamed))
            .expect_err("a stream that ended must not be resumed");
        assert!(
            err.reason().contains("session is closed"),
            "{}",
            err.reason()
        );
    }
}
