//! K5 — the run scope.
//!
//! A session binds one history, one budget and the projection caches
//! together and is the only handle on kernel state.  All of it lives in
//! the value: two sessions share nothing.
//!
//! # The session *is* the scope
//!
//! A session holds only its own events, so ownership is not a per-event
//! question: the session carries one `owner` — a real principal id, or the
//! reserved [`ANON`] / [`SYSTEM`] id — and it is *total* (never `Option`,
//! never a "kernel vs caller" flag).  System-originated work is a session
//! whose owner is [`SYSTEM`]; unspecified is [`ANON`].  The policy layer
//! above the kernel reads [`Session::owner`] to authorize; the kernel
//! itself does not branch on it.
//!
//! # One append
//!
//! There is a single write path.  [`Session::append`] validates, stamps
//! the kernel-owned `seq` / `epoch_ms`, and pushes.  When the event is a
//! `model_response` it also *numbers* it: the kernel assigns the `turn`
//! from its own count, overwriting any the caller supplied.
//!
//! It does not charge.  An append is a record of something that happened,
//! and the budget is a permission asked for *before* something happens —
//! [`Session::reserve`], which the layer that knows what a call costs
//! calls, then settles with [`Session::spend`].  Folding the two together
//! is what turned the budget into a flag that only stands up once the
//! allowance is already gone; the counter and the `usage` projection are
//! independent readings and neither is the other's ledger.
//!
//! # The budget is in the log
//!
//! Every move of the balance is an event first — `budget_granted` when an
//! owner allows, `budget_reserved` / `budget_refused` at the decision
//! point, `budget_spent` at the settlement — written through the same
//! append, before the counter moves.  So the balance is not session-local
//! state that dies with the process: [`Session::resume`] recovers it by
//! folding the ledger, and any reader can check the counter against
//! [`super::budget::fold_balance`].  Those kinds are the kernel's alone to
//! write ([`super::event::is_kernel_only`]) — [`Session::append`] refuses
//! them from a caller, because writing one is moving the account.
//!
//! The kernel writes the run's own boundaries through the same append:
//! [`Session::new`] appends `run_started` and [`Session::close`] appends
//! `run_finished`, so a run is bracketed in the history whether or not the
//! shell remembers to say so.  After a close, `append` / `spend` are errors
//! while reads keep working — the record outlives the run.
//!
//! The scope itself is the `closed` flag, not the `run_finished` event.
//! An event is a record of something; what ends the run is [`close`], and
//! a caller that appends a `run_finished` of its own has written a line
//! in the history and changed no state.
//!
//! [`close`]: Session::close

use serde_json::{Map, Value};

use super::budget::{self, fold_balance, last_grant, BudgetGrant};
use super::event::{
    is_kernel_only, kernel_event, kind_of, seq_of, FIELD_AMOUNT, FIELD_DESC, FIELD_DETAIL,
    FIELD_KIND, FIELD_REASON, FIELD_REMAINING, FIELD_TAG, FIELD_TURN, KIND_BUDGET_GRANTED,
    KIND_BUDGET_REFUSED, KIND_BUDGET_RESERVED, KIND_BUDGET_SPENT, KIND_MODEL_RESPONSE,
    KIND_RUN_FINISHED, KIND_RUN_STARTED,
};
use super::event_store::{EventStore, MemEventStore, UpcastingEventStore};
use super::projection::{tail_count, Views, VIEW_TAIL, VIEW_USAGE};
use super::{projection, Budget, KnlError, KnlResult};

/// Reason recorded by `close()` when the caller does not give one.
pub const DEFAULT_CLOSE_REASON: &str = "closed";
/// Reason recorded when a run scope ended on its own: the Lua `<close>`
/// variable holding the session went out of scope with no error.
pub const CLOSE_REASON_SCOPE_EXIT: &str = "scope_exit";
/// Reason recorded when a run scope ended because the block raised: the
/// message goes to [`FIELD_DETAIL`], never into the reason.
pub const CLOSE_REASON_ERROR: &str = "error";
/// Reason recorded by the backstop: the handle died without anyone closing
/// it, so the boundary is written where the value is dropped.
pub const CLOSE_REASON_DROPPED: &str = "dropped";

/// Reserved owner: no principal was named when the session opened.
pub const ANON: &str = "anon";
/// Reserved owner: the session belongs to the system itself.
pub const SYSTEM: &str = "system";

/// Payload field the kernel records on `run_started` so a later
/// [`Session::resume`] can recover the session's `owner` from the log.
///
/// `run_started` is an open-shape reserved kind (no required fields), so
/// carrying this extra field needs no change to the event validator.
const FIELD_OWNER: &str = "owner";

/// A `budget_granted` event for `grant`.
///
/// Only what the owner said is written — an absent `tag` is an absent
/// field, not a null — so the record carries the grant and nothing more.
fn granted_event(grant: &BudgetGrant) -> Map<String, Value> {
    let mut event = kernel_event(KIND_BUDGET_GRANTED);
    event.insert(FIELD_AMOUNT.to_string(), Value::from(grant.amount));
    if let Some(tag) = grant.tag.as_ref() {
        event.insert(FIELD_TAG.to_string(), Value::from(tag.clone()));
    }
    if let Some(desc) = grant.desc.as_ref() {
        event.insert(FIELD_DESC.to_string(), Value::from(desc.clone()));
    }
    event
}

