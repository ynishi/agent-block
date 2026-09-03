//! K1 — the append-only history.
//!
//! The absence of mutation APIs is the specification, not an omission:
//! there is no `update`, `delete` or `replace`.  Reads clone, so a caller
//! holding a returned value cannot reach recorded state.

use serde_json::{Map, Value};

use super::event::{seq_of, stamp, validate_event};
use super::KnlResult;

/// First sequence number handed out by a fresh history.
pub const FIRST_SEQ: u64 = 1;

/// Append-only event store.
#[derive(Debug, Clone, Default)]
pub struct History {
    /// Recorded events, in `seq` order.  Every entry is a JSON object.
    events: Vec<Value>,
    /// Sequence number handed to the next append.
    next_seq: u64,
}

impl History {
    /// A fresh, empty history.
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            next_seq: FIRST_SEQ,
        }
    }

    /// Validate and append a caller-authored event, returning its `seq`.
    ///
    /// A rejected event leaves no trace and does not consume a sequence
    /// number.
    pub fn append(&mut self, obj: Map<String, Value>) -> KnlResult<u64> {
        validate_event(&obj)?;
        Ok(self.push(obj))
    }

    /// Append a kernel-authored event (`run_started` / `run_finished` /
    /// `model_response` / `model_call_failed`).
    ///
    /// Validation is skipped because the payload is built inside this
    /// module from the reserved vocabulary itself — re-checking it would
    /// only add a failure path that cannot be reached.  Caller-authored
    /// events go through [`History::append`]; what stops a caller from
    /// imitating one of these lives a layer up, in
    /// [`super::Session::append`], next to the turn counter, budget and
    /// run scope a forgery would put out of step.
    pub(super) fn append_kernel(&mut self, obj: Map<String, Value>) -> u64 {
        self.push(obj)
    }

    /// Stamp the envelope and record the event.
    fn push(&mut self, mut obj: Map<String, Value>) -> u64 {
        let seq = self.next_seq;
        stamp(&mut obj, seq, super::now_ms());
        self.events.push(Value::Object(obj));
        self.next_seq = seq.saturating_add(1);
        seq
    }

    /// Number of recorded events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether nothing has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The sequence number the next append will receive.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// All recorded events, borrowed.
    pub fn events(&self) -> &[Value] {
        &self.events
    }

    /// Events with `seq >= from`, cloned.
    pub fn since(&self, from: u64) -> Vec<Value> {
        let start = self.events.partition_point(|e| seq_of(e) < from);
        self.events[start..].to_vec()
    }

    /// Events with `seq > seq`, borrowed.
    ///
    /// This is the incremental read a projection fold uses: `seq` order is
    /// maintained by construction, so the split point is a binary search
    /// rather than a scan of the whole history.
    pub fn slice_after(&self, seq: u64) -> &[Value] {
        let start = self.events.partition_point(|e| seq_of(e) <= seq);
        &self.events[start..]
    }

    /// The last `n` events, cloned (fewer when the history is shorter).
    pub fn tail(&self, n: usize) -> Vec<Value> {
        let start = self.events.len().saturating_sub(n);
        self.events[start..].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::{FIELD_EPOCH_MS, FIELD_KIND, FIELD_SEQ};
    use serde_json::json;

    /// Object map for an event literal.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
    }

    /// Append `n` open-kind events named `e1..eN`.
    fn fill(history: &mut History, n: usize) {
        for i in 1..=n {
            history
                .append(obj(json!({ "kind": format!("e{i}") })))
                .unwrap_or_else(|e| panic!("append e{i}: {e}"));
        }
    }

    #[test]
    fn append_assigns_strictly_increasing_seq_from_one() {
        let mut h = History::new();
        assert!(h.is_empty());
        assert_eq!(h.append(obj(json!({ "kind": "a" }))), Ok(1));
        assert_eq!(h.append(obj(json!({ "kind": "b" }))), Ok(2));
        assert_eq!(h.len(), 2);
        assert_eq!(h.next_seq(), 3);

        let seqs: Vec<u64> = h.events().iter().map(seq_of).collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn append_stamps_the_envelope_and_keeps_the_payload() {
        let mut h = History::new();
        h.append(obj(json!({ "kind": "note", "text": "hi", "seq": 999 })))
            .expect("append");
        let event = &h.events()[0];
        assert_eq!(event.get(FIELD_KIND).and_then(Value::as_str), Some("note"));
        assert_eq!(event.get("text").and_then(Value::as_str), Some("hi"));
        assert_eq!(event.get(FIELD_SEQ).and_then(Value::as_u64), Some(1));
        assert!(event.get(FIELD_EPOCH_MS).and_then(Value::as_u64).is_some());
    }

    #[test]
    fn a_rejected_append_records_nothing_and_burns_no_seq() {
        let mut h = History::new();
        h.append(obj(json!({ "text": "no kind" })))
            .expect_err("kind is required");
        assert_eq!(h.len(), 0);
        assert_eq!(h.append(obj(json!({ "kind": "ok" }))), Ok(1));
    }

    #[test]
    fn since_filters_by_seq() {
        let mut h = History::new();
        fill(&mut h, 3);
        assert_eq!(h.since(0).len(), 3);
        assert_eq!(h.since(1).len(), 3);
        assert_eq!(h.since(2).len(), 2);
        assert_eq!(h.since(4).len(), 0);
        assert_eq!(seq_of(&h.since(2)[0]), 2);
    }

    #[test]
    fn slice_after_is_the_incremental_read() {
        let mut h = History::new();
        fill(&mut h, 3);
        assert_eq!(h.slice_after(0).len(), 3);
        assert_eq!(h.slice_after(2).len(), 1);
        assert_eq!(seq_of(&h.slice_after(2)[0]), 3);
        assert!(h.slice_after(3).is_empty());
        assert!(h.slice_after(99).is_empty());
    }

    #[test]
    fn tail_clamps_to_the_available_length() {
        let mut h = History::new();
        fill(&mut h, 3);
        assert_eq!(h.tail(0).len(), 0);
        assert_eq!(h.tail(2).len(), 2);
        assert_eq!(seq_of(&h.tail(2)[0]), 2);
        assert_eq!(h.tail(99).len(), 3);
    }

    #[test]
    fn reads_are_copies_so_history_cannot_be_reached_through_them() {
        let mut h = History::new();
        fill(&mut h, 1);
        let mut copy = h.since(0);
        copy[0]["kind"] = Value::String("TAMPERED".into());
        copy.push(json!({ "kind": "ghost" }));
        assert_eq!(h.len(), 1);
        assert_eq!(
            h.events()[0].get(FIELD_KIND).and_then(Value::as_str),
            Some("e1")
        );
    }
}
