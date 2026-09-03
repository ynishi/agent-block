//! K2 — the model call: what the kernel does with a backend's result.
//!
//! `call` is the shape that forces "record it and account for it before
//! returning".  The backend — transport plus provider knowledge — is the
//! shell's, and the kernel never looks inside the request; what it insists
//! on is that whatever comes back is something it can record, that the
//! record is written before the budget is charged, and that the response
//! is stamped with a turn number the kernel owns.
//!
//! Split against the adapter: invoking the backend is a Lua call, so it
//! belongs to [`crate::bridge::knl`], which runs it while holding nothing.
//! What is here is the half that needs no VM — the result contract
//! ([`validate_backend_result`]), the charge, and the event a result
//! becomes — so it stays unit-testable and cannot call back into the
//! shell.
//!
//! The turn number is the kernel's rather than the caller's because a
//! session outlives any one loop over it: a caller-supplied turn restarts
//! at 1 every time a new loop is pointed at the same session, and the
//! history would then carry several `turn = 1` responses with no way to
//! order them.  [`super::Session`] counts successful calls instead, so the
//! numbering is monotone for the life of the run.

use serde_json::{Map, Value};

use super::event::{
    json_type_name, FIELD_CONTENT, FIELD_KIND, FIELD_TURN, FIELD_USAGE, KIND_MODEL_RESPONSE,
};
use super::projection::{whole, USAGE_COUNTERS};
use super::{KnlError, KnlResult};

/// Payload field of `model_response`: why the model stopped.
pub const FIELD_STOP_REASON: &str = "stop_reason";
/// Open kind recorded when a call produced no response the kernel could
/// take: the backend failed, or what it handed back does not meet the
/// contract below.  Both mean the same thing to the run — a model call
/// was attempted and there is nothing to show for it — so they are one
/// kind, and the reason is what tells them apart.
///
/// Open rather than reserved: a failed call is a fact about the shell's
/// transport, not one of the Turn facts the kernel checks, and v1 keeps
/// the reserved vocabulary as it is.
pub const KIND_MODEL_CALL_FAILED: &str = "model_call_failed";
/// Payload field of `model_call_failed`: the failure, as text.
pub const FIELD_ERROR: &str = "error";

/// A backend result the kernel is willing to record.
///
/// Holding one is the proof that the contract was met: it can only be
/// built by [`validate_backend_result`], so nothing downstream has to
/// re-check the three fields.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelResult {
    /// The response blocks, verbatim and non-empty.
    content: Vec<Value>,
    /// The provider's usage counters (any subset, all optional).
    usage: Map<String, Value>,
    /// Why the model stopped.
    stop_reason: String,
}

impl ModelResult {
    /// The response blocks.
    pub fn content(&self) -> &[Value] {
        &self.content
    }

    /// The usage counters as reported.
    pub fn usage(&self) -> &Map<String, Value> {
        &self.usage
    }

    /// Why the model stopped.
    pub fn stop_reason(&self) -> &str {
        &self.stop_reason
    }

    /// What this response costs the budget: input + output + thinking.
    ///
    /// The counters are provider-supplied, so a missing one is `0` and the
    /// total is floored at `0` — a backend reporting a negative counter
    /// cannot turn a charge into a refund.
    pub fn charge(&self) -> i64 {
        USAGE_COUNTERS
            .iter()
            .fold(0i64, |total, counter| {
                total.saturating_add(self.usage.get(*counter).map_or(0, whole))
            })
            .max(0)
    }

    /// The `model_response` event this result becomes at `turn`.
    pub fn to_event(&self, turn: u64) -> Map<String, Value> {
        let mut event = Map::new();
        event.insert(FIELD_KIND.to_string(), Value::from(KIND_MODEL_RESPONSE));
        event.insert(FIELD_TURN.to_string(), Value::from(turn));
        event.insert(
            FIELD_CONTENT.to_string(),
            Value::Array(self.content.clone()),
        );
        event.insert(FIELD_USAGE.to_string(), Value::Object(self.usage.clone()));
        event.insert(
            FIELD_STOP_REASON.to_string(),
            Value::from(self.stop_reason.clone()),
        );
        event
    }
}

