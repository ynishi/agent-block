//! Named projections over the history.
//!
//! A projection is *derived*: folding never changes the history, and a
//! fold result is a cache rather than a capture — [`Views`] keeps, per
//! view, the last folded `seq` plus the accumulated value, so a call
//! folds only the events appended since the previous one.  Reading a view
//! is therefore amortised in the number of *new* events, not in the size
//! of the history, and the cached value is always reproducible by
//! [`usage_of`] from scratch.
//!
//! The folds consume an event *slice* (`&[Current]`), not a `History`, so the
//! same arithmetic serves the in-memory store and the durable one: the
//! caller reads the range it wants through the seam
//! ([`CurrentStore`](super::event_store::CurrentStore)) and hands the events
//! over.  For the incremental cache the caller reads only what is new
//! (`read(folded_seq + 1, ..)`); for a from-scratch reference it reads the
//! whole log.  The slice is `Current`, so a projection cannot be taken over
//! events that went round the upcaster chain.
//!
//! The vocabulary is closed on purpose — named folds live here rather
//! than being handed to a caller-supplied callback, so the kernel never
//! calls back into the shell while it holds kernel state — and it is
//! deliberately short: the events themselves, `usage` (the token
//! account), `tail` (the last events, for a post-mortem).
//!
//! A fold is named here only when its consumer is fixed in kernel terms:
//! `usage` is the arithmetic the budget charge already does, `tail` is
//! the record read from the end.  A projection whose shape depends on
//! what the caller means to do with it is policy and stays out — the
//! conversation rendered for a provider is the case in point (which role
//! each kind maps to, whether a system message belongs in it, where to
//! cut it off are all decisions of the shell assembling a request), and
//! naming it here would accumulate those decisions in the kernel.  Such a
//! projection is built on the shell side from the incremental event read
//! (`events(from)`) instead.
//!
//! # What `usage` counts
//!
//! [`UsageFold`] adds up every `llm_response` in the session.  A session
//! holds only its own events, so an `llm_response` in it is a call this
//! run made — there is nothing foreign to exclude, and no author to key
//! on.  Those are exactly the responses the budget was charged for, so the
//! view and the balance are two readings of one set of facts rather than
//! two counts that can drift.
//!
//! # The position a fold has reached
//!
//! `usage` carries `at_seq`: the `seq` of the last event folded into it,
//! `0` for a history with nothing in it.  It answers "as of what?" for
//! the totals, and it is the point a shell-side fold reads on from —
//! `events(at_seq + 1)` is the rest of the history — so a projection the
//! kernel does not name can still be caught up against one it does.
//! Events keep their own `seq` in the envelope, so an incremental read
//! needs no separate position of its own.

use serde_json::{Map, Value};

use super::event::{FIELD_USAGE, KIND_LLM_RESPONSE};
use super::event_store::Current;
use super::{KnlError, KnlResult};

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
/// Usage field: number of `llm_response` events folded.
const FIELD_MODEL_CALLS: &str = "model_calls";
/// Usage field: `seq` of the last event the totals include.
const FIELD_AT_SEQ: &str = "at_seq";

/// Incremental fold of the `usage` view.
///
/// Keyed on the `kind` alone: a session holds only its own events, so every
/// `llm_response` in it is a call this run made and was charged for.
/// There is no foreign response to filter out — that was the point of
/// dropping the per-event author.
#[derive(Debug, Clone, Default)]
pub struct UsageFold {
    /// Highest `seq` already folded.
    folded_seq: u64,
    /// Running totals, in the order of [`USAGE_COUNTERS`].
    totals: [i64; 3],
    /// Number of `llm_response` events folded.
    model_calls: u64,
}

impl UsageFold {
    /// Fold the events given that are newer than the last fold.
    ///
    /// The events are a slice read from an
    /// [`EventStore`](super::EventStore), each still carrying its `seq`, so
    /// the same fold serves the in-memory and the durable backend.  An event
    /// at or before [`folded_seq`](Self::folded_seq) is skipped, so handing
    /// a range that overlaps what was already folded does not double-count —
    /// reading twice without an append repeats the value rather than
    /// doubling it.
    pub fn advance(&mut self, events: &[Current]) {
        for event in events {
            if event.seq() <= self.folded_seq {
                continue;
            }
            if event.kind() == KIND_LLM_RESPONSE {
                self.model_calls = self.model_calls.saturating_add(1);
                let usage = event.get(FIELD_USAGE);
                for (slot, counter) in self.totals.iter_mut().zip(USAGE_COUNTERS) {
                    let value = usage.and_then(|u| u.get(counter)).map_or(0, whole);
                    *slot = slot.saturating_add(value);
                }
            }
            self.folded_seq = event.seq();
        }
    }

