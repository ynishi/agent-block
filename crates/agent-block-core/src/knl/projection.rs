//! Named projections over the history.
//!
//! A projection is *derived*: folding never changes the history, and a
//! fold result is a cache rather than a capture — [`Views`] keeps, per
//! view, the last folded `seq` plus the accumulated value, so a call
//! folds only the events appended since the previous one.  Reading a view
//! is therefore amortised in the number of *new* events, not in the size
//! of the history, and the cached value is always reproducible by
//! [`dialogue_of`] / [`usage_of`] from scratch.
//!
//! The vocabulary is closed on purpose: named folds live here rather than
//! being handed to a caller-supplied callback, so the kernel never calls
//! back into the shell while it holds kernel state.  A projection the
//! kernel does not name is built on the shell side from the incremental
//! event read instead.
//!
//! # `dialogue` fold rules (provider neutral)
//!
//! | event kind       | dialogue row                                        |
//! |------------------|-----------------------------------------------------|
//! | `msg_user`       | `{ role = "user", content = <content> }`            |
//! | `model_response` | `{ role = "assistant", content = <content> }`       |
//! | `tool_result`    | `{ role = "tool", call_id, ok, result }`            |
//! | (anything else)  | — (does not appear)                                 |
//!
//! Rows are in `seq` order.  Wire shaping for a specific provider is a
//! shell concern; this view is its provider-neutral input.
//!
//! The dialogue fold reads `kind` and ignores `author`, on purpose: a
//! conversation the caller carried in — an earlier assistant turn, say —
//! is material for the next request whoever recorded it, so continuing a
//! conversation is an ordinary `append` and needs no syscall of its own.
//!
//! # What `usage` counts
//!
//! The other way round: [`UsageFold`] adds up the `model_response` events
//! the *kernel* wrote and no others.  Those are exactly the responses the
//! budget was charged for, so the view and the balance are two readings
//! of one set of facts rather than two counts that can drift.  A caller's
//! `model_response` is a fact about some other run — it cost this one
//! nothing, and it is reported as costing nothing.

use serde_json::{Map, Value};

use super::event::{
    is_kernel_authored, kind_of, seq_of, FIELD_CALL_ID, FIELD_CONTENT, FIELD_OK, FIELD_RESULT,
    FIELD_USAGE, KIND_MODEL_RESPONSE, KIND_MSG_USER, KIND_TOOL_RESULT,
};
use super::{History, KnlError, KnlResult};

/// View name: the conversation as provider-neutral rows.
pub const VIEW_DIALOGUE: &str = "dialogue";
/// View name: usage totals.
pub const VIEW_USAGE: &str = "usage";
/// View name: the last `n` events, verbatim.
pub const VIEW_TAIL: &str = "tail";

/// `tail` option: how many events to return.
pub const OPT_N: &str = "n";
/// How many events `tail` returns when `n` is not given.
pub const DEFAULT_TAIL_N: usize = 20;

/// Usage counters summed by the `usage` view.
///
/// `pub(super)` because the budget charge of a model call sums exactly
/// these, read exactly the way [`whole`] reads them: what a run is charged
/// and what its `usage` view reports are then the same arithmetic over the
/// same fields, rather than two definitions that can drift apart.
pub(super) const USAGE_COUNTERS: [&str; 3] = ["input_tokens", "output_tokens", "thinking_tokens"];
/// Usage field: number of kernel-authored `model_response` events folded.
const FIELD_MODEL_CALLS: &str = "model_calls";

/// Incremental fold of the `dialogue` view.
#[derive(Debug, Clone, Default)]
pub struct DialogueFold {
    /// Highest `seq` already folded.
    folded_seq: u64,
    /// Rows accumulated so far, in `seq` order.
    rows: Vec<Value>,
}

impl DialogueFold {
    /// Fold every event newer than the last fold.
    pub fn advance(&mut self, history: &History) {
        for event in history.slice_after(self.folded_seq) {
            if let Some(row) = dialogue_row(event) {
                self.rows.push(row);
            }
            self.folded_seq = seq_of(event);
        }
    }