/// What a recorded call reports back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallOutcome {
    /// The turn the response was stamped with (1-based, monotone).
    pub turn: u64,
    /// Budget left after the charge (`None` without a budget).
    pub remaining: Option<i64>,
    /// Whether the budget is now used up.
    ///
    /// A flag, not a stop: whether to open another turn is the caller's
    /// decision, taken at the top of its loop.
    pub exhausted: bool,
}

/// The `model_call_failed` event for a call that never produced a result.
///
/// `turn` is the number the call *would* have been given: a failure does
/// not consume one, so the next successful call takes it.
pub fn failure_event(turn: u64, error: &str) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert(FIELD_KIND.to_string(), Value::from(KIND_MODEL_CALL_FAILED));
    event.insert(FIELD_TURN.to_string(), Value::from(turn));
    event.insert(FIELD_ERROR.to_string(), Value::from(error));
    event
}

/// Check a backend result against the call contract.
///
/// `content` must be a non-empty array of blocks, `usage` a table and
/// `stop_reason` a string; anything else the backend returns is ignored
/// here and never reaches the history — a latency or a status code is the
/// backend's own business.  Nothing is written by this function: a
/// rejected result costs no `model_response` and no charge, and the turn
/// stays with the call that gets it right.  What the adapter does on top
/// is note the attempt as a [`KIND_MODEL_CALL_FAILED`], the same way it
/// notes a transport failure.
pub fn validate_backend_result(value: &Value) -> KnlResult<ModelResult> {
    let Value::Object(obj) = value else {
        return Err(KnlError::new(format!(
            "backend result must be a table, got {}",
            json_type_name(value)
        )));
    };

    let content = match obj.get(FIELD_CONTENT) {
        Some(Value::Array(blocks)) if !blocks.is_empty() => blocks.clone(),
        Some(Value::Array(_)) => return Err(mistyped(FIELD_CONTENT, "an empty array")),
        Some(other) => return Err(mistyped(FIELD_CONTENT, json_type_name(other))),
        None => return Err(missing(FIELD_CONTENT, "a non-empty array")),
    };

    let usage = match obj.get(FIELD_USAGE) {
        Some(Value::Object(usage)) => usage.clone(),
        Some(other) => return Err(mistyped(FIELD_USAGE, json_type_name(other))),
        None => return Err(missing(FIELD_USAGE, "a table")),
    };

    let stop_reason = match obj.get(FIELD_STOP_REASON) {
        Some(Value::String(reason)) => reason.clone(),
        Some(other) => return Err(mistyped(FIELD_STOP_REASON, json_type_name(other))),
        None => return Err(missing(FIELD_STOP_REASON, "a string")),
    };

    Ok(ModelResult {
        content,
        usage,
        stop_reason,
    })
}

/// What each field of the contract must be, for an error message.
fn expected(field: &str) -> &'static str {
    match field {
        FIELD_CONTENT => "a non-empty array",
        FIELD_USAGE => "a table",
        _ => "a string",
    }
}

/// A field of the contract that is absent.
fn missing(field: &str, shape: &str) -> KnlError {
    KnlError::new(format!("backend result requires {field:?} ({shape})"))
}