/// A `budget_reserved` / `budget_spent` event for `amount`, tagged with the
/// grant's unit so the ledger reads without a join.
fn budget_move_event(kind: &str, amount: i64, tag: Option<&str>) -> Map<String, Value> {
    let mut event = kernel_event(kind);
    event.insert(FIELD_AMOUNT.to_string(), Value::from(amount));
    if let Some(tag) = tag {
        event.insert(FIELD_TAG.to_string(), Value::from(tag.to_string()));
    }
    event
}

/// One run scope.
pub struct Session {
    /// Run-correlation id, unique per session.
    id: String,
    /// Whose scope this is: a real principal id, or [`ANON`] / [`SYSTEM`].
    /// Total — never absent, read by the policy layer above the kernel.
    owner: String,
    /// K1 append-only history, held behind the event-store SPI so the
    /// backend can be the in-memory store or the durable SQLite one.
    store: Box<dyn EventStore>,
    /// K4 budget counter.
    budget: Budget,
    /// The grant this run opened with, kept for its words: a refused
    /// [`Session::reserve`] hands the `tag` back so a caller can say which
    /// allowance stopped it.  `None` when the run has no budget.
    grant: Option<BudgetGrant>,
    /// Cached projection folds (derived, never authoritative).
    views: Views,
    /// Model responses recorded so far: the turn number's authority.
    turns: u64,
    /// The stream head this session has observed and writes against.
    ///
    /// The compare-and-swap expectation for the next [`append`] — *not* a
    /// fresh read of the store.  A single-writer session wrote every event, so
    /// this always equals the real head and every CAS succeeds.  A second
    /// session on the same stream keeps the head it last saw, so the moment
    /// another writer advances the real head this one goes stale and its next
    /// append fails loud with a head-conflict instead of duplicating a turn.
    ///
    /// [`append`]: Session::append
    head: u64,
    /// Set by `close()`; blocks further `append` / `spend`.
    closed: bool,
}

impl std::fmt::Debug for Session {
    /// The store is a trait object (`dyn EventStore` is not `Debug`), so it
    /// is summarised rather than printed; the fields that identify the run
    /// scope are shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("owner", &self.owner)
            .field("budget", &self.budget)
            .field("turns", &self.turns)
            .field("closed", &self.closed)
            .field("len", &self.store.len())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Open a run for `owner` with an optional budget grant, on the
    /// in-memory store.
    ///
    /// `owner` is total: pass a real principal id, or [`ANON`] / [`SYSTEM`]
    /// for the reserved ones.  The `run_started` event is appended here, so
    /// a fresh session already has one event.
    pub fn new(owner: String, grant: Option<BudgetGrant>) -> KnlResult<Self> {
        Self::open_on(owner, grant, Box::new(MemEventStore::new()))
    }

    /// Open a run for `owner` on a caller-chosen backend `store` (the
    /// in-memory store, or the durable SQLite one).
    ///
    /// Like [`Session::new`] but takes the backend, so the shell decides
    /// whether the log is ephemeral or persisted.  It appends the same
    /// `run_started` boundary, recording the session's `owner` on it so a
    /// later [`Session::resume`] can recover the principal from the log
    /// alone — and the `grant`, so the log says what the owner allowed
    /// this run.  `run_started` is an open-shape reserved kind, so both
    /// extra fields are accepted without any change to the validator.
    pub fn open_on(
        owner: String,
        grant: Option<BudgetGrant>,
        store: Box<dyn EventStore>,
    ) -> KnlResult<Self> {
        // Wrap the chosen backend in the read-time upcasting seam, so every one
        // of this session's reads (view folds, `events`) passes through it by
        // construction.  The chain is empty today (v1), making the decorator a
        // functional no-op; a future upcaster registers at this one wrap site.
        let store: Box<dyn EventStore> = Box::new(UpcastingEventStore::new(store, Vec::new()));
        let mut session = Self {
            id: uuid::Uuid::new_v4().to_string(),
            owner,
            store,
            budget: Budget::new(grant.as_ref().map(|g| g.amount)),
            grant,
            views: Views::default(),
            turns: 0,
            // A fresh run opens on an empty stream: the first append (the
            // `run_started` below) CASes against head 0 (expect empty) and
            // advances the observed head to the seq it lands at.
            head: 0,
            closed: false,
        };
        // The same one path records `run_started` as records everything
        // else, and it CAN fail on a durable backend (a non-empty stream
        // fails the CAS; a busy database exhausts its retries) — a session
        // that could not record its own opening must not exist, so the
        // error surfaces instead of leaving a run with no `run_started`.
        // The append advances `self.head` to the run_started's seq, so a
        // freshly opened session observes the head right after open.
        let mut started = kernel_event(KIND_RUN_STARTED);
        started.insert(FIELD_OWNER.to_string(), Value::from(session.owner.clone()));
        session.append_kernel(started)?;

        // The grant is its own fact, right after the boundary: what the
        // owner allowed is the first entry of the ledger the balance folds
        // from, not a decoration on the run's opening.  A session that
        // could not record its own grant must not exist either — the error
        // surfaces rather than leaving a counter no event accounts for.
        if let Some(grant) = session.grant.clone() {
            session.append_kernel(granted_event(&grant))?;
        }
        Ok(session)
    }