    /// Highest `seq` folded so far.
    pub fn folded_seq(&self) -> u64 {
        self.folded_seq
    }

    /// The rows as a JSON array (a fresh copy each call).
    pub fn value(&self) -> Value {
        Value::Array(self.rows.clone())
    }
}

/// The dialogue row an event contributes, if any.
fn dialogue_row(event: &Value) -> Option<Value> {
    let field = |name: &str| event.get(name).cloned().unwrap_or(Value::Null);
    let mut row = Map::new();
    match kind_of(event) {
        KIND_MSG_USER => {
            row.insert("role".to_string(), Value::from("user"));
            row.insert(FIELD_CONTENT.to_string(), field(FIELD_CONTENT));
        }
        KIND_MODEL_RESPONSE => {
            row.insert("role".to_string(), Value::from("assistant"));
            row.insert(FIELD_CONTENT.to_string(), field(FIELD_CONTENT));
        }
        KIND_TOOL_RESULT => {
            row.insert("role".to_string(), Value::from("tool"));
            row.insert(FIELD_CALL_ID.to_string(), field(FIELD_CALL_ID));
            row.insert(FIELD_OK.to_string(), field(FIELD_OK));
            row.insert(FIELD_RESULT.to_string(), field(FIELD_RESULT));
        }
        _ => return None,
    }
    Some(Value::Object(row))
}

/// Incremental fold of the `usage` view.
///
/// Keyed on `author`: what this run spent is what the kernel recorded
/// spending, and a `model_response` a caller appended is somebody else's
/// bill.  Reading the kind alone would count it, and the totals would
/// then disagree with the budget the run was actually charged.
#[derive(Debug, Clone, Default)]
pub struct UsageFold {
    /// Highest `seq` already folded.
    folded_seq: u64,
    /// Running totals, in the order of [`USAGE_COUNTERS`].
    totals: [i64; 3],
    /// Number of kernel-authored `model_response` events folded.
    model_calls: u64,
}

impl UsageFold {
    /// Fold every event newer than the last fold.
    pub fn advance(&mut self, history: &History) {
        for event in history.slice_after(self.folded_seq) {
            if kind_of(event) == KIND_MODEL_RESPONSE && is_kernel_authored(event) {
                self.model_calls = self.model_calls.saturating_add(1);
                let usage = event.get(FIELD_USAGE);
                for (slot, counter) in self.totals.iter_mut().zip(USAGE_COUNTERS) {
                    let value = usage.and_then(|u| u.get(counter)).map_or(0, whole);
                    *slot = slot.saturating_add(value);
                }
            }
            self.folded_seq = seq_of(event);
        }
    }

    /// Highest `seq` folded so far.
    pub fn folded_seq(&self) -> u64 {
        self.folded_seq
    }

    /// The totals as a JSON object (a fresh copy each call).
    pub fn value(&self) -> Value {
        let mut obj = Map::new();
        for (total, counter) in self.totals.iter().zip(USAGE_COUNTERS) {
            obj.insert(counter.to_string(), Value::from(*total));
        }
        obj.insert(FIELD_MODEL_CALLS.to_string(), Value::from(self.model_calls));
        Value::Object(obj)
    }
}

/// A usage counter as a whole number (`0` when absent or not numeric —
/// the counters are provider-supplied and optional).
pub(super) fn whole(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|f| f.trunc() as i64))
        .unwrap_or(0)
}

/// The cached folds a session carries.
#[derive(Debug, Clone, Default)]
pub struct Views {
    /// `dialogue` fold state.
    dialogue: DialogueFold,
    /// `usage` fold state.
    usage: UsageFold,
}

impl Views {
    /// The `dialogue` view, folding only what is new.
    pub fn dialogue(&mut self, history: &History) -> Value {
        self.dialogue.advance(history);
        self.dialogue.value()
    }

