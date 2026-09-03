//! Event envelope and the two-layer `kind` vocabulary.
//!
//! # Envelope
//!
//! Every stored event is a JSON object carrying the kernel-owned fields
//! [`FIELD_SEQ`] (`u64`, starts at 1, strictly increasing) and
//! [`FIELD_EPOCH_MS`] (`u64`, wall clock at append time) plus the
//! caller-owned [`FIELD_KIND`] (string) and an arbitrary payload.  The two
//! kernel-owned fields are stamped by [`stamp`] and overwrite any
//! caller-supplied field of the same name.
//!
//! # Two layers of `kind`
//!
//! - **Reserved kinds** (the table below) describe a Turn's observable
//!   facts.  Their required fields are checked here, so a malformed
//!   `model_response` cannot enter the history.
//! - **Open kinds** — everything else.  Only `kind: string` is checked.
//!   Caller-domain events (decision / note / carry …) live here and their
//!   shape is the shell's business, which is why the kind vocabulary is
//!   deliberately *not* closed in Rust.
//!
//! | reserved kind    | required fields                                      |
//! |------------------|------------------------------------------------------|
//! | `run_started`    | — (appended by the kernel when a session opens)        |
//! | `run_finished`   | `reason: string` (appended by the kernel on close)    |
//! | `msg_user`       | `content: string \| array`                            |
//! | `model_response` | `turn: integer`, `content: array`, `usage: table`     |
//! | `tool_call`      | `turn: integer`, `call_id: string`, `name: string`, `args: table` |
//! | `tool_result`    | `turn: integer`, `call_id: string`, `ok: boolean`, `result` (any) |
//!
//! The literal request/response bytes are not stored: they are derivable
//! from these facts by a projection, and byte-level fidelity is the dump
//! sink's job.  Storing both would be two sources of truth.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::{KnlError, KnlResult};

/// Kernel-owned event field: strictly increasing sequence number.
pub const FIELD_SEQ: &str = "seq";
/// Kernel-owned event field: wall-clock append time in milliseconds.
pub const FIELD_EPOCH_MS: &str = "epoch_ms";
/// Caller-owned event field that every event must carry.
pub const FIELD_KIND: &str = "kind";

/// Reserved kind: a run opened (appended by the kernel).
pub const KIND_RUN_STARTED: &str = "run_started";
/// Reserved kind: a run ended (appended by the kernel on close).
pub const KIND_RUN_FINISHED: &str = "run_finished";
/// Reserved kind: a user message.
pub const KIND_MSG_USER: &str = "msg_user";
/// Reserved kind: a model response (verbatim blocks + usage).
pub const KIND_MODEL_RESPONSE: &str = "model_response";
/// Reserved kind: a tool invocation.
pub const KIND_TOOL_CALL: &str = "tool_call";
/// Reserved kind: a tool result (failures are events too).
pub const KIND_TOOL_RESULT: &str = "tool_result";

/// Payload field of `run_finished`.
pub const FIELD_REASON: &str = "reason";
/// Payload field of `msg_user` / `model_response`.
pub const FIELD_CONTENT: &str = "content";
/// Payload field of `model_response`.
pub const FIELD_USAGE: &str = "usage";
/// Payload field of `tool_call` / `tool_result`.
pub const FIELD_CALL_ID: &str = "call_id";
/// Payload field of `tool_result`.
pub const FIELD_OK: &str = "ok";
/// Payload field of `tool_result`.
pub const FIELD_RESULT: &str = "result";

/// Expected JSON shape of a required field on a reserved kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// A JSON string.
    Str,
    /// A whole number.
    Integer,
    /// A JSON boolean.
    Bool,
    /// A JSON array.
    Array,
    /// A JSON object (a Lua table with string keys).
    Object,
    /// A JSON string or array.
    StringOrArray,
    /// Any value: the field must be present, its shape is the caller's.
    Any,
}

impl Shape {
    /// Lua-facing wording used in error messages.
    fn name(self) -> &'static str {
        match self {
            Shape::Str => "a string",
            Shape::Integer => "a whole number",
            Shape::Bool => "a boolean",
            Shape::Array => "an array",
            Shape::Object => "a table",
            Shape::StringOrArray => "a string or an array",
            Shape::Any => "present",
        }
    }

    /// Whether `value` satisfies this shape.
    fn accepts(self, value: &Value) -> bool {
        match self {
            Shape::Str => value.is_string(),
            Shape::Integer => is_whole_number(value),
            Shape::Bool => value.is_boolean(),
            Shape::Array => value.is_array(),
            Shape::Object => value.is_object(),
            Shape::StringOrArray => value.is_string() || value.is_array(),
            Shape::Any => true,
        }
    }
}