    /// Continue an existing run by re-folding its persisted log.
    ///
    /// The `store` already holds a run's events (a reopened SQLite stream),
    /// so resume does *not* append a new `run_started` — the run already
    /// started.  It reads the whole log once and restores the run's state
    /// from it:
    ///
    /// - `owner` from the first `run_started` event's [`FIELD_OWNER`] field,
    ///   falling back to [`ANON`] for an older log written before it was
    ///   recorded;
    /// - the turn counter from the number of `model_response` events, so the
    ///   kernel-owned numbering continues where it left off;
    /// - the balance, by folding the `budget_*` ledger
    ///   ([`fold_balance`]) — the counter is a cache of the log, so a
    ///   reopened stream carries on with exactly what was left, and the
    ///   grant's words come back with it.
    ///
    /// A `grant` passed here is the owner granting *again*: it is appended
    /// as a new `budget_granted` and raises the restored balance, rather
    /// than replacing it.  Omit it to continue on what is left.  Nothing is
    /// deducted for the previous run's `model_response` usage — an append
    /// never charged, and what was consumed is the `usage` projection's
    /// answer, not the quota's.
    ///
    /// The projection caches start empty and re-fold lazily from the store,
    /// so a resumed session's `usage` view is correct on first read.
    pub fn resume(grant: Option<BudgetGrant>, store: Box<dyn EventStore>) -> KnlResult<Self> {
        // Fallible read: a transient busy read or an undecodable row surfaces
        // here rather than being silently folded into a wrong resumed state.
        let log = store.read(0, usize::MAX)?;

        // Resuming an empty or mistyped stream is a caller error, not an
        // anonymous zero session: a real run always opens with a `run_started`.
        let run_started = log
            .iter()
            .find(|event| kind_of(event) == KIND_RUN_STARTED)
            .ok_or_else(|| {
                KnlError::new("resume: stream has no run to resume (no run_started event)")
            })?;

        // The owner rides on `run_started`; an older log written before the
        // field existed falls back to ANON — but only for a real `run_started`,
        // never for an absent one.
        let owner = run_started
            .get(FIELD_OWNER)
            .and_then(Value::as_str)
            .unwrap_or(ANON)
            .to_string();

        let turns = log
            .iter()
            .filter(|event| kind_of(event) == KIND_MODEL_RESPONSE)
            .count() as u64;

        // The observed head is the log's current head — the last event's seq
        // (`read` returns events in seq order).  A resume errors above on an
        // empty / run_started-less log, so a resumed session's head is a real
        // event's seq; the `0` fallback is unreachable but keeps this total.
        let head = log.last().map(seq_of).unwrap_or(0);

        // The restore read above used the raw backend; the session itself reads
        // through the same upcasting seam `open_on` establishes, so a resumed
        // run's log read, view folds and `events` all pass through it too.  The
        // chain is empty today (v1), so this is a functional no-op.
        let store: Box<dyn EventStore> = Box::new(UpcastingEventStore::new(store, Vec::new()));

        // The balance is what the ledger says, and the grant that named it
        // comes back with it: a resumed run keeps having a budget (and a
        // tag to report) even when the caller grants nothing new.
        let mut session = Self {
            id: uuid::Uuid::new_v4().to_string(),
            owner,
            store,
            budget: Budget::new(fold_balance(&log)),
            grant: last_grant(&log),
            views: Views::default(),
            turns,
            head,
            closed: false,
        };

        // A fresh grant is the owner allowing more, so it is recorded like
        // any other and *adds* to what was left.  Write-ahead: the event
        // lands first, and a failed append leaves the restored balance
        // exactly as the log describes it.
        if let Some(grant) = grant {
            session.append_kernel(granted_event(&grant))?;
            session.budget.grant(grant.amount)?;
            session.grant = Some(grant);
        }
        Ok(session)
    }