    /// Highest `seq` folded so far.
    pub fn folded_seq(&self) -> u64 {
        self.folded_seq
    }

    /// The totals as a JSON object (a fresh copy each call).
    ///
    /// `at_seq` says how far the totals reach, so a reader can tell
    /// "nothing was spent" from "nothing has happened yet", and a
    /// shell-side fold has a point to read on from.
    pub fn value(&self) -> Value {
        let mut obj = Map::new();
        for (total, counter) in self.totals.iter().zip(USAGE_COUNTERS) {
            obj.insert(counter.to_string(), Value::from(*total));
        }
        obj.insert(FIELD_MODEL_CALLS.to_string(), Value::from(self.model_calls));
        obj.insert(FIELD_AT_SEQ.to_string(), Value::from(self.folded_seq));
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
    /// `usage` fold state.
    usage: UsageFold,
}

impl Views {
    /// The `usage` view, folding only the events it has not seen.
    ///
    /// `new_events` are the events past
    /// [`usage_folded_seq`](Self::usage_folded_seq) — the caller reads them
    /// with `read(usage_folded_seq + 1, ..)`, so the fold is amortised in
    /// the number of new events, not the size of the history.  Passing a
    /// wider range is safe too: the fold skips anything it has already seen.
    pub fn usage(&mut self, new_events: &[Current]) -> Value {
        self.usage.advance(new_events);
        self.usage.value()
    }

