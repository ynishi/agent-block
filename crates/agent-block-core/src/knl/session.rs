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
//! `model_response` it also *numbers and charges* it: the kernel assigns
//! the `turn` from its own count (overwriting any the caller supplied) and
//! deducts the usage from the budget.  So appending a `model_response` is
//! the recorded, numbered, charged call — there is no separate step a
//! driver has to remember, and no path by which a response reaches the
//! history without being accounted for.
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

use super::call::charge_of;
use super::event::{
    kernel_event, kind_of, FIELD_KIND, FIELD_REASON, FIELD_TURN, FIELD_USAGE, KIND_MODEL_RESPONSE,
    KIND_RUN_FINISHED, KIND_RUN_STARTED,
};
use super::event_store::{EventStore, MemEventStore};
use super::projection::{tail_count, Views, VIEW_TAIL, VIEW_USAGE};
use super::{projection, Budget, KnlError, KnlResult};

/// Reason recorded by `close()` when the caller does not give one.
pub const DEFAULT_CLOSE_REASON: &str = "closed";

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
    /// Cached projection folds (derived, never authoritative).
    views: Views,
    /// Model responses recorded so far: the turn number's authority.
    turns: u64,
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
    /// Open a run for `owner` with an optional token budget, on the
    /// in-memory store.
    ///
    /// `owner` is total: pass a real principal id, or [`ANON`] / [`SYSTEM`]
    /// for the reserved ones.  The `run_started` event is appended here, so
    /// a fresh session already has one event.
    pub fn new(owner: String, budget_tokens: Option<i64>) -> Self {
        Self::open_on(owner, budget_tokens, Box::new(MemEventStore::new()))
    }

    /// Open a run for `owner` on a caller-chosen backend `store` (the
    /// in-memory store, or the durable SQLite one).
    ///
    /// Like [`Session::new`] but takes the backend, so the shell decides
    /// whether the log is ephemeral or persisted.  It appends the same
    /// `run_started` boundary, recording the session's `owner` on it so a
    /// later [`Session::resume`] can recover the principal from the log
    /// alone.  `run_started` is an open-shape reserved kind, so the extra
    /// `owner` field is accepted without any change to the validator.
    pub fn open_on(owner: String, budget_tokens: Option<i64>, store: Box<dyn EventStore>) -> Self {
        let mut session = Self {
            id: uuid::Uuid::new_v4().to_string(),
            owner,
            store,
            budget: Budget::new(budget_tokens),
            views: Views::default(),
            turns: 0,
            closed: false,
        };
        // `run_started` is well-formed and the session is open, so this
        // append cannot fail; the same one path records it as records
        // everything else.  The owner rides along so resume can recover it.
        let mut started = kernel_event(KIND_RUN_STARTED);
        started.insert(FIELD_OWNER.to_string(), Value::from(session.owner.clone()));
        let _ = session.append(started);
        session
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
    /// - the budget: `budget_tokens` is a fresh policy input (the cap on the
    ///   resumed run), and the already-spent total is the summed
    ///   [`charge_of`] over the recorded responses, spent once here so
    ///   [`Session::remaining`] reflects what the run has used.
    ///
    /// The projection caches start empty and re-fold lazily from the store,
    /// so a resumed session's `usage` view is correct on first read.
    pub fn resume(budget_tokens: Option<i64>, store: Box<dyn EventStore>) -> KnlResult<Self> {
        let log = store.read(0, usize::MAX);

        let owner = log
            .iter()
            .find(|event| kind_of(event) == KIND_RUN_STARTED)
            .and_then(|event| event.get(FIELD_OWNER).and_then(Value::as_str))
            .unwrap_or(ANON)
            .to_string();

        let turns = log
            .iter()
            .filter(|event| kind_of(event) == KIND_MODEL_RESPONSE)
            .count() as u64;

        let spent: i64 = log
            .iter()
            .filter(|event| kind_of(event) == KIND_MODEL_RESPONSE)
            .map(|event| {
                event
                    .get(FIELD_USAGE)
                    .and_then(Value::as_object)
                    .map_or(0, charge_of)
            })
            .sum();

        // The budget spends monotonically, so the restored total is spent
        // once here; `remaining()` then reflects the run's prior usage.
        let mut budget = Budget::new(budget_tokens);
        let _ = budget.spend(spent);

        Ok(Self {
            id: uuid::Uuid::new_v4().to_string(),
            owner,
            store,
            budget,
            views: Views::default(),
            turns,
            closed: false,
        })
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
    /// A `model_response` is more than a record: appending one *is* the
    /// recorded, numbered, charged call.  The kernel
    ///
    /// - assigns the `turn` from its own count (overwriting whatever the
    ///   caller put there — the number is kernel-owned, like `seq`), and
    /// - charges the usage against the budget, write-ahead: the response is
    ///   in the history before the balance moves, so a charge that somehow
    ///   failed would over-record and under-charge (visible, recoverable)
    ///   rather than bill for a turn no event names.  In practice it cannot
    ///   fail — the amount is non-negative by construction.
    ///
    /// A `run_finished` a caller appends is a line in the history, not a
    /// close: the run scope is the `closed` flag, and only
    /// [`Session::close`] sets it.
    pub fn append(&mut self, mut event: Map<String, Value>) -> KnlResult<u64> {
        if self.closed {
            return Err(KnlError::new("session is closed"));
        }

        let is_response =
            event.get(FIELD_KIND).and_then(Value::as_str) == Some(KIND_MODEL_RESPONSE);
        if !is_response {
            return self.store.append(event).map(|committed| committed.seq);
        }

        // Kernel-owned turn, assigned before the event is stored so the
        // record carries the number the run will refer to it by.
        let turn = self.next_turn();
        event.insert(FIELD_TURN.to_string(), Value::from(turn));
        let charge = event
            .get(FIELD_USAGE)
            .and_then(Value::as_object)
            .map_or(0, charge_of);

        // Validate + stamp + push.  A malformed response is rejected here,
        // before the turn counter advances or the budget is touched.
        let seq = self.store.append(event)?.seq;
        self.turns = turn;
        let _ = self.budget.spend(charge);
        Ok(seq)
    }

    /// Events with `seq >= from`, cloned.
    pub fn events(&self, from: u64) -> Vec<Value> {
        self.store.read(from, usize::MAX)
    }

    /// Number of recorded events.
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Whether the history is empty (only before `run_started`, i.e.
    /// never for a session built by [`Session::new`]).
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    /// Deduct `amount` from the budget, returning the new balance.
    pub fn spend(&mut self, amount: i64) -> KnlResult<Option<i64>> {
        if self.closed {
            return Err(KnlError::new("session is closed"));
        }
        self.budget.spend(amount)
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
    pub fn close(&mut self, reason: Option<&str>) {
        if self.closed {
            return;
        }
        let mut event = kernel_event(KIND_RUN_FINISHED);
        event.insert(
            FIELD_REASON.to_string(),
            Value::from(reason.unwrap_or(DEFAULT_CLOSE_REASON)),
        );
        // The same append records the boundary; it is well-formed and the
        // session is still open, so it cannot fail.
        let _ = self.append(event);
        self.closed = true;
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
                let fresh = self.store.read(from, usize::MAX);
                Ok(self.views.usage(&fresh))
            }
            VIEW_TAIL => {
                let n = tail_count(opts)?;
                let events = self.store.read(0, usize::MAX);
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

    /// A session owned by the reserved anonymous principal.
    fn new_session(budget: Option<i64>) -> Session {
        Session::new(ANON.to_string(), budget)
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
        assert_eq!(s.len(), 1);
        let events = s.events(0);
        assert_eq!(kind_of(&events[0]), KIND_RUN_STARTED);
        assert_eq!(seq_of(&events[0]), 1);
        assert!(!s.is_closed());
        assert!(!s.id().is_empty());
    }

    #[test]
    fn the_owner_is_total_and_read_back_verbatim() {
        assert_eq!(new_session(None).owner(), ANON);
        assert_eq!(Session::new(SYSTEM.to_string(), None).owner(), SYSTEM);
        assert_eq!(
            Session::new("user-42".to_string(), None).owner(),
            "user-42"
        );
    }

    #[test]
    fn close_records_run_finished_once_with_the_given_reason() {
        let mut s = new_session(None);
        s.close(Some("budget_exhausted"));
        s.close(Some("ignored"));
        assert_eq!(s.len(), 2, "close must be idempotent");

        let last = s.events(2).pop().expect("run_finished");
        assert_eq!(kind_of(&last), KIND_RUN_FINISHED);
        assert_eq!(last["reason"], json!("budget_exhausted"));
        assert!(s.is_closed());
    }

    #[test]
    fn close_without_a_reason_records_the_default() {
        let mut s = new_session(None);
        s.close(None);
        let last = s.events(2).pop().expect("run_finished");
        assert_eq!(last["reason"], json!(DEFAULT_CLOSE_REASON));
    }

    #[test]
    fn a_closed_session_rejects_writes_but_keeps_serving_reads() {
        let mut s = new_session(Some(10));
        s.append(obj(json!({ "kind": "note" }))).expect("append");
        s.spend(4).expect("spend");
        s.close(None);

        let err = s
            .append(obj(json!({ "kind": "note" })))
            .expect_err("append after close");
        assert_eq!(err.reason(), "session is closed");
        let err = s.spend(1).expect_err("spend after close");
        assert_eq!(err.reason(), "session is closed");

        assert_eq!(s.len(), 3, "run_started + note + run_finished");
        assert_eq!(s.remaining(), Some(6));
        assert!(!s.exhausted());
        assert_eq!(kind_of(&s.events(0)[1]), "note");
    }

    #[test]
    fn two_sessions_share_nothing() {
        let mut a = new_session(Some(100));
        let mut b = new_session(Some(100));
        assert_ne!(a.id(), b.id());

        a.append(obj(json!({ "kind": "only_in_a" })))
            .expect("append");
        a.spend(60).expect("spend");

        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
        assert_eq!(a.remaining(), Some(40));
        assert_eq!(b.remaining(), Some(100));

        a.close(None);
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

        let events = s.events(0);
        assert_eq!(kind_of(&events[1]), "msg_user");
        assert_eq!(events[1]["content"], json!("hi"));
    }

    /// Appending a `model_response` is the recorded, numbered, charged
    /// call: the kernel stamps the turn, the usage view counts it, and the
    /// budget is charged — all through the one write path, whoever appended
    /// it, because a session holds only its own events.
    #[test]
    fn appending_a_model_response_numbers_charges_and_counts_it() {
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

        let recorded = s.events(seq).pop().expect("model_response");
        assert_eq!(kind_of(&recorded), KIND_MODEL_RESPONSE);
        assert_eq!(recorded[FIELD_TURN], json!(1), "the kernel numbered it 1");

        assert_eq!(s.turns(), 1);
        assert_eq!(s.remaining(), Some(70), "20 + 10 charged");
        let usage = s.view(VIEW_USAGE, None).expect("usage");
        assert_eq!(usage["model_calls"], json!(1));
        assert_eq!(usage["input_tokens"], json!(20));
        assert_eq!(usage["output_tokens"], json!(10));
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
        assert_eq!(s.append(obj(json!({ "kind": "note" }))), Ok(3));
        assert_eq!(s.spend(10), Ok(Some(90)));

        // Closing still writes the kernel's own boundary, and only then
        // do writes stop.
        s.close(Some("done"));
        assert!(s.is_closed());
        let last = s.events(0).pop().expect("run_finished");
        assert_eq!(kind_of(&last), KIND_RUN_FINISHED);
        assert_eq!(last["reason"], json!("done"));
        assert_eq!(
            s.append(obj(json!({ "kind": "note" })))
                .expect_err("append after close")
                .reason(),
            "session is closed"
        );
    }

    #[test]
    fn a_recorded_response_is_in_the_history_before_it_is_charged() {
        let mut s = new_session(Some(100));
        s.append(response(30)).expect("recorded");

        assert_eq!(s.turns(), 1);
        assert_eq!(s.remaining(), Some(70));
        assert!(!s.exhausted());

        let recorded = s.events(2).pop().expect("model_response");
        assert_eq!(kind_of(&recorded), "model_response");
        assert_eq!(recorded[FIELD_TURN], json!(1));
        assert_eq!(recorded["stop_reason"], json!("end_turn"));
        assert_eq!(s.remaining(), Some(70));
        assert_eq!(s.view(VIEW_USAGE, None).expect("usage")["input_tokens"], 30);
    }

    #[test]
    fn turns_are_numbered_by_the_kernel_over_the_recorded_responses() {
        let mut s = new_session(None);
        assert_eq!(s.next_turn(), 1);

        s.append(response(1)).expect("first");
        assert_eq!(s.turns(), 1);

        // An open-kind event between responses does not advance the counter.
        s.append(obj(json!({ "kind": "model_call_failed", "turn": 2, "error": "boom" })))
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
        s.close(None);

        let err = s.append(response(10)).expect_err("closed session");
        assert_eq!(err.reason(), "session is closed");

        assert_eq!(s.len(), 2, "run_started + run_finished only");
        assert_eq!(s.remaining(), Some(100), "nothing was charged");
        assert_eq!(s.turns(), 0);
    }

    #[test]
    fn the_budget_is_only_a_flag_when_a_call_uses_it_up() {
        let mut s = new_session(Some(10));
        s.append(response(25)).expect("recorded");
        assert_eq!(s.remaining(), Some(0), "the charge floors at zero");
        assert!(s.exhausted(), "the flag is set");

        // Exhausted does not stop the kernel: the next call is recorded
        // and charged too.  Stopping is the caller's decision.
        s.append(response(5)).expect("recorded");
        assert_eq!(s.turns(), 2);
        assert!(s.exhausted());
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn without_a_budget_a_call_reports_no_remaining_and_is_never_exhausted() {
        let mut s = new_session(None);
        s.append(response(9_000)).expect("recorded");
        assert_eq!(s.remaining(), None);
        assert!(!s.exhausted());
    }

    #[test]
    fn views_stay_readable_and_correct_after_close() {
        let mut s = new_session(None);
        s.append(response(9)).expect("recorded");
        let before = s.view(VIEW_USAGE, None).expect("usage");
        s.close(None);
        let after = s.view(VIEW_USAGE, None).expect("usage after close");

        // `run_finished` costs nothing, so the totals are unchanged even
        // though the history grew — and `at_seq` says the fold saw it.
        assert_eq!(before["input_tokens"], after["input_tokens"]);
        assert_eq!(before["model_calls"], after["model_calls"]);
        assert_eq!(before["at_seq"], json!(2));
        assert_eq!(after["at_seq"], json!(3));

        assert_eq!(s.len(), 3);
        assert_eq!(kind_of(&s.events(0)[2]), KIND_RUN_FINISHED);
    }

    /// `open_on` on a durable backend records the session's owner on the
    /// `run_started` boundary, so resume can recover it from the log alone.
    #[cfg(feature = "sqlite")]
    #[test]
    fn open_on_records_the_owner_on_run_started() {
        use crate::knl::SqliteEventStore;

        let store = SqliteEventStore::open_in_memory("owner-stream").expect("open");
        let s = Session::open_on("user-7".to_string(), Some(100), Box::new(store));

        let started = s.events(0);
        let started = started.first().expect("run_started");
        assert_eq!(kind_of(started), KIND_RUN_STARTED);
        assert_eq!(
            started.get("owner").and_then(Value::as_str),
            Some("user-7"),
            "owner rides on run_started: {started}"
        );
        assert_eq!(s.owner(), "user-7");
    }

    /// Resume re-folds a persisted SQLite stream: the turn counter, the
    /// spent budget and the owner all come back from the log, and new turns
    /// number and charge on from the restored state.
    #[cfg(feature = "sqlite")]
    #[test]
    fn resume_restores_turn_owner_and_spent_budget_from_the_log() {
        use crate::knl::SqliteEventStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let stream = "resume-stream";

        // A durable session: two charged responses and a user message.
        {
            let store = SqliteEventStore::open(&path, stream).expect("open");
            let mut s = Session::open_on("user-42".to_string(), Some(100), Box::new(store));
            s.append(response(30)).expect("first response");
            s.append(obj(json!({ "kind": "msg_user", "content": "more" })))
                .expect("msg_user");
            s.append(response(20)).expect("second response");
            assert_eq!(s.turns(), 2);
            assert_eq!(s.remaining(), Some(50));
        } // dropped: the connection closes, the log persists.

        // Reopen the same stream and resume — no new run_started is written.
        let store = SqliteEventStore::open(&path, stream).expect("reopen");
        let mut resumed = Session::resume(Some(100), Box::new(store)).expect("resume");

        assert_eq!(resumed.owner(), "user-42", "owner restored from run_started");
        assert_eq!(resumed.turns(), 2, "turn counter restored");
        assert_eq!(
            resumed.remaining(),
            Some(50),
            "spent budget restored (30 + 20 charged)"
        );
        // Resume appended nothing: the log is exactly what was persisted.
        assert_eq!(resumed.len(), 4, "run_started + response + msg_user + response");

        // The usage view re-folds correctly from the reopened store.
        let usage = resumed.view(VIEW_USAGE, None).expect("usage");
        assert_eq!(usage["model_calls"], json!(2));
        assert_eq!(usage["input_tokens"], json!(50));

        // Numbering and accounting continue: the next response is turn 3.
        let seq = resumed.append(response(5)).expect("third response");
        let recorded = resumed.events(seq).pop().expect("model_response");
        assert_eq!(
            recorded[FIELD_TURN],
            json!(3),
            "numbering continues from the restored count"
        );
        assert_eq!(resumed.turns(), 3);
        assert_eq!(resumed.remaining(), Some(45), "5 more charged");
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
}
