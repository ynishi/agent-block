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

use super::event::{kernel_event, FIELD_REASON, KIND_RUN_FINISHED, KIND_RUN_STARTED};
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
            closed: false,
        }
    }

    /// The run-correlation id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Record an event, returning its `seq`.
    pub fn append(&mut self, event: Map<String, Value>) -> KnlResult<u64> {
        if self.closed {
            return Err(KnlError::new("session is closed"));
        }
        self.history.append(event)
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
    use crate::knl::event::{kind_of, seq_of};
    use serde_json::json;

    /// Object map for an event literal.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
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
        s.append(obj(json!({
            "kind": "model_response", "turn": 1, "content": [],
            "usage": { "input_tokens": 9 }
        })))
        .expect("append");

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
