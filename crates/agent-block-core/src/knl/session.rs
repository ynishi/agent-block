//! K5 — the run scope.
//!
//! A session binds one history, one budget and the projection caches
//! together and is the only handle on kernel state.  All of it lives in
//! the value: two sessions share nothing.
//!
//! The kernel writes the run's own boundaries: [`Session::new`] appends
//! `run_started` and [`Session::close`] appends `run_finished`, so a run
//! is bracketed in the history whether or not the shell remembers to say
//! so.  After a close, `append` / `spend` are errors while reads keep
//! working — the record outlives the run.

use serde_json::{Map, Value};

use super::call::{failure_event, CallOutcome, ModelResult};
use super::event::{
    is_kernel_authored, kernel_event, kind_field, FIELD_REASON, KIND_RUN_FINISHED, KIND_RUN_STARTED,
};
use super::projection::{tail_count, Views, VIEW_DIALOGUE, VIEW_TAIL, VIEW_USAGE};
use super::{projection, Budget, History, KnlError, KnlResult};

/// Reason recorded by `close()` when the caller does not give one.
pub const DEFAULT_CLOSE_REASON: &str = "closed";

/// One run scope.
#[derive(Debug)]
pub struct Session {
    /// Run-correlation id, unique per session.
    id: String,
    /// K1 append-only history.
    history: History,
    /// K4 budget counter.
    budget: Budget,
    /// Cached projection folds (derived, never authoritative).
    views: Views,
    /// Model responses recorded so far: the turn number's authority.
    turns: u64,
    /// Set by `close()`; blocks further `append` / `spend`.
    closed: bool,
}