    /// The `seq` the `usage` fold has reached: the point a caller reads on
    /// from to hand [`usage`](Self::usage) only what is new.
    pub fn usage_folded_seq(&self) -> u64 {
        self.usage.folded_seq()
    }
}

/// The `usage` view computed from scratch over `events` (the cache's
/// reference): a fresh fold folds them all, in `seq` order.
pub fn usage_of(events: &[Current]) -> Value {
    let mut fold = UsageFold::default();
    fold.advance(events);
    fold.value()
}

/// The `tail` view: the last `n` of `events`, verbatim.
///
/// The events are copied back out as plain JSON objects — the view is a
/// value handed to a reader, and `Current` is the kernel's own proof that a
/// read went through the seam, not something a reader needs.
pub fn tail_of(events: &[Current], n: usize) -> Value {
    let start = events.len().saturating_sub(n);
    Value::Array(
        events[start..]
            .iter()
            .map(|event| Value::Object((**event).clone()))
            .collect(),
    )
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
            KnlError::Validation(format!(
                "n must be a non-negative whole number, got {}",
                super::event::json_type_name(value)
            ))
        })?;
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::{kind_of, FIELD_CALL_ID, FIELD_CONTENT, FIELD_OK, FIELD_RESULT};
    use crate::knl::History;
    use serde_json::json;

    /// Object map for an event literal.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
    }

    /// The events of `h` from `from` on, as the folds take them.
    ///
    /// These tests drive a [`History`] rather than a session, so there is no
    /// seam to read through; the fixtures are written in today's shape by
    /// construction and say so.
    fn since(h: &History, from: u64) -> Vec<Current> {
        h.since(from)
            .into_iter()
            .map(Current::assume_current)
            .collect()
    }

    /// Append an event, panicking on a rejected fixture.  The one write
    /// path, whoever the event is "from": there is no separate kernel path.
    fn append(history: &mut History, value: Value) {
        let event = value.clone();
        history
            .append(obj(value))
            .unwrap_or_else(|e| panic!("append {event}: {e}"));
    }

    /// Read the `usage` view the way a session does: fold only the events
    /// past what the cache has already seen (`read(folded_seq + 1, ..)`).
    fn usage_cached(views: &mut Views, h: &History) -> Value {
        let from = views.usage_folded_seq().saturating_add(1);
        views.usage(&since(h, from))
    }

    /// A history covering every reserved kind plus an open one.
    fn mixed_history() -> History {
        let mut h = History::new();
        append(&mut h, json!({ "kind": "session_opened" }));
        append(&mut h, json!({ "kind": "msg_user", "content": "hi" }));
        append(
            &mut h,
            json!({
                "kind": "llm_response",
                "beat": "b1",
                "content": [{ "type": "text", "text": "ok" }],
                "usage": { "input_tokens": 10, "output_tokens": 3 }
            }),
        );
        append(
            &mut h,
            json!({
                "kind": "tool_call", "beat": "b1", "call_id": "c1",
                "name": "sh", "args": { "cmd": "ls" }
            }),
        );
        append(
            &mut h,
            json!({
                "kind": "tool_result", "beat": "b1", "call_id": "c1",
                "ok": false, "result": "boom"
            }),
        );
        append(
            &mut h,
            json!({ "kind": "note", "text": "no part of a request" }),
        );
        h
    }

    /// The conversational kinds are read from the record itself: the
    /// events come back in `seq` order with their payloads verbatim, which
    /// is what a shell assembling a request works from now that no named
    /// fold shapes one for it.
    #[test]
    fn the_conversation_is_read_from_the_events_in_seq_order() {
        let events = since(&mixed_history(), 0);
        let kinds: Vec<&str> = events.iter().map(Current::kind).collect();
        assert_eq!(
            kinds,
            [
                "session_opened",
                "msg_user",
                "llm_response",
                "tool_call",
                "tool_result",
                "note"
            ]
        );

        assert_eq!(events[1][FIELD_CONTENT], json!("hi"));
        assert_eq!(
            events[2][FIELD_CONTENT],
            json!([{ "type": "text", "text": "ok" }])
        );
        assert_eq!(events[4][FIELD_CALL_ID], json!("c1"));
        assert_eq!(events[4][FIELD_OK], json!(false));
        assert_eq!(events[4][FIELD_RESULT], json!("boom"));

        // The envelope stays on: an event read hands back the record, not
        // a row shaped for a particular reader.
        assert!(events[1].get("seq").is_some(), "{}", events[1]);
    }

    /// A conversation the caller carried in is material like any other:
    /// the events read back in `seq` order, so continuing a conversation
    /// is an ordinary `append`.
    #[test]
    fn events_read_back_in_order() {
        let mut h = History::new();
        append(
            &mut h,
            json!({
                "kind": "llm_response", "beat": "b1",
                "content": [{ "type": "text", "text": "said last time" }],
                "usage": { "input_tokens": 9_000 }
            }),
        );
        append(&mut h, json!({ "kind": "msg_user", "content": "and now?" }));

        let events = since(&h, 0);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].kind(), KIND_LLM_RESPONSE);
        assert_eq!(
            events[0][FIELD_CONTENT],
            json!([{ "type": "text", "text": "said last time" }])
        );
        assert_eq!(events[1].kind(), "msg_user");
    }

    /// `usage` counts every `llm_response` in the session — there is no
    /// foreign response to exclude, because a session holds only its own
    /// events.
    #[test]
    fn usage_counts_every_llm_response() {
        let mut h = History::new();
        append(
            &mut h,
            json!({
                "kind": "llm_response", "beat": "b1", "content": [],
                "usage": { "input_tokens": 4, "output_tokens": 2 }
            }),
        );
        append(
            &mut h,
            json!({
                "kind": "llm_response", "beat": "b2", "content": [],
                "usage": { "input_tokens": 5, "thinking_tokens": 7 }
            }),
        );
        assert_eq!(
            usage_of(&since(&h, 0)),
            json!({
                "input_tokens": 9,
                "output_tokens": 2,
                "thinking_tokens": 7,
                "model_calls": 2,
                "at_seq": 2
            })
        );
    }

    #[test]
    fn usage_sums_llm_responses_and_counts_the_calls() {
        let mut h = mixed_history();
        append(
            &mut h,
            json!({
                "kind": "llm_response",
                "beat": "b2",
                "content": [],
                "usage": { "input_tokens": 5, "thinking_tokens": 7 }
            }),
        );
        assert_eq!(
            usage_of(&since(&h, 0)),
            json!({
                "input_tokens": 15,
                "output_tokens": 3,
                "thinking_tokens": 7,
                "model_calls": 2,
                "at_seq": 7
            })
        );
    }

    /// An empty slice has folded nothing, and says so: `at_seq` is `0`,
    /// which is the one position `events(from)` can be asked to start
    /// before.
    #[test]
    fn usage_of_an_empty_history_is_all_zero() {
        assert_eq!(
            usage_of(&[]),
            json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "thinking_tokens": 0,
                "model_calls": 0,
                "at_seq": 0
            })
        );
    }

    /// `at_seq` names the last event the totals include — every event,
    /// not only the ones that moved a counter — so it is the point a
    /// shell-side fold reads on from.
    #[test]
    fn at_seq_advances_with_the_history_not_with_the_totals() {
        let mut h = History::new();
        let mut views = Views::default();
        assert_eq!(usage_cached(&mut views, &h)["at_seq"], json!(0));

        append(
            &mut h,
            json!({ "kind": "note", "text": "counts for nothing" }),
        );
        append(&mut h, json!({ "kind": "msg_user", "content": "hi" }));
        let usage = usage_cached(&mut views, &h);
        assert_eq!(usage["at_seq"], json!(2), "{usage}");
        assert_eq!(usage["model_calls"], json!(0), "{usage}");

        let at_seq = usage["at_seq"].as_u64().expect("a position");
        assert!(since(&h, at_seq + 1).is_empty(), "nothing is left to read");
        append(
            &mut h,
            json!({
                "kind": "llm_response", "beat": "b1", "content": [],
                "usage": { "input_tokens": 4 }
            }),
        );
        let rest = since(&h, at_seq + 1);
        assert_eq!(rest.len(), 1, "{rest:?}");
        assert_eq!(usage_cached(&mut views, &h)["at_seq"], json!(3));
    }

    #[test]
    fn the_cache_agrees_with_a_full_recomputation_at_every_step() {
        let mut h = History::new();
        let mut views = Views::default();

        // Reading before anything is appended must not poison the cache.
        assert_eq!(usage_cached(&mut views, &h), usage_of(&since(&h, 0)));

        let script = [
            json!({ "kind": "session_opened" }),
            json!({ "kind": "msg_user", "content": "one" }),
            json!({ "kind": "note", "text": "ignored" }),
            json!({
                "kind": "llm_response", "beat": "b1", "content": [],
                "usage": { "input_tokens": 4, "output_tokens": 2 }
            }),
            json!({
                "kind": "tool_call", "beat": "b1", "call_id": "c",
                "name": "sh", "args": {}
            }),
            json!({
                "kind": "tool_result", "beat": "b1", "call_id": "c",
                "ok": true, "result": { "out": "ok" }
            }),
            json!({ "kind": "msg_user", "content": [{ "type": "text", "text": "two" }] }),
            json!({
                "kind": "llm_response", "beat": "b2", "content": [],
                "usage": { "input_tokens": 9_000 }
            }),
            json!({
                "kind": "llm_response", "beat": "b3", "content": [],
                "usage": { "input_tokens": 6, "thinking_tokens": 1 }
            }),
        ];

        for (i, event) in script.into_iter().enumerate() {
            append(&mut h, event);
            // Read after every append: the incremental result must equal
            // the from-scratch one at each step, not only at the end.
            assert_eq!(
                usage_cached(&mut views, &h),
                usage_of(&since(&h, 0)),
                "step {i}"
            );
        }

        // Reading twice without an append repeats the same value rather
        // than double-folding.
        assert_eq!(usage_cached(&mut views, &h), usage_of(&since(&h, 0)));

        // Every llm_response counts now: three calls, and the 9000-token
        // one is summed in with the rest.
        assert_eq!(
            usage_cached(&mut views, &h),
            json!({
                "input_tokens": 9_010,
                "output_tokens": 2,
                "thinking_tokens": 1,
                "model_calls": 3,
                "at_seq": 9
            })
        );

        let responses = since(&h, 0)
            .iter()
            .filter(|e| e.kind() == KIND_LLM_RESPONSE)
            .count();
        assert_eq!(responses, 3, "{:?}", since(&h, 0));
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
        assert_eq!(usage_cached(&mut views, &h), usage_of(&since(&h, 0)));
        assert_eq!(views.usage_folded_seq(), 10);
    }

    #[test]
    fn tail_returns_the_last_n_events_verbatim() {
        let h = mixed_history();
        let events = since(&h, 0);
        let tail = tail_of(&events, 2);
        let tail = tail.as_array().expect("array");
        assert_eq!(tail.len(), 2);
        assert_eq!(kind_of(&tail[0]), "tool_result");
        assert_eq!(kind_of(&tail[1]), "note");
        assert!(tail[1].get("seq").is_some(), "tail keeps the envelope");

        assert_eq!(tail_of(&events, 99).as_array().map(Vec::len), Some(h.len()));
        assert_eq!(tail_of(&events, 0).as_array().map(Vec::len), Some(0));
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