/// Required fields of `run_finished`.
const RUN_FINISHED_FIELDS: &[(&str, Shape)] = &[(FIELD_REASON, Shape::Str)];
/// Required fields of `msg_user`.
const MSG_USER_FIELDS: &[(&str, Shape)] = &[(FIELD_CONTENT, Shape::StringOrArray)];
/// Required fields of `model_response`.
const MODEL_RESPONSE_FIELDS: &[(&str, Shape)] = &[
    ("turn", Shape::Integer),
    (FIELD_CONTENT, Shape::Array),
    (FIELD_USAGE, Shape::Object),
];
/// Required fields of `tool_call`.
const TOOL_CALL_FIELDS: &[(&str, Shape)] = &[
    ("turn", Shape::Integer),
    (FIELD_CALL_ID, Shape::Str),
    ("name", Shape::Str),
    ("args", Shape::Object),
];
/// Required fields of `tool_result`.
const TOOL_RESULT_FIELDS: &[(&str, Shape)] = &[
    ("turn", Shape::Integer),
    (FIELD_CALL_ID, Shape::Str),
    (FIELD_OK, Shape::Bool),
    (FIELD_RESULT, Shape::Any),
];
/// Required fields of `run_started` (none — the kernel appends it).
const RUN_STARTED_FIELDS: &[(&str, Shape)] = &[];

/// The required fields of a reserved kind, or `None` for an open kind.
fn required_fields(kind: &str) -> Option<&'static [(&'static str, Shape)]> {
    match kind {
        KIND_RUN_STARTED => Some(RUN_STARTED_FIELDS),
        KIND_RUN_FINISHED => Some(RUN_FINISHED_FIELDS),
        KIND_MSG_USER => Some(MSG_USER_FIELDS),
        KIND_MODEL_RESPONSE => Some(MODEL_RESPONSE_FIELDS),
        KIND_TOOL_CALL => Some(TOOL_CALL_FIELDS),
        KIND_TOOL_RESULT => Some(TOOL_RESULT_FIELDS),
        _ => None,
    }
}

/// Whether `kind` is part of the reserved (kernel-checked) vocabulary.
pub fn is_reserved(kind: &str) -> bool {
    required_fields(kind).is_some()
}

/// Whether `value` is a number without a fractional part.
fn is_whole_number(value: &Value) -> bool {
    match value {
        Value::Number(n) => {
            n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => false,
    }
}

/// JSON type name used in error messages (Lua-facing wording).
pub fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "nil",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "table",
    }
}