/// A field of the contract that is present but not what it must be.
fn mistyped(field: &str, got: &str) -> KnlError {
    KnlError::new(format!(
        "backend result: {field:?} must be {}, got {got}",
        expected(field)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A result that satisfies the contract.
    fn ok_result() -> Value {
        json!({
            "content": [{ "type": "text", "text": "ok" }],
            "usage": { "input_tokens": 10, "output_tokens": 3 },
            "stop_reason": "end_turn"
        })
    }

    #[test]
    fn a_conforming_result_keeps_the_three_fields_and_drops_the_rest() {
        let mut value = ok_result();
        value["latency_ms"] = json!(42);
        value["status"] = json!(200);

        let result = validate_backend_result(&value).expect("contract met");
        assert_eq!(result.content(), &[json!({ "type": "text", "text": "ok" })]);
        assert_eq!(result.usage()["input_tokens"], json!(10));
        assert_eq!(result.stop_reason(), "end_turn");

        // The surplus never reaches the event either.
        let event = result.to_event(1);
        let mut keys: Vec<&str> = event.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["content", "kind", "stop_reason", "turn", "usage"]
        );
        assert_eq!(event[FIELD_KIND], json!(KIND_MODEL_RESPONSE));
        assert_eq!(event[FIELD_TURN], json!(1));
    }

    #[test]
    fn content_must_be_a_non_empty_array() {
        for (value, expected) in [
            (json!({}), "requires \"content\""),
            (
                json!({ "content": [], "usage": {}, "stop_reason": "end_turn" }),
                "got an empty array",
            ),
            (
                json!({ "content": "text", "usage": {}, "stop_reason": "end_turn" }),
                "must be a non-empty array",
            ),
            (
                json!({ "content": {}, "usage": {}, "stop_reason": "end_turn" }),
                "must be a non-empty array",
            ),
        ] {
            let err = validate_backend_result(&value).expect_err("content check");
            assert!(err.reason().contains(expected), "{value}: {err}");
        }
    }

    #[test]
    fn usage_and_stop_reason_are_required_with_their_shapes() {
        let mut value = ok_result();
        value.as_object_mut().expect("object").remove("usage");
        let err = validate_backend_result(&value).expect_err("usage is required");
        assert!(err.reason().contains("usage"), "{err}");

        let mut value = ok_result();
        value["usage"] = json!(120);
        let err = validate_backend_result(&value).expect_err("usage must be a table");
        assert!(err.reason().contains("must be a table"), "{err}");

        let mut value = ok_result();
        value.as_object_mut().expect("object").remove("stop_reason");
        let err = validate_backend_result(&value).expect_err("stop_reason is required");
        assert!(err.reason().contains("stop_reason"), "{err}");

        let mut value = ok_result();
        value["stop_reason"] = json!(7);
        let err = validate_backend_result(&value).expect_err("stop_reason must be a string");
        assert!(err.reason().contains("must be a string"), "{err}");
    }

    #[test]
    fn a_result_that_is_not_a_table_is_rejected_by_shape_not_by_field() {
        for value in [json!("done"), json!(1), json!([1, 2]), json!(null)] {
            let err = validate_backend_result(&value).expect_err("not a table");
            assert!(err.reason().contains("must be a table"), "{value}: {err}");
        }
    }

    #[test]
    fn the_charge_sums_the_three_counters_and_never_goes_negative() {
        let charge_of = |usage: Value| {
            let value = json!({
                "content": [{ "type": "text" }],
                "usage": usage,
                "stop_reason": "end_turn"
            });
            validate_backend_result(&value)
                .expect("contract met")
                .charge()
        };

        assert_eq!(
            charge_of(json!({ "input_tokens": 10, "output_tokens": 3, "thinking_tokens": 7 })),
            20
        );
        // Missing counters are zero, and unknown ones are not charged.
        assert_eq!(charge_of(json!({ "input_tokens": 5 })), 5);
        assert_eq!(charge_of(json!({})), 0);
        assert_eq!(charge_of(json!({ "cache_read_tokens": 900 })), 0);
        // A provider that reports nonsense cannot hand budget back.
        assert_eq!(charge_of(json!({ "input_tokens": -50 })), 0);
        assert_eq!(charge_of(json!({ "input_tokens": "many" })), 0);
        // Whole floats are counted (Lua numbers arrive as floats).
        assert_eq!(charge_of(json!({ "input_tokens": 4.0 })), 4);
    }

    #[test]
    fn the_recorded_event_passes_the_reserved_kind_check() {
        let result = validate_backend_result(&ok_result()).expect("contract met");
        super::super::validate_event(&result.to_event(3)).expect("a recordable model_response");
    }

    #[test]
    fn a_failure_event_names_the_turn_that_was_not_consumed() {
        let event = failure_event(2, "backend: HTTP 500 after 3 attempts");
        assert_eq!(event[FIELD_KIND], json!(KIND_MODEL_CALL_FAILED));
        assert_eq!(event[FIELD_TURN], json!(2));
        assert_eq!(
            event[FIELD_ERROR],
            json!("backend: HTTP 500 after 3 attempts")
        );
        // Open kind: the kernel accepts it without a required-field table.
        super::super::validate_event(&event).expect("a recordable open kind");
    }
}