    /// The run-correlation id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Adopt `id` as the run-correlation id.
    ///
    /// Used only by the durable Lua bridge so a session and the SQLite
    /// stream it writes to share one id: `open_on` / `resume` mint a fresh
    /// id like `new`, and the bridge overrides it to the stream it opened,
    /// so the id a caller resumes by *is* the stream.  `run_started` records
    /// the `owner`, not the id, so overriding the id does not desync the log.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub(crate) fn adopt_id(&mut self, id: impl Into<String>) {
        self.id = id.into();
    }

    /// Whose scope this is (a principal id, or [`ANON`] / [`SYSTEM`]).
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Record an event, returning its `seq`.  The one write path.
    ///
    /// Any kind is welcome, the reserved ones included, as long as it meets
    /// the shape its kind requires.  The kernel-owned `seq` / `epoch_ms`
    /// are stamped here and overwrite any caller-supplied value.
    ///
    /// A `model_response` is numbered as it is recorded: the kernel assigns
    /// the `turn` from its own count, overwriting whatever the caller put
    /// there — the number is kernel-owned, like `seq`.
    ///
    /// No append moves the budget, this one included.  What a call may
    /// consume is decided before it happens ([`Session::reserve`]) and
    /// settled after ([`Session::spend`]) by the layer that knows what a
    /// call costs; the history records what happened and says nothing about
    /// what was allowed.
    ///
    /// A `run_finished` a caller appends is a line in the history, not a
    /// close: the run scope is the `closed` flag, and only
    /// [`Session::close`] sets it.
    ///
    /// One stream has one live writer: the append is a compare-and-swap on the
    /// head *this session observed* (`self.head`), not a fresh read of the
    /// store.  A single writer wrote every event, so its observed head always
    /// matches the real one and every CAS succeeds — unchanged behaviour.  But
    /// a second session on the same stream keeps the head it last saw: the
    /// moment the first writes, the second's observed head is stale, and its
    /// next append fails loud with a head-conflict rather than reading the
    /// already-advanced head and silently duplicating a turn.
    pub fn append(&mut self, event: Map<String, Value>) -> KnlResult<u64> {
        // The budget ledger is the kernel's to write: the balance is a fold
        // of those events, so accepting one from a caller would be letting
        // it grant itself the quota its owner set.  Refused before the
        // closed check has anything to say about it — the kind is wrong
        // whatever state the run is in.
        let kind = event.get(FIELD_KIND).and_then(Value::as_str).unwrap_or("");
        if is_kernel_only(kind) {
            return Err(KnlError::new(format!(
                "{kind:?} is written by the kernel only (use reserve / spend)"
            )));
        }
        self.append_kernel(event)
    }

    /// [`Session::append`] without the kernel-only guard: the path the
    /// kernel's own writes take.
    ///
    /// Same validation, same stamping, same compare-and-swap — the only
    /// difference is that this one may write the `budget_*` kinds, which is
    /// what makes "the kernel wrote it" a property of the code path rather
    /// than of a field a caller could set.
    fn append_kernel(&mut self, mut event: Map<String, Value>) -> KnlResult<u64> {
        if self.closed {
            return Err(KnlError::new("session is closed"));
        }

        let is_response =
            event.get(FIELD_KIND).and_then(Value::as_str) == Some(KIND_MODEL_RESPONSE);
        if !is_response {
            // CAS against the head this session observed, not a fresh read: a
            // concurrent writer that advanced the real head leaves this
            // expectation stale, so the conflict surfaces instead of appending
            // blind.  The observed head advances only after the write lands.
            let committed = self.store.append_if_head(event, self.head)?;
            self.head = committed.seq;
            return Ok(committed.seq);
        }

        // Kernel-owned turn, assigned before the event is stored so the
        // record carries the number the run will refer to it by.
        let turn = self.next_turn();
        event.insert(FIELD_TURN.to_string(), Value::from(turn));

        // Validate + stamp + push, guarded by the observed head.  A malformed
        // response is rejected, and an observed head another writer has moved
        // past is a head-conflict error — either way the observed head and the
        // turn counter are left untouched, advancing only after the append
        // actually lands.
        let committed = self.store.append_if_head(event, self.head)?;
        self.head = committed.seq;
        self.turns = turn;
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

    /// Whether the history is empty (only before `run_started`, i.e.
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
    /// thing a log must not be silent about.  The event lands *before* the
    /// balance moves, so an append that fails leaves the counter exactly
    /// where the log says it is.
    ///
    /// Always `true`, and recorded nowhere, without a budget: a run with no
    /// quota has no ledger to keep.  Closed sessions refuse, like
    /// [`Session::spend`] — a run that has ended cannot be granted more.
    pub fn reserve(&mut self, amount: i64) -> KnlResult<bool> {
        if self.closed {
            return Err(KnlError::new("session is closed"));
        }
        budget::check_amount(amount)?;
        // No budget, no ledger: nothing to decide and nothing to record.
        let Some(tag) = self.grant.as_ref().map(|g| g.tag.clone()) else {
            return Ok(true);
        };

        let remaining = self.budget.remaining().unwrap_or(0);
        if remaining < amount {
            let mut event = budget_move_event(KIND_BUDGET_REFUSED, amount, tag.as_deref());
            event.insert(FIELD_REMAINING.to_string(), Value::from(remaining));
            self.append_kernel(event)?;
            return Ok(false);
        }

        self.append_kernel(budget_move_event(
            KIND_BUDGET_RESERVED,
            amount,
            tag.as_deref(),
        ))?;
        self.budget.reserve(amount)
    }

    /// Settle `amount` against the budget, returning the new balance.
    ///
    /// The after-the-fact half of [`Session::reserve`]: what a call really
    /// cost, beyond what was reserved for it.  Recorded as `budget_spent`
    /// before the balance moves, on the same write-ahead rule — and
    /// recorded nowhere, moving nothing, when there is no budget.
    pub fn spend(&mut self, amount: i64) -> KnlResult<Option<i64>> {
        if self.closed {
            return Err(KnlError::new("session is closed"));
        }
        budget::check_amount(amount)?;
        let Some(tag) = self.grant.as_ref().map(|g| g.tag.clone()) else {
            return Ok(None);
        };
        self.append_kernel(budget_move_event(KIND_BUDGET_SPENT, amount, tag.as_deref()))?;
        self.budget.spend(amount)
    }

    /// The grant this run opened (or resumed) with, if any.
    ///
    /// Read for its words — the `tag` a caller reports when a reservation
    /// is refused.  The kernel itself reads only `amount`, and only once,
    /// when the counter is built.
    pub fn grant(&self) -> Option<&BudgetGrant> {
        self.grant.as_ref()
    }

    /// The turn number the next recorded model response will carry.
    ///
    /// Also the number a *failed* call names in its `model_call_failed`
    /// event: the failure does not consume it, so the next success takes
    /// the same one and the successful turns stay 1, 2, 3 … without gaps.
    ///
    /// A counter, not a scan of the history: it advances in
    /// [`Session::append`]'s `model_response` branch and nowhere else, so
    /// the `turn` field of an appended event — whatever it says — cannot
    /// renumber the run.
    pub fn next_turn(&self) -> u64 {
        self.turns.saturating_add(1)
    }

    /// How many model responses this session has recorded.
    pub fn turns(&self) -> u64 {
        self.turns
    }

    /// The remaining balance (`None` without a budget).
    pub fn remaining(&self) -> Option<i64> {
        self.budget.remaining()
    }

    /// Whether the budget is used up.
    pub fn exhausted(&self) -> bool {
        self.budget.exhausted()
    }

    /// Whether the run scope has ended.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// End the run scope, recording `run_finished` with `reason`
    /// (defaulting to [`DEFAULT_CLOSE_REASON`]).
    ///
    /// Idempotent: closing an already closed session records nothing.
    ///
    /// Fallible on a durable backend: the `run_finished` append CASes
    /// against the observed head, and a concurrent writer can make that
    /// fail.  On failure the session stays open (closed is not set), so
    /// the caller knows the boundary was NOT recorded and can retry —
    /// a close that reports success with no `run_finished` in the log
    /// would silently break resume/audit reads.
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
        let mut event = kernel_event(KIND_RUN_FINISHED);
        event.insert(
            FIELD_REASON.to_string(),
            Value::from(reason.unwrap_or(DEFAULT_CLOSE_REASON)),
        );
        if let Some(detail) = detail {
            event.insert(FIELD_DETAIL.to_string(), Value::from(detail.to_string()));
        }
        self.append(event)?;
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
    use crate::knl::event::{kind_of, seq_of};
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
    /// With a budget it opens with two events, not one: `run_started` and
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

    /// A `model_response` event charging `tokens`, as the kernel accepts it.
    fn response(tokens: i64) -> Map<String, Value> {
        obj(json!({
            "kind": "model_response",
            "content": [{ "type": "text", "text": "ok" }],
            "usage": { "input_tokens": tokens },
            "stop_reason": "end_turn"
        }))
    }

    #[test]
    fn a_new_session_already_carries_run_started() {
        let s = new_session(None);
        assert_eq!(s.len().expect("len"), 1);
        let events = s.events(0).expect("events");
        assert_eq!(kind_of(&events[0]), KIND_RUN_STARTED);
        assert_eq!(seq_of(&events[0]), 1);
        assert!(!s.is_closed());
        assert!(!s.id().is_empty());
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
    fn close_records_run_finished_once_with_the_given_reason() {
        let mut s = new_session(None);
        s.close(Some("budget_exhausted")).expect("close");
        s.close(Some("ignored")).expect("close (idempotent)");
        assert_eq!(s.len().expect("len"), 2, "close must be idempotent");

        let last = s.events(2).expect("events").pop().expect("run_finished");
        assert_eq!(kind_of(&last), KIND_RUN_FINISHED);
        assert_eq!(last["reason"], json!("budget_exhausted"));
        assert!(s.is_closed());
    }

    #[test]
    fn close_without_a_reason_records_the_default() {
        let mut s = new_session(None);
        s.close(None).expect("close");
        let last = s.events(2).expect("events").pop().expect("run_finished");
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
            "run_started + budget_granted + note + budget_spent + run_finished"
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
            "run_started + budget_granted + only_in_a + budget_spent"
        );
        assert_eq!(b.len().expect("len"), 2, "run_started + budget_granted");
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
            "run_started + msg_user + response"
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

    /// Appending a `model_response` records, numbers and counts it — and
    /// leaves the budget alone.  What a call was allowed to cost was
    /// decided before it happened; the record of it happening is not a
    /// second place where that is decided.
    #[test]
    fn appending_a_model_response_numbers_and_counts_it_without_charging() {
        let mut s = new_session(Some(100));
        let seq = s
            .append(obj(json!({
                "kind": "model_response",
                // A turn the caller supplied is ignored: the kernel owns it.
                "turn": 99,
                "content": [{ "type": "text", "text": "hi" }],
                "usage": { "input_tokens": 20, "output_tokens": 10 }
            })))
            .expect("append");

        let recorded = s
            .events(seq)
            .expect("events")
            .pop()
            .expect("model_response");
        assert_eq!(kind_of(&recorded), KIND_MODEL_RESPONSE);
        assert_eq!(recorded[FIELD_TURN], json!(1), "the kernel numbered it 1");

        assert_eq!(s.turns(), 1);
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

    /// The grant is the first thing the ledger says, right after the run's
    /// own boundary, with the owner's words on it.
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
        assert_eq!(events.len(), 2, "run_started + budget_granted");
        assert_eq!(kind_of(&events[0]), KIND_RUN_STARTED);
        assert_eq!(
            events[0].get("budget"),
            None,
            "the grant is its own event, not a field on run_started"
        );

        let granted = &events[1];
        assert_eq!(kind_of(granted), KIND_BUDGET_GRANTED);
        assert_eq!(granted["amount"], json!(500));
        assert_eq!(granted["tag"], json!("tokens"));
        assert_eq!(granted["desc"], json!("one nightly run"));
        assert_eq!(s.remaining(), Some(500));
        assert_eq!(folded(&s), s.remaining());

        // A run with no grant keeps no ledger at all.
        let bare = new_session(None);
        assert_eq!(bare.len().expect("len"), 1, "run_started only");
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

    /// The settlement is recorded like everything else, and the counter and
    /// the fold agree after any sequence of moves.
    #[test]
    fn the_counter_is_a_fold_of_the_ledger_after_any_sequence() {
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

    /// The run scope is the flag, so a `run_finished` a caller writes is a
    /// line in the history and nothing more: the session stays open and
    /// keeps taking writes.
    #[test]
    fn an_appended_run_finished_records_a_fact_without_ending_the_run() {
        let mut s = new_session(Some(100));
        s.append(obj(
            json!({ "kind": "run_finished", "reason": "carried over" }),
        ))
        .expect("append");

        assert!(!s.is_closed(), "an event ended the run scope");
        assert_eq!(s.append(obj(json!({ "kind": "note" }))), Ok(4));
        assert_eq!(s.spend(10), Ok(Some(90)));

        // Closing still writes the kernel's own boundary, and only then
        // do writes stop.
        s.close(Some("done")).expect("close");
        assert!(s.is_closed());
        let last = s.events(0).expect("events").pop().expect("run_finished");
        assert_eq!(kind_of(&last), KIND_RUN_FINISHED);
        assert_eq!(last["reason"], json!("done"));
        assert_eq!(
            s.append(obj(json!({ "kind": "note" })))
                .expect_err("append after close")
                .reason(),
            "session is closed"
        );
    }

    /// The record and the account are separate readings of the same run:
    /// the response is in the history and in `usage`, and the balance is
    /// exactly what was granted, because nobody reserved anything.
    #[test]
    fn a_recorded_response_is_in_the_history_without_being_charged() {
        let mut s = new_session(Some(100));
        s.append(response(30)).expect("recorded");

        assert_eq!(s.turns(), 1);
        assert_eq!(s.remaining(), Some(100));
        assert!(!s.exhausted());

        let recorded = s.events(3).expect("events").pop().expect("model_response");
        assert_eq!(kind_of(&recorded), "model_response");
        assert_eq!(recorded[FIELD_TURN], json!(1));
        assert_eq!(recorded["stop_reason"], json!("end_turn"));
        assert_eq!(s.view(VIEW_USAGE, None).expect("usage")["input_tokens"], 30);
        assert_eq!(folded(&s), Some(100), "the ledger recorded no consumption");
    }

    #[test]
    fn turns_are_numbered_by_the_kernel_over_the_recorded_responses() {
        let mut s = new_session(None);
        assert_eq!(s.next_turn(), 1);

        s.append(response(1)).expect("first");
        assert_eq!(s.turns(), 1);

        // An open-kind event between responses does not advance the counter.
        s.append(obj(
            json!({ "kind": "model_call_failed", "turn": 2, "error": "boom" }),
        ))
        .expect("note");
        assert_eq!(s.turns(), 1, "a non-response must not advance the counter");
        assert_eq!(s.next_turn(), 2);

        s.append(response(1)).expect("second");
        assert_eq!(s.turns(), 2);
        s.append(response(1)).expect("third");
        assert_eq!(s.turns(), 3);
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
            "run_started + budget_granted + run_finished only"
        );
        assert_eq!(s.remaining(), Some(100), "nothing was consumed");
        assert_eq!(s.turns(), 0);
    }

    /// The budget stops a run *before* it spends, not after: a reservation
    /// the balance cannot cover is refused, and the call it was for never
    /// happens.  This replaces the old contract, where the budget was a
    /// flag that only stood up once a recorded call had already used the
    /// allowance up — by which time the spending was done.
    #[test]
    fn the_budget_refuses_before_the_call_rather_than_flagging_after_it() {
        let mut s = new_session(Some(10));

        // The estimate fits, so the beat proceeds and records its response.
        assert_eq!(s.reserve(10), Ok(true));
        s.append(response(25)).expect("recorded");
        assert_eq!(s.remaining(), Some(0), "the reservation took it all");
        assert!(s.exhausted());

        // The next beat asks first and is turned away, so no second
        // response is recorded: the caller never made the call.
        assert_eq!(s.reserve(1), Ok(false));
        assert_eq!(s.turns(), 1, "the refused beat made no call");
        assert_eq!(s.remaining(), Some(0));
        assert_eq!(folded(&s), s.remaining());

        // The kernel still does not police it: a caller that ignores the
        // refusal can append anyway, and the history says that it did.
        s.append(response(5)).expect("recorded");
        assert_eq!(s.turns(), 2, "stopping is the caller's decision");
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

        // `run_finished` costs nothing, so the totals are unchanged even
        // though the history grew — and `at_seq` says the fold saw it.
        assert_eq!(before["input_tokens"], after["input_tokens"]);
        assert_eq!(before["model_calls"], after["model_calls"]);
        assert_eq!(before["at_seq"], json!(2));
        assert_eq!(after["at_seq"], json!(3));

        assert_eq!(s.len().expect("len"), 3);
        assert_eq!(kind_of(&s.events(0).expect("events")[2]), KIND_RUN_FINISHED);
    }

    /// `open_on` on a durable backend records the session's owner on the
    /// `run_started` boundary, so resume can recover it from the log alone.
    #[cfg(feature = "sqlite")]
    #[test]
    fn open_on_records_the_owner_on_run_started() {
        use crate::knl::SqliteEventStore;

        let store = SqliteEventStore::open_in_memory("owner-stream").expect("open");
        let s = Session::open_on("user-7".to_string(), Some(grant(100)), Box::new(store))
            .expect("open");

        let events = s.events(0).expect("events");
        let started = events.first().expect("run_started");
        assert_eq!(kind_of(started), KIND_RUN_STARTED);
        assert_eq!(
            started.get("owner").and_then(Value::as_str),
            Some("user-7"),
            "owner rides on run_started: {started}"
        );
        assert_eq!(s.owner(), "user-7");

        // The grant is durable too, as its own event.
        assert_eq!(kind_of(&events[1]), KIND_BUDGET_GRANTED);
        assert_eq!(events[1]["amount"], json!(100));
    }

    /// Resume re-folds a persisted SQLite stream: the turn counter, the
    /// owner and the *balance* all come back from the log, because every
    /// move of the balance is in it.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_restores_turn_owner_and_the_folded_balance() {
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
            assert_eq!(s.turns(), 2);
            assert_eq!(s.remaining(), Some(50), "100 - 30 - 15 - 5");
            assert_eq!(folded(&s), s.remaining());
            s.remaining()
        }; // dropped: the connection closes, the log persists.

        // Reopen the same stream and resume — no new run_started is written,
        // and no new grant either.
        let store = SqliteEventStore::open(&path, stream).expect("reopen");
        let mut resumed = Session::resume(None, Box::new(store)).expect("resume");

        assert_eq!(
            resumed.owner(),
            "user-42",
            "owner restored from run_started"
        );
        assert_eq!(resumed.turns(), 2, "turn counter restored");
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
            "run_started + granted + reserved + response + msg_user \
             + reserved + response + spent — and nothing from resume itself"
        );

        // The usage view re-folds correctly from the reopened store.
        let usage = resumed.view(VIEW_USAGE, None).expect("usage");
        assert_eq!(usage["model_calls"], json!(2));
        assert_eq!(usage["input_tokens"], json!(50));

        // Numbering continues, and so does the ledger: the next reservation
        // comes off the restored balance.
        assert_eq!(resumed.reserve(5), Ok(true));
        let seq = resumed.append(response(5)).expect("third response");
        let recorded = resumed
            .events(seq)
            .expect("events")
            .pop()
            .expect("model_response");
        assert_eq!(
            recorded[FIELD_TURN],
            json!(3),
            "numbering continues from the restored count"
        );
        assert_eq!(resumed.turns(), 3);
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

    /// An older log with no `owner` on `run_started` resumes as [`ANON`]
    /// rather than failing — the field is a later addition.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_falls_back_to_anon_when_the_log_has_no_owner() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "legacy-stream";

        // Write a run_started with no owner field, as an older build would.
        {
            let mut store = SqliteEventStore::open(&path, stream).expect("open");
            store
                .append(kernel_event(KIND_RUN_STARTED))
                .expect("run_started");
        }

        let store = SqliteEventStore::open(&path, stream).expect("reopen");
        let resumed = Session::resume(None, Box::new(store)).expect("resume");
        assert_eq!(resumed.owner(), ANON);
        assert_eq!(resumed.turns(), 0);
        assert_eq!(resumed.remaining(), None, "resumed without a budget cap");
    }

    /// (Fix 5) Resuming an empty log is a caller error — a mistyped or
    /// nonexistent stream must not fold into an anonymous zero session.
    #[test]
    fn resume_of_an_empty_store_is_a_caller_error_not_an_anon_session() {
        let err = Session::resume(Some(grant(100)), Box::new(MemEventStore::new()))
            .expect_err("an empty store has no run to resume");
        assert!(
            err.reason().contains("no run to resume"),
            "{}",
            err.reason()
        );
    }

    /// (Fix 5) A log that has events but never opened with `run_started` is a
    /// caller error too — the ANON fallback is only for a real run_started.
    #[test]
    fn resume_of_a_store_without_run_started_is_a_caller_error() {
        let mut store = MemEventStore::new();
        store
            .append(obj(json!({ "kind": "note" })))
            .expect("seed a non-run_started event");
        let err = Session::resume(None, Box::new(store))
            .expect_err("a log with no run_started has no run to resume");
        assert!(
            err.reason().contains("no run to resume"),
            "{}",
            err.reason()
        );
    }

    /// A store that drops a competing write in between the head read and the
    /// CAS on the first `model_response`, deterministically reproducing the
    /// race [`Session::append`]'s compare-and-swap guards: the injected write
    /// advances the head, so the session's CAS then holds a stale expectation.
    struct RacyStore {
        inner: MemEventStore,
        injected: bool,
    }

    impl EventStore for RacyStore {
        fn append(&mut self, event: Map<String, Value>) -> KnlResult<crate::knl::Committed> {
            self.inner.append(event)
        }

        fn append_if_head(
            &mut self,
            event: Map<String, Value>,
            expected_head: u64,
        ) -> KnlResult<crate::knl::Committed> {
            let is_response =
                event.get(FIELD_KIND).and_then(Value::as_str) == Some(KIND_MODEL_RESPONSE);
            if is_response && !self.injected {
                self.injected = true;
                // A competing writer lands one event, advancing the head
                // under us so the pending CAS is now stale.
                self.inner
                    .append(obj(json!({ "kind": "sneaked_in" })))
                    .expect("injected concurrent write");
            }
            self.inner.append_if_head(event, expected_head)
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

    /// (Fix 1) `Session::append` is a compare-and-swap on the head: a second
    /// writer landing between the head read and the write makes the append a
    /// stale CAS, which fails loud instead of silently duplicating a turn —
    /// and neither the turn counter nor the budget move on the failure.
    #[test]
    fn append_cas_rejects_a_stale_writer_without_duplicating_a_turn() {
        // Single-writer construction is unaffected: run_started lands as usual.
        let store = RacyStore {
            inner: MemEventStore::new(),
            injected: false,
        };
        let mut s =
            Session::open_on("user".to_string(), Some(grant(1000)), Box::new(store)).expect("open");
        assert_eq!(
            s.len().expect("len"),
            2,
            "run_started + budget_granted so far"
        );
        assert_eq!(s.turns(), 0);

        // The CAS sees a head advanced by the injected competing writer.
        let err = s
            .append(response(10))
            .expect_err("a stale CAS must fail loud");
        assert!(err.reason().contains("head conflict"), "{}", err.reason());

        // Neither the turn counter nor the budget moved: the append did not land.
        assert_eq!(s.turns(), 0, "a failed CAS must not advance the turn");
        assert_eq!(s.remaining(), Some(1000), "a failed CAS must not consume");

        // No model_response reached the log — no duplicate turn was written.
        let log = s.events(0).expect("events");
        let responses = log
            .iter()
            .filter(|e| kind_of(e) == KIND_MODEL_RESPONSE)
            .count();
        assert_eq!(
            responses, 0,
            "the conflicting response must not be recorded"
        );
    }

    /// (Fix 5) Resuming a nonexistent SQLite stream is a caller error, not an
    /// anonymous empty session.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_of_a_nonexistent_sqlite_stream_is_a_caller_error() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        // A stream that was never opened as a run: its log is empty.
        let store = SqliteEventStore::open(&path, "ghost-stream").expect("open");
        let err = Session::resume(Some(grant(100)), Box::new(store))
            .expect_err("an empty stream has no run to resume");
        assert!(
            err.reason().contains("no run to resume"),
            "{}",
            err.reason()
        );
    }

    /// (Concurrency) The interleaved two-session scenario, with no artificial
    /// straddle decorator: two `Session`s open on ONE durable stream, both
    /// observing the same head `H`.  Session A appends a response, so the head
    /// advances; Session B — whose observed head is still `H`, from *before* A's
    /// write — appends a response and MUST get a head-conflict, write nothing,
    /// and leave no duplicate turn in the log.
    ///
    /// This is the exact bug a fresh `self.store.head()` read would *not* catch:
    /// B would read the already-advanced head, its CAS would match, and it would
    /// silently write a second event carrying the same turn.  CASing against the
    /// head each session *observed* is what makes B's stale write fail loud.
    #[cfg(feature = "sqlite")]
    #[test]
    fn two_sessions_interleaving_on_one_stream_conflict_without_duplicating_a_turn() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "interleave-stream";

        // A opens the run on the shared stream: `run_started` lands at seq 1
        // and its `budget_granted` at seq 2, so A observes head 2 (H).
        let store_a = SqliteEventStore::open(&path, stream).expect("open A");
        let mut a = Session::open_on("user".to_string(), Some(grant(1000)), Box::new(store_a))
            .expect("open A");
        assert_eq!(a.turns(), 0);

        // B resumes the SAME stream while it holds only those two, so B
        // observes the same head 2 — genuinely from before A's write, not
        // injected mid-call.  It resumes on the folded balance, granting
        // nothing new (a second grant would be a second write).
        let store_b = SqliteEventStore::open(&path, stream).expect("open B");
        let mut b = Session::resume(None, Box::new(store_b)).expect("resume B");
        assert_eq!(b.turns(), 0);
        assert_eq!(b.remaining(), Some(1000), "B resumed on A's ledger");

        // A appends a model_response: its observed head still matches the real
        // one, so it lands (seq 3, turn 1) and A advances its observed head.
        let a_seq = a.append(response(10)).expect("A appends");
        assert_eq!(a_seq, 3);
        assert_eq!(a.turns(), 1);

        // B's observed head is still 2 — it never saw A's write — so its CAS is
        // stale: a head-conflict, with no write, no charge, and no turn advance.
        let err = b
            .append(response(20))
            .expect_err("B's stale observed head must conflict");
        assert!(err.reason().contains("head conflict"), "{}", err.reason());
        assert_eq!(
            b.turns(),
            0,
            "a conflicting append must not advance B's turn"
        );
        assert_eq!(
            b.remaining(),
            Some(1000),
            "a conflicting append must not consume B's budget"
        );

        // A stale writer cannot move the ledger either: B's reservation
        // fails on the same CAS, and its balance is untouched.
        let err = b
            .reserve(10)
            .expect_err("B's stale reservation must conflict");
        assert!(err.reason().contains("head conflict"), "{}", err.reason());
        assert_eq!(b.remaining(), Some(1000), "a failed CAS must not deduct");

        // The durable log: exactly one event past H = 2 (A's response at seq 3)
        // and exactly one model_response, numbered turn 1 — no duplicate.
        let verify = SqliteEventStore::open(&path, stream).expect("reopen to verify");
        let log = verify.read(0, usize::MAX).expect("read log");
        let past_h: Vec<_> = log.iter().filter(|e| seq_of(e) > 2).collect();
        assert_eq!(past_h.len(), 1, "exactly one event past H");
        let responses: Vec<_> = log
            .iter()
            .filter(|e| kind_of(e) == KIND_MODEL_RESPONSE)
            .collect();
        assert_eq!(
            responses.len(),
            1,
            "no duplicate turn: one model_response only"
        );
        assert_eq!(
            responses[0][FIELD_TURN],
            json!(1),
            "the single recorded response is turn 1"
        );
    }
}