/// Validate an event before it enters the history.
///
/// `kind` must be a string for every event.  For a reserved kind the
/// required fields of the table above are checked as well; an open kind
/// passes through untouched.
pub fn validate_event(obj: &Map<String, Value>) -> KnlResult<()> {
    let kind = match obj.get(FIELD_KIND) {
        Some(Value::String(kind)) => kind.as_str(),
        Some(other) => {
            return Err(KnlError::new(format!(
                "kind must be a string, got {}",
                json_type_name(other)
            )));
        }
        None => return Err(KnlError::new("kind is required (string)")),
    };

    // Open kind: the shell owns the shape, so there is nothing to check.
    let Some(fields) = required_fields(kind) else {
        return Ok(());
    };

    for (name, shape) in fields {
        match obj.get(*name) {
            None => {
                return Err(KnlError::new(format!(
                    "reserved kind {kind:?} requires {name:?} ({})",
                    shape.name()
                )));
            }
            Some(value) if !shape.accepts(value) => {
                return Err(KnlError::new(format!(
                    "reserved kind {kind:?}: {name:?} must be {}, got {}",
                    shape.name(),
                    json_type_name(value)
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Stamp the kernel-owned envelope fields, overwriting caller values.
pub fn stamp(obj: &mut Map<String, Value>, seq: u64, epoch_ms: u64) {
    obj.insert(FIELD_SEQ.to_string(), Value::from(seq));
    obj.insert(FIELD_EPOCH_MS.to_string(), Value::from(epoch_ms));
}

/// The `seq` of a stored event (`0` when absent, which cannot happen for
/// an event that went through [`stamp`]).
pub fn seq_of(event: &Value) -> u64 {
    event.get(FIELD_SEQ).and_then(Value::as_u64).unwrap_or(0)
}

/// The `kind` of a stored event (empty when absent).
pub fn kind_of(event: &Value) -> &str {
    event.get(FIELD_KIND).and_then(Value::as_str).unwrap_or("")
}

/// Build a kernel-authored event with `kind` set.
pub fn kernel_event(kind: &str) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert(FIELD_KIND.to_string(), Value::String(kind.to_string()));
    obj
}

/// Wall-clock milliseconds since the Unix epoch (`0` if the clock is
/// before the epoch — the value is informational and must not abort an
/// append).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Turn a JSON literal into the object map `validate_event` takes.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
    }

    #[test]
    fn kind_must_be_present_and_a_string() {
        let err = validate_event(&obj(json!({ "text": "hi" }))).expect_err("missing kind");
        assert!(err.reason().contains("kind is required"), "{err}");

        let err = validate_event(&obj(json!({ "kind": 42 }))).expect_err("numeric kind");
        assert!(err.reason().contains("kind must be a string"), "{err}");
        assert!(err.reason().contains("number"), "{err}");
    }

    #[test]
    fn open_kinds_pass_through_unchecked() {
        // Any payload at all, including none.
        validate_event(&obj(json!({ "kind": "decision" }))).expect("bare open kind");
        validate_event(&obj(json!({ "kind": "note", "any": { "nested": [1, 2] } })))
            .expect("open kind with payload");
        // Near-misses of the reserved vocabulary are open kinds too.
        validate_event(&obj(json!({ "kind": "user_msg" }))).expect("user_msg is not reserved");
    }

    #[test]
    fn reserved_kinds_accept_their_documented_shape() {
        for event in [
            json!({ "kind": "run_started" }),
            json!({ "kind": "run_finished", "reason": "closed" }),
            json!({ "kind": "msg_user", "content": "hi" }),
            json!({ "kind": "msg_user", "content": [{ "type": "text", "text": "hi" }] }),
            json!({
                "kind": "model_response",
                "turn": 1,
                "content": [{ "type": "text", "text": "ok" }],
                "usage": { "input_tokens": 3 }
            }),
            json!({
                "kind": "tool_call",
                "turn": 1,
                "call_id": "c1",
                "name": "sh",
                "args": { "cmd": "ls" }
            }),
            json!({
                "kind": "tool_result",
                "turn": 1,
                "call_id": "c1",
                "ok": false,
                "result": null
            }),
        ] {
            validate_event(&obj(event.clone())).unwrap_or_else(|e| panic!("{event}: {e}"));
        }
    }

    #[test]
    fn reserved_kinds_reject_a_missing_required_field() {
        let err = validate_event(&obj(json!({ "kind": "run_finished" })))
            .expect_err("reason is required");
        assert!(err.reason().contains("run_finished"), "{err}");
        assert!(err.reason().contains("reason"), "{err}");

        let err = validate_event(&obj(json!({
            "kind": "model_response",
            "turn": 1,
            "content": []
        })))
        .expect_err("usage is required");
        assert!(err.reason().contains("usage"), "{err}");

        let err = validate_event(&obj(json!({
            "kind": "tool_call",
            "turn": 1,
            "call_id": "c1",
            "args": {}
        })))
        .expect_err("name is required");
        assert!(err.reason().contains("name"), "{err}");
    }

    #[test]
    fn reserved_kinds_reject_a_mistyped_required_field() {
        let err = validate_event(&obj(json!({ "kind": "run_finished", "reason": 7 })))
            .expect_err("reason must be a string");
        assert!(err.reason().contains("must be a string"), "{err}");
        assert!(err.reason().contains("number"), "{err}");

        let err = validate_event(&obj(json!({
            "kind": "model_response",
            "turn": 1,
            "content": "not blocks",
            "usage": {}
        })))
        .expect_err("content must be an array");
        assert!(err.reason().contains("an array"), "{err}");

        let err = validate_event(&obj(json!({
            "kind": "tool_result",
            "turn": 1,
            "call_id": "c1",
            "ok": "yes",
            "result": 1
        })))
        .expect_err("ok must be a boolean");
        assert!(err.reason().contains("a boolean"), "{err}");
    }

    #[test]
    fn tool_result_requires_result_to_be_present_but_not_of_any_shape() {
        // Present-but-null is accepted; absent is not.
        validate_event(&obj(json!({
            "kind": "tool_result", "turn": 1, "call_id": "c", "ok": true, "result": null
        })))
        .expect("null result is present");

        let err = validate_event(&obj(json!({
            "kind": "tool_result", "turn": 1, "call_id": "c", "ok": true
        })))
        .expect_err("result is required");
        assert!(err.reason().contains("result"), "{err}");
    }

    #[test]
    fn whole_floats_count_as_integers() {
        // Lua numbers arrive as floats when they are written `1.0`.
        validate_event(&obj(json!({
            "kind": "model_response", "turn": 1.0, "content": [], "usage": {}
        })))
        .expect("1.0 is a whole number");

        let err = validate_event(&obj(json!({
            "kind": "model_response", "turn": 1.5, "content": [], "usage": {}
        })))
        .expect_err("1.5 is not whole");
        assert!(err.reason().contains("whole number"), "{err}");
    }

    #[test]
    fn stamp_overwrites_caller_supplied_envelope_fields() {
        let mut event = obj(json!({ "kind": "note", "seq": 999, "epoch_ms": 1 }));
        stamp(&mut event, 7, 12_345);
        assert_eq!(event.get(FIELD_SEQ).and_then(Value::as_u64), Some(7));
        assert_eq!(
            event.get(FIELD_EPOCH_MS).and_then(Value::as_u64),
            Some(12_345)
        );
    }

    #[test]
    fn reserved_vocabulary_is_exactly_the_six_kinds() {
        for kind in [
            "run_started",
            "run_finished",
            "msg_user",
            "model_response",
            "tool_call",
            "tool_result",
        ] {
            assert!(is_reserved(kind), "{kind} must be reserved");
        }
        for kind in ["note", "decision", "carry", "user_msg", "run_start", ""] {
            assert!(!is_reserved(kind), "{kind} must be open");
        }
    }
}
