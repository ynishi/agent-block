//! Named projections over the history.
//!
//! A projection is *derived*: folding never changes the history, and a
//! projection's result is a cache rather than a capture — whatever it says
//! is recomputable from the events, so reading one is never what makes it
//! true.
//!
//! The projections consume an event *slice* (`&[Current]`), not a `History`,
//! so the same arithmetic serves the in-memory store and the durable one:
//! the caller reads the range it wants through the seam
//! ([`CurrentStore`](super::event_store::CurrentStore)) and hands the events
//! over.  The slice is `Current`, so a projection cannot be taken over
//! events that skipped the upcaster chain.
//!
//! The vocabulary is closed on purpose — named projections live here rather
//! than being handed to a caller-supplied callback, so the kernel never
//! calls back into the shell while it holds kernel state — and it is now as
//! short as it goes: beside the events themselves, the kernel names `tail`
//! (the last events, for a post-mortem) and nothing else.
//!
//! A projection is named here only when its consumer is fixed in kernel
//! terms, and `tail` is: the record read from the end, in no shape but its
//! own.  One whose shape depends on what the caller means to do with it is
//! policy and stays out.  The conversation rendered for a provider is the
//! case in point — which role each kind maps to, whether a system message
//! belongs in it, where to cut it off are all the shell's decisions — and so
//! is the token account, which reads the `data` of an `llm_response`, a
//! shape the kernel has no opinion about.  Both are written on the Lua side:
//! over the incremental event read (`events(from)`), or as a query view in
//! SQL over the published schema (`session:query`, `knl.views.usage`).

use serde_json::{Map, Value};

use super::event_store::Current;
use super::{KnlError, KnlResult};

/// View name: the last `n` events, verbatim.
pub const VIEW_TAIL: &str = "tail";

/// `tail` option: how many events to return.
pub const OPT_N: &str = "n";
/// How many events `tail` returns when `n` is not given.
pub const DEFAULT_TAIL_N: usize = 20;

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
    use crate::knl::event::{data_field, kind_of, FIELD_DATA};
    use crate::knl::History;
    use serde_json::json;

    /// Object map for an event literal.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
    }

    /// The events of `h` from `from` on, as the projections take them.
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

    /// A history covering a whole beat, plus a kind of the shell's own.
    fn mixed_history() -> History {
        let mut h = History::new();
        append(
            &mut h,
            json!({
                "kind": "session_opened",
                "data": { "scope_id": "scope-1", "owner": "anon" }
            }),
        );
        append(
            &mut h,
            json!({ "kind": "msg_user", "data": { "content": "hi" } }),
        );
        append(
            &mut h,
            json!({
                "kind": "llm_response",
                "beat": "b1",
                "data": {
                    "content": [{ "type": "text", "text": "ok" }],
                    "usage": { "input_tokens": 10, "output_tokens": 3 }
                }
            }),
        );
        append(
            &mut h,
            json!({
                "kind": "tool_call", "beat": "b1",
                "data": { "call_id": "c1", "name": "sh", "args": { "cmd": "ls" } }
            }),
        );
        append(
            &mut h,
            json!({
                "kind": "tool_result", "beat": "b1",
                "data": { "call_id": "c1", "ok": false, "result": "boom" }
            }),
        );
        append(
            &mut h,
            json!({ "kind": "note", "data": { "text": "no part of a request" } }),
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

        // Each kind's own fields are under `data`, verbatim — the kernel
        // stores what was written and reads none of it.
        assert_eq!(events[1][FIELD_DATA], json!({ "content": "hi" }));
        assert_eq!(
            data_field(&events[2], "content"),
            Some(&json!([{ "type": "text", "text": "ok" }]))
        );
        assert_eq!(
            events[4][FIELD_DATA],
            json!({ "call_id": "c1", "ok": false, "result": "boom" })
        );

        // The envelope stays on: an event read hands back the record, not
        // a row shaped for a particular reader.
        assert!(events[1].get("seq").is_some(), "{}", events[1]);
        assert_eq!(events[3]["beat"], json!("b1"), "{}", events[3]);
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
                "data": {
                    "content": [{ "type": "text", "text": "said last time" }],
                    "usage": { "input_tokens": 9_000 }
                }
            }),
        );
        append(
            &mut h,
            json!({ "kind": "msg_user", "data": { "content": "and now?" } }),
        );

        let events = since(&h, 0);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].kind(), "llm_response");
        assert_eq!(
            data_field(&events[0], "content"),
            Some(&json!([{ "type": "text", "text": "said last time" }]))
        );
        assert_eq!(events[1].kind(), "msg_user");
    }

    /// The token account is not one of the names any more: it reads the
    /// `data` of an `llm_response`, which is the shell's vocabulary, so it is
    /// a query view in Lua (`knl.views.usage`) over the published schema.
    /// What the kernel still hands it is the record, in `seq` order.
    #[test]
    fn the_token_account_is_read_from_the_data_the_kernel_stores_verbatim() {
        let events = since(&mixed_history(), 0);
        let responses: Vec<&Current> = events
            .iter()
            .filter(|e| e.kind() == "llm_response")
            .collect();
        assert_eq!(responses.len(), 1, "{events:?}");
        assert_eq!(
            data_field(responses[0], "usage"),
            Some(&json!({ "input_tokens": 10, "output_tokens": 3 })),
            "the counts are stored as the provider reported them"
        );
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