    /// The `usage` view, folding only what is new.
    pub fn usage(&mut self, history: &History) -> Value {
        self.usage.advance(history);
        self.usage.value()
    }
}

/// The `dialogue` view computed from scratch (the cache's reference).
pub fn dialogue_of(history: &History) -> Value {
    let mut fold = DialogueFold::default();
    fold.advance(history);
    fold.value()
}

/// The `usage` view computed from scratch (the cache's reference).
pub fn usage_of(history: &History) -> Value {
    let mut fold = UsageFold::default();
    fold.advance(history);
    fold.value()
}

/// The `tail` view: the last `n` events, verbatim.
pub fn tail_of(history: &History, n: usize) -> Value {
    Value::Array(history.tail(n))
}

/// Read `opts.n` for the `tail` view, defaulting to [`DEFAULT_TAIL_N`].
pub fn tail_count(opts: Option<&Map<String, Value>>) -> KnlResult<usize> {
    let Some(value) = opts.and_then(|o| o.get(OPT_N)) else {
        return Ok(DEFAULT_TAIL_N);
    };
    if value.is_null() {
        return Ok(DEFAULT_TAIL_N);
    }
    let n = value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0)
        .ok_or_else(|| {
            KnlError::new(format!(
                "n must be a non-negative whole number, got {}",
                super::event::json_type_name(value)
            ))
        })?;
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Object map for an event literal.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
    }

    /// Append a caller-authored event, panicking on a rejected fixture.
    fn append(history: &mut History, value: Value) {
        let event = value.clone();
        history
            .append(obj(value))
            .unwrap_or_else(|e| panic!("append {event}: {e}"));
    }

    /// Append an event the way the kernel does — the path `run_started`,
    /// `run_finished` and a recorded `model_response` take.
    fn append_kernel(history: &mut History, value: Value) {
        history.append_kernel(obj(value));
    }

    /// A history covering every reserved kind plus an open one, each on
    /// the path it takes in a real run.
    fn mixed_history() -> History {
        let mut h = History::new();
        append_kernel(&mut h, json!({ "kind": "run_started" }));
        append(&mut h, json!({ "kind": "msg_user", "content": "hi" }));
        append_kernel(
            &mut h,
            json!({
                "kind": "model_response",
                "turn": 1,
                "content": [{ "type": "text", "text": "ok" }],
                "usage": { "input_tokens": 10, "output_tokens": 3 }
            }),
        );
        append(
            &mut h,
            json!({
                "kind": "tool_call", "turn": 1, "call_id": "c1",
                "name": "sh", "args": { "cmd": "ls" }
            }),
        );
        append(
            &mut h,
            json!({
                "kind": "tool_result", "turn": 1, "call_id": "c1",
                "ok": false, "result": "boom"
            }),
        );
        append(&mut h, json!({ "kind": "note", "text": "not dialogue" }));
        h
    }

    #[test]
    fn dialogue_folds_only_the_three_conversational_kinds_in_seq_order() {
        let rows = dialogue_of(&mixed_history());
        let rows = rows.as_array().expect("dialogue is an array").clone();
        assert_eq!(rows.len(), 3, "{rows:?}");

        assert_eq!(rows[0]["role"], json!("user"));
        assert_eq!(rows[0][FIELD_CONTENT], json!("hi"));

        assert_eq!(rows[1]["role"], json!("assistant"));
        assert_eq!(
            rows[1][FIELD_CONTENT],
            json!([{ "type": "text", "text": "ok" }])
        );

        assert_eq!(rows[2]["role"], json!("tool"));
        assert_eq!(rows[2][FIELD_CALL_ID], json!("c1"));
        assert_eq!(rows[2][FIELD_OK], json!(false));
        assert_eq!(rows[2][FIELD_RESULT], json!("boom"));
    }

    #[test]
    fn dialogue_rows_carry_no_envelope_fields() {
        let rows = dialogue_of(&mixed_history());
        for row in rows.as_array().expect("array") {
            assert!(row.get("seq").is_none(), "{row}");
            assert!(row.get("kind").is_none(), "{row}");
            assert!(row.get("author").is_none(), "{row}");
        }
    }

    /// A conversation the caller carried in is dialogue material like any
    /// other: the fold reads the kind, so a `model_response` that came
    /// from an earlier run takes its place in `seq` order.
    #[test]
    fn dialogue_reads_the_kind_whoever_wrote_it() {
        let mut h = History::new();
        append(
            &mut h,
            json!({
                "kind": "model_response", "turn": 1,
                "content": [{ "type": "text", "text": "said last time" }],
                "usage": { "input_tokens": 9_000 }
            }),
        );
        append(&mut h, json!({ "kind": "msg_user", "content": "and now?" }));

        let rows = dialogue_of(&h);
        let rows = rows.as_array().expect("array");
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0]["role"], json!("assistant"));
        assert_eq!(
            rows[0][FIELD_CONTENT],
            json!([{ "type": "text", "text": "said last time" }])
        );
        assert_eq!(rows[1]["role"], json!("user"));
    }

    /// …and it is invisible to the accounting: the usage view reports what
    /// this run was charged for, which is what the kernel recorded.
    #[test]
    fn usage_ignores_a_model_response_the_caller_brought() {
        let mut h = History::new();
        append(
            &mut h,
            json!({
                "kind": "model_response", "turn": 1, "content": [],
                "usage": { "input_tokens": 9_000, "output_tokens": 9_000 }
            }),
        );
        assert_eq!(
            usage_of(&h),
            json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "thinking_tokens": 0,
                "model_calls": 0
            })
        );

        // The same payload on the kernel's path is counted in full, so it
        // is the author and nothing else that decides.
        append_kernel(
            &mut h,
            json!({
                "kind": "model_response", "turn": 1, "content": [],
                "usage": { "input_tokens": 4, "output_tokens": 2 }
            }),
        );
        assert_eq!(
            usage_of(&h),
            json!({
                "input_tokens": 4,
                "output_tokens": 2,
                "thinking_tokens": 0,
                "model_calls": 1
            })
        );
    }

    #[test]
    fn usage_sums_model_responses_and_counts_the_calls() {
        let mut h = mixed_history();
        append_kernel(
            &mut h,
            json!({
                "kind": "model_response",
                "turn": 2,
                "content": [],
                "usage": { "input_tokens": 5, "thinking_tokens": 7 }
            }),
        );
        assert_eq!(
            usage_of(&h),
            json!({
                "input_tokens": 15,
                "output_tokens": 3,
                "thinking_tokens": 7,
                "model_calls": 2
            })
        );
    }

    #[test]
    fn usage_of_an_empty_history_is_all_zero() {
        assert_eq!(
            usage_of(&History::new()),
            json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "thinking_tokens": 0,
                "model_calls": 0
            })
        );
    }

    #[test]
    fn the_cache_agrees_with_a_full_recomputation_at_every_step() {
        let mut h = History::new();
        let mut views = Views::default();

        // Reading before anything is appended must not poison the cache.
        assert_eq!(views.dialogue(&h), dialogue_of(&h));
        assert_eq!(views.usage(&h), usage_of(&h));

        // `true` where the kernel is the one writing, so the script mixes
        // both authors and the cache has to agree about both.
        let script = [
            (true, json!({ "kind": "run_started" })),
            (false, json!({ "kind": "msg_user", "content": "one" })),
            (false, json!({ "kind": "note", "text": "ignored" })),
            (
                true,
                json!({
                    "kind": "model_response", "turn": 1, "content": [],
                    "usage": { "input_tokens": 4, "output_tokens": 2 }
                }),
            ),
            (
                false,
                json!({
                    "kind": "tool_call", "turn": 1, "call_id": "c",
                    "name": "sh", "args": {}
                }),
            ),
            (
                false,
                json!({
                    "kind": "tool_result", "turn": 1, "call_id": "c",
                    "ok": true, "result": { "out": "ok" }
                }),
            ),
            (
                false,
                json!({ "kind": "msg_user", "content": [{ "type": "text", "text": "two" }] }),
            ),
            (
                // A caller's response: dialogue takes it, usage does not,
                // and the cache has to reach the same conclusion as a
                // fold from scratch.
                false,
                json!({
                    "kind": "model_response", "turn": 1, "content": [],
                    "usage": { "input_tokens": 9_000 }
                }),
            ),
            (
                true,
                json!({
                    "kind": "model_response", "turn": 2, "content": [],
                    "usage": { "input_tokens": 6, "thinking_tokens": 1 }
                }),
            ),
        ];

        for (i, (by_kernel, event)) in script.into_iter().enumerate() {
            if by_kernel {
                append_kernel(&mut h, event);
            } else {
                append(&mut h, event);
            }
            // Read after every append: the incremental result must equal
            // the from-scratch one at each step, not only at the end.
            assert_eq!(views.dialogue(&h), dialogue_of(&h), "step {i}");
            assert_eq!(views.usage(&h), usage_of(&h), "step {i}");
        }

        // Reading twice without an append repeats the same value rather
        // than double-folding.
        assert_eq!(views.dialogue(&h), dialogue_of(&h));
        assert_eq!(views.usage(&h), usage_of(&h));

        // Agreeing with a from-scratch fold that made the same mistake
        // would prove nothing, so the totals are named: three assistant
        // rows in the dialogue, two calls in the usage, and the caller's
        // 9000 tokens nowhere.
        assert_eq!(
            views.usage(&h),
            json!({
                "input_tokens": 10,
                "output_tokens": 2,
                "thinking_tokens": 1,
                "model_calls": 2
            })
        );
        let rows = views.dialogue(&h);
        let assistants = rows
            .as_array()
            .expect("array")
            .iter()
            .filter(|row| row["role"] == json!("assistant"))
            .count();
        assert_eq!(assistants, 3, "{rows}");
    }

    #[test]
    fn a_cache_read_after_many_appends_catches_up_in_one_go() {
        let mut h = History::new();
        let mut views = Views::default();
        for i in 0..10 {
            append(
                &mut h,
                json!({ "kind": "msg_user", "content": format!("m{i}") }),
            );
        }
        assert_eq!(views.dialogue(&h), dialogue_of(&h));
        assert_eq!(views.dialogue.folded_seq(), 10);
    }

    #[test]
    fn tail_returns_the_last_n_events_verbatim() {
        let h = mixed_history();
        let tail = tail_of(&h, 2);
        let tail = tail.as_array().expect("array");
        assert_eq!(tail.len(), 2);
        assert_eq!(kind_of(&tail[0]), "tool_result");
        assert_eq!(kind_of(&tail[1]), "note");
        assert!(tail[1].get("seq").is_some(), "tail keeps the envelope");

        assert_eq!(tail_of(&h, 99).as_array().map(Vec::len), Some(h.len()));
        assert_eq!(tail_of(&h, 0).as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn tail_count_defaults_and_validates() {
        assert_eq!(tail_count(None), Ok(DEFAULT_TAIL_N));
        assert_eq!(tail_count(Some(&obj(json!({})))), Ok(DEFAULT_TAIL_N));
        assert_eq!(tail_count(Some(&obj(json!({ "n": 5 })))), Ok(5));
        assert_eq!(tail_count(Some(&obj(json!({ "n": 5.0 })))), Ok(5));
        assert_eq!(tail_count(Some(&obj(json!({ "n": 0 })))), Ok(0));

        let err = tail_count(Some(&obj(json!({ "n": -1 })))).expect_err("negative n");
        assert!(err.reason().contains("non-negative"), "{err}");
        let err = tail_count(Some(&obj(json!({ "n": "many" })))).expect_err("string n");
        assert!(err.reason().contains("got string"), "{err}");
        let err = tail_count(Some(&obj(json!({ "n": 1.5 })))).expect_err("fractional n");
        assert!(err.reason().contains("whole number"), "{err}");
    }
}