impl Session {
    /// Open a run with an optional token budget.
    ///
    /// The `run_started` event is appended here, so a fresh session
    /// already has one event.
    pub fn new(budget_tokens: Option<i64>) -> Self {
        let mut history = History::new();
        history.append_kernel(kernel_event(KIND_RUN_STARTED));
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            history,
            budget: Budget::new(budget_tokens),
            views: Views::default(),
            turns: 0,
            closed: false,
        }
    }

    /// The run-correlation id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Record a caller-authored event, returning its `seq`.
    ///
    /// The kinds the kernel authors itself
    /// ([`super::event::KERNEL_AUTHORED_KINDS`]) are refused here, before
    /// the run scope is even consulted: a caller may echo the facts it
    /// owns, not manufacture the ones the kernel produces.  Accepting a
    /// forged one would make three things legal that the rest of this
    /// module exists to rule out:
    ///
    /// - a `model_response` nobody was charged for.  The budget is only
    ///   deducted on the way through [`Session::record_model_response`],
    ///   while the `usage` fold counts whatever events say
    ///   `model_response` — so a direct append shows up in the usage view
    ///   as a call that cost nothing, and the two stop agreeing.
    /// - a turn the kernel did not number.  `turn` is a required field of
    ///   the kind, so a caller supplies one, and nothing stops it from
    ///   being a number the kernel is about to hand out itself — leaving
    ///   two responses claiming the same turn with no way to order them.
    /// - a `run_finished` in a session that is still open.  `close()` is
    ///   what ends the scope; writing the event without it leaves a
    ///   history that says the run ended while `append` / `spend` keep
    ///   working.
    ///
    /// The kernel's own writes go through [`Session::append_kernel`],
    /// which skips this gate and nothing else.
    pub fn append(&mut self, event: Map<String, Value>) -> KnlResult<u64> {
        if let Some(kind) = kind_field(&event) {
            if is_kernel_authored(kind) {
                return Err(KnlError::new(format!("kind {kind:?} is kernel-authored")));
            }
        }
        if self.closed {
            return Err(KnlError::new("session is closed"));
        }
        self.history.append(event)
    }

    /// Record a kernel-authored event, returning its `seq`.
    ///
    /// The run-scope check of [`Session::append`] without its
    /// kernel-authored gate: the payload was built here from the reserved
    /// vocabulary, and it is precisely what the gate keeps a caller from
    /// imitating.  Private, so the bypass has no way out of this module.
    fn append_kernel(&mut self, event: Map<String, Value>) -> KnlResult<u64> {
        if self.closed {
            return Err(KnlError::new("session is closed"));
        }
        Ok(self.history.append_kernel(event))
    }

    /// Events with `seq >= from`, cloned.
    pub fn events(&self, from: u64) -> Vec<Value> {
        self.history.since(from)
    }

    /// Number of recorded events.
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// Whether the history is empty (only before `run_started`, i.e.
    /// never for a session built by [`Session::new`]).
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
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
    pub fn next_turn(&self) -> u64 {
        self.turns.saturating_add(1)
    }

    /// How many model responses this session has recorded.
    pub fn turns(&self) -> u64 {
        self.turns
    }

    /// Record a model response and charge what it cost — steps [4] and
    /// [5] of the call sequence, in that order and in one place.
    ///
    /// The write comes first on purpose: if the history takes the response
    /// and the charge is what fails, the run is over-recorded and
    /// under-charged, which is visible and recoverable; the other order
    /// can bill for a turn that no event mentions.  In practice the charge
    /// cannot fail here — the amount is non-negative by construction and
    /// the append just proved the session open — so the failure path is
    /// kept only because the types say it exists.
    ///
    /// The turn counter advances only on the way through: a call that is
    /// never recorded leaves the numbering where it was.
    pub fn record_model_response(&mut self, result: &ModelResult) -> KnlResult<CallOutcome> {
        let turn = self.next_turn();
        self.append_kernel(result.to_event(turn))?;
        self.turns = turn;
        let remaining = self.spend(result.charge())?;
        Ok(CallOutcome {
            turn,
            remaining,
            exhausted: self.exhausted(),
        })
    }

    /// Note a call that produced no result, best effort.
    ///
    /// Returns whether the note landed: a closed run cannot take it, and
    /// that is not worth turning into a second failure on top of the one
    /// being reported — the caller is already returning an error.
    pub fn record_model_call_failure(&mut self, error: &str) -> bool {
        let event = failure_event(self.next_turn(), error);
        self.append_kernel(event).is_ok()
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
        self.history.append_kernel(event);
        self.closed = true;
    }

    /// A named projection over the history.
    ///
    /// `dialogue` and `usage` are served from the incremental caches;
    /// `tail` reads `opts.n` (default
    /// [`projection::DEFAULT_TAIL_N`]).  An unknown name is an error —
    /// the vocabulary is closed on purpose.
    pub fn view(&mut self, name: &str, opts: Option<&Map<String, Value>>) -> KnlResult<Value> {
        match name {
            VIEW_DIALOGUE => Ok(self.views.dialogue(&self.history)),
            VIEW_USAGE => Ok(self.views.usage(&self.history)),
            VIEW_TAIL => Ok(projection::tail_of(&self.history, tail_count(opts)?)),
            other => Err(KnlError::new(format!("unknown view {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::call::validate_backend_result;
    use crate::knl::event::{kind_of, seq_of, FIELD_TURN};
    use serde_json::json;

    /// Object map for an event literal.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
    }

    /// A backend result charging `tokens`, as the kernel accepts it.
    fn result(tokens: i64) -> ModelResult {
        validate_backend_result(&json!({
            "content": [{ "type": "text", "text": "ok" }],
            "usage": { "input_tokens": tokens },
            "stop_reason": "end_turn"
        }))
        .expect("contract met")
    }

    #[test]
    fn a_new_session_already_carries_run_started() {
        let s = Session::new(None);
        assert_eq!(s.len(), 1);
        let events = s.events(0);
        assert_eq!(kind_of(&events[0]), KIND_RUN_STARTED);
        assert_eq!(seq_of(&events[0]), 1);
        assert!(!s.is_closed());
        assert!(!s.id().is_empty());
    }

    #[test]
    fn close_records_run_finished_once_with_the_given_reason() {
        let mut s = Session::new(None);
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
        let mut s = Session::new(None);
        s.close(None);
        let last = s.events(2).pop().expect("run_finished");
        assert_eq!(last["reason"], json!(DEFAULT_CLOSE_REASON));
    }

    #[test]
    fn a_closed_session_rejects_writes_but_keeps_serving_reads() {
        let mut s = Session::new(Some(10));
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
        let mut a = Session::new(Some(100));
        let mut b = Session::new(Some(100));
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
        let mut s = Session::new(None);
        s.append(obj(json!({ "kind": "msg_user", "content": "hi" })))
            .expect("append");
        // Through the kernel, because a `model_response` is not a caller's
        // to append.
        s.record_model_response(&result(9)).expect("recorded");

        let dialogue = s.view(VIEW_DIALOGUE, None).expect("dialogue");
        assert_eq!(dialogue.as_array().map(Vec::len), Some(2));

        let usage = s.view(VIEW_USAGE, None).expect("usage");
        assert_eq!(usage["input_tokens"], json!(9));
        assert_eq!(usage["model_calls"], json!(1));

        let tail = s
            .view(VIEW_TAIL, Some(&obj(json!({ "n": 1 }))))
            .expect("tail");
        assert_eq!(tail.as_array().map(Vec::len), Some(1));

        let err = s.view("nope", None).expect_err("unknown view");
        assert_eq!(err.reason(), r#"unknown view "nope""#);
    }

    /// The three kinds the kernel authors are refused to a caller, and
    /// refused on their own terms: the message names the kind rather than
    /// the state of the run, and a rejected event costs neither a `seq`
    /// nor a turn.
    #[test]
    fn a_caller_cannot_author_the_kinds_the_kernel_writes_itself() {
        let mut s = Session::new(Some(100));
        for (kind, event) in [
            ("run_started", json!({ "kind": "run_started" })),
            (
                "run_finished",
                json!({ "kind": "run_finished", "reason": "faked" }),
            ),
            (
                "model_response",
                json!({
                    "kind": "model_response", "turn": 1,
                    "content": [{ "type": "text", "text": "never happened" }],
                    "usage": { "input_tokens": 9_000 }
                }),
            ),
        ] {
            let err = s.append(obj(event)).expect_err("a forged kernel event");
            assert_eq!(err.reason(), format!("kind {kind:?} is kernel-authored"));
        }

        // Nothing landed: the usage view still says no call was made, the
        // run is still open, and the next seq is the one the first
        // rejected event would have taken.
        assert_eq!(s.len(), 1, "run_started only");
        assert_eq!(s.view(VIEW_USAGE, None).expect("usage")["model_calls"], 0);
        assert!(!s.is_closed());
        assert_eq!(s.append(obj(json!({ "kind": "note" }))), Ok(2));

        // And the kernel still writes all three itself.
        assert_eq!(
            s.record_model_response(&result(10)).expect("recorded").turn,
            1
        );
        s.close(Some("done"));
        assert_eq!(kind_of(&s.events(0)[3]), KIND_RUN_FINISHED);
    }

    /// The gate is ahead of the run-scope check: a closed session refuses
    /// a forged `model_response` as a forgery, not as a late write.
    #[test]
    fn a_forged_kind_is_refused_by_kind_before_the_run_scope_is_consulted() {
        let mut s = Session::new(None);
        s.close(None);

        let err = s
            .append(obj(json!({
                "kind": "model_response", "turn": 1, "content": [], "usage": {}
            })))
            .expect_err("a forged kernel event");
        assert_eq!(err.reason(), r#"kind "model_response" is kernel-authored"#);

        // An open kind on the same closed session still reports the scope.
        let err = s
            .append(obj(json!({ "kind": "note" })))
            .expect_err("append after close");
        assert_eq!(err.reason(), "session is closed");
    }

    #[test]
    fn a_recorded_response_is_in_the_history_before_it_is_charged() {
        let mut s = Session::new(Some(100));
        let outcome = s.record_model_response(&result(30)).expect("recorded");

        assert_eq!(outcome.turn, 1);
        assert_eq!(outcome.remaining, Some(70));
        assert!(!outcome.exhausted);

        // The record says the same thing the outcome does.
        let recorded = s.events(2).pop().expect("model_response");
        assert_eq!(kind_of(&recorded), "model_response");
        assert_eq!(recorded[FIELD_TURN], json!(1));
        assert_eq!(recorded["stop_reason"], json!("end_turn"));
        assert_eq!(s.remaining(), Some(70));
        assert_eq!(s.view(VIEW_USAGE, None).expect("usage")["input_tokens"], 30);
    }

    #[test]
    fn turns_are_numbered_by_the_kernel_and_a_failure_takes_no_number() {
        let mut s = Session::new(None);
        assert_eq!(s.next_turn(), 1);

        assert_eq!(s.record_model_response(&result(1)).expect("first").turn, 1);
        assert!(s.record_model_call_failure("backend: boom"), "recorded");
        assert_eq!(s.turns(), 1, "a failure must not advance the counter");
        assert_eq!(s.next_turn(), 2);
        assert_eq!(s.record_model_response(&result(1)).expect("second").turn, 2);
        assert_eq!(s.record_model_response(&result(1)).expect("third").turn, 3);

        // The note names the turn the retry then took.
        let noted = s
            .events(0)
            .into_iter()
            .find(|e| kind_of(e) == "model_call_failed")
            .expect("model_call_failed");
        assert_eq!(noted[FIELD_TURN], json!(2));
        assert_eq!(noted["error"], json!("backend: boom"));
    }

    #[test]
    fn a_closed_session_records_neither_a_response_nor_a_failure() {
        let mut s = Session::new(Some(100));
        s.close(None);

        let err = s
            .record_model_response(&result(10))
            .expect_err("closed session");
        assert_eq!(err.reason(), "session is closed");
        assert!(
            !s.record_model_call_failure("backend: boom"),
            "a closed run cannot take the note either"
        );

        assert_eq!(s.len(), 2, "run_started + run_finished only");
        assert_eq!(s.remaining(), Some(100), "nothing was charged");
        assert_eq!(s.turns(), 0);
    }

    #[test]
    fn the_budget_is_only_a_flag_when_a_call_uses_it_up() {
        let mut s = Session::new(Some(10));
        let outcome = s.record_model_response(&result(25)).expect("recorded");
        assert_eq!(outcome.remaining, Some(0), "the charge floors at zero");
        assert!(outcome.exhausted, "the flag is set");

        // Exhausted does not stop the kernel: the next call is recorded
        // and charged too.  Stopping is the caller's decision.
        let outcome = s.record_model_response(&result(5)).expect("recorded");
        assert_eq!(outcome.turn, 2);
        assert!(outcome.exhausted);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn without_a_budget_a_call_reports_no_remaining_and_is_never_exhausted() {
        let mut s = Session::new(None);
        let outcome = s.record_model_response(&result(9_000)).expect("recorded");
        assert_eq!(outcome.remaining, None);
        assert!(!outcome.exhausted);
    }

    #[test]
    fn views_stay_readable_and_correct_after_close() {
        let mut s = Session::new(None);
        s.append(obj(json!({ "kind": "msg_user", "content": "hi" })))
            .expect("append");
        let before = s.view(VIEW_DIALOGUE, None).expect("dialogue");
        s.close(None);
        let after = s.view(VIEW_DIALOGUE, None).expect("dialogue after close");
        // run_finished is not a dialogue kind, so the fold is unchanged
        // even though the history grew.
        assert_eq!(before, after);
        assert_eq!(s.len(), 3);
    }
}
