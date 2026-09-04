//! Event envelope and the two-layer `kind` vocabulary.
//!
//! # Envelope
//!
//! Every stored event is a JSON object carrying the kernel-owned fields
//! [`FIELD_SEQ`] (`u64`, starts at 1, strictly increasing) and
//! [`FIELD_EPOCH_MS`] (`u64`, wall clock at append time), plus the
//! caller-owned [`FIELD_KIND`] (string) and an arbitrary payload.  The two
//! kernel-owned fields are stamped by [`stamp`] and overwrite any
//! caller-supplied field of the same name.
//!
//! # No per-event author
//!
//! There is no `author` on an event, and no "kernel vs caller" tag.  A
//! session holds only its own events, so who "owns" them is not a per-event
//! question: ownership is a session-level `owner` (a real principal id, or
//! the reserved `anon` / `system` id), total and read by the policy layer
//! above the kernel — see [`super::Session`].  The earlier design inferred
//! ownership from which append path was taken, which made a response the
//! kernel recorded read back as the caller's and fall out of the account
//! (the `usage = 0` bug); that mechanism is gone.
//!
//! Because a session's log is exactly the calls it made, the accounting
//! keys on the `kind` alone: [`super::projection::UsageFold`] counts every
//! `model_response` in the session.  A `model_response` in the history is
//! one this run made.
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
//! | reserved kind     | required fields                                      |
//! |-------------------|------------------------------------------------------|
//! | `run_started`     | — (appended by the kernel when a session opens)       |
//! | `run_finished`    | `reason: string` (appended by the kernel on close)   |
//! | `msg_user`        | `content: string \| array`                           |
//! | `model_response`  | `content: array`, `usage: table` (`beat` is kernel-assigned) |
//! | `tool_call`       | `beat: integer`, `call_id: string`, `name: string`, `args: table` |
//! | `tool_result`     | `beat: integer`, `call_id: string`, `ok: boolean`, `result` (any) |
//! | `budget_granted`  | `amount: integer` (`tag` / `desc` optional)          |
//! | `budget_reserved` | `amount: integer` (`tag` optional)                   |
//! | `budget_refused`  | `amount: integer`, `remaining: integer` (`tag` optional) |
//! | `budget_spent`    | `amount: integer` (`tag` optional)                   |
//!
//! The kernel writes two more fields onto some of those, neither of them
//! required: `owner` and [`FIELD_SCOPE_ID`] on `run_started`, and
//! [`FIELD_SCOPE_ID`] on each `budget_*` event.  They record the scope the
//! run was written under ([`super::Scope`]) so it can be recovered from the
//! log alone; leaving them out of the required set is what lets a log
//! written before they existed still validate and still resume.
//!
//! # Kernel-only kinds
//!
//! The four `budget_*` kinds are the ledger the budget counter is a fold
//! of, so [`is_kernel_only`] marks them and [`super::Session::append`]
//! refuses them: a caller that could write `budget_granted` could grant
//! itself the quota its owner set, and one that could write
//! `budget_reserved` could drain someone else's.  This is a stronger rule
//! than the one the `run_*` kinds live under, and for a reason — a
//! hand-written `run_finished` is a line in the history that changes no
//! state (the run scope is the `closed` flag), while a hand-written
//! `budget_granted` would change what every later read of the balance
//! says.  They are written only by [`super::Session`], through the same
//! append, so they carry the same `seq` / `epoch_ms` guarantees as
//! everything else.
//!
//! `model_response` does not require a caller-supplied `beat`: the kernel
//! numbers it from its own count of the responses recorded (like `seq`), so
//! the number cannot be forged or restarted by a loop.  `tool_call` /
//! `tool_result` keep their `beat` field — they reference the beat the model
//! call belongs to, which the shell learns after the response is appended
//! and supplies.
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
/// Kernel-only kind: an owner granted the run a quota.
pub const KIND_BUDGET_GRANTED: &str = "budget_granted";
/// Kernel-only kind: a reservation was allowed, and the balance fell by it.
pub const KIND_BUDGET_RESERVED: &str = "budget_reserved";
/// Kernel-only kind: a reservation was refused; the balance did not move.
pub const KIND_BUDGET_REFUSED: &str = "budget_refused";
/// Kernel-only kind: a settlement after the fact, deducted from the balance.
pub const KIND_BUDGET_SPENT: &str = "budget_spent";

/// Payload field of `run_finished`.
pub const FIELD_REASON: &str = "reason";
/// Optional payload field of `run_finished`: free text about the close that
/// wrote it — the message of the error that ended a scope, say.  It is kept
/// out of [`FIELD_REASON`] on purpose: the reason is a small vocabulary a
/// reader can fold on, and an error message is not part of it.
pub const FIELD_DETAIL: &str = "detail";
/// Payload field of `model_response` / `tool_call` / `tool_result`: which
/// beat — which model call — the fact belongs to.  On a `model_response`
/// the kernel assigns it; on `tool_call` / `tool_result` the shell supplies
/// it.
pub const FIELD_BEAT: &str = "beat";
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
/// Payload field of every `budget_*` kind: how much, in the grant's unit.
pub const FIELD_AMOUNT: &str = "amount";
/// Payload field of `budget_*`: the grant's unit / identity, if it named
/// one.  Optional and kernel-uninterpreted — it rides along so a log can
/// be read without asking the shell what the numbers counted.
pub const FIELD_TAG: &str = "tag";
/// Payload field of `budget_granted`: the owner's free-text note.
pub const FIELD_DESC: &str = "desc";
/// Payload field of `budget_refused`: the balance at the moment of the
/// refusal, which the refusal did not change.
pub const FIELD_REMAINING: &str = "remaining";
/// Payload field of `run_started` and of every `budget_*` kind: the
/// kernel-issued id of the scope the event was written under
/// ([`super::Scope`]).
///
/// It rides on the events that define the boundary — the run's opening and
/// every move of its balance — so a reader can tell whose authority a
/// ledger entry was written with from the log alone, and a resume can
/// restore the scope without being told.  Both kinds are open-shaped as far
/// as this field is concerned (nothing below requires it), so a log written
/// before it existed still validates and still resumes.
pub const FIELD_SCOPE_ID: &str = "scope_id";

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
///
/// `beat` is not among them: the kernel assigns it on append, so a caller
/// does not have to supply it and cannot renumber the run by doing so.
const MODEL_RESPONSE_FIELDS: &[(&str, Shape)] =
    &[(FIELD_CONTENT, Shape::Array), (FIELD_USAGE, Shape::Object)];
/// Required fields of `tool_call`.
const TOOL_CALL_FIELDS: &[(&str, Shape)] = &[
    (FIELD_BEAT, Shape::Integer),
    (FIELD_CALL_ID, Shape::Str),
    ("name", Shape::Str),
    ("args", Shape::Object),
];
/// Required fields of `tool_result`.
const TOOL_RESULT_FIELDS: &[(&str, Shape)] = &[
    (FIELD_BEAT, Shape::Integer),
    (FIELD_CALL_ID, Shape::Str),
    (FIELD_OK, Shape::Bool),
    (FIELD_RESULT, Shape::Any),
];
/// Required fields of `run_started` (none — the kernel appends it).
const RUN_STARTED_FIELDS: &[(&str, Shape)] = &[];
/// Required fields of `budget_granted` (`tag` / `desc` are optional).
const BUDGET_GRANTED_FIELDS: &[(&str, Shape)] = &[(FIELD_AMOUNT, Shape::Integer)];
/// Required fields of `budget_reserved` / `budget_spent` (`tag` optional).
const BUDGET_MOVE_FIELDS: &[(&str, Shape)] = &[(FIELD_AMOUNT, Shape::Integer)];
/// Required fields of `budget_refused`: what was asked for, and what there
/// was — the pair that makes the refusal readable without a fold.
const BUDGET_REFUSED_FIELDS: &[(&str, Shape)] = &[
    (FIELD_AMOUNT, Shape::Integer),
    (FIELD_REMAINING, Shape::Integer),
];

/// The required fields of a reserved kind, or `None` for an open kind.
fn required_fields(kind: &str) -> Option<&'static [(&'static str, Shape)]> {
    match kind {
        KIND_RUN_STARTED => Some(RUN_STARTED_FIELDS),
        KIND_RUN_FINISHED => Some(RUN_FINISHED_FIELDS),
        KIND_MSG_USER => Some(MSG_USER_FIELDS),
        KIND_MODEL_RESPONSE => Some(MODEL_RESPONSE_FIELDS),
        KIND_TOOL_CALL => Some(TOOL_CALL_FIELDS),
        KIND_TOOL_RESULT => Some(TOOL_RESULT_FIELDS),
        KIND_BUDGET_GRANTED => Some(BUDGET_GRANTED_FIELDS),
        KIND_BUDGET_RESERVED | KIND_BUDGET_SPENT => Some(BUDGET_MOVE_FIELDS),
        KIND_BUDGET_REFUSED => Some(BUDGET_REFUSED_FIELDS),
        _ => None,
    }
}

/// Whether `kind` is part of the reserved (kernel-checked) vocabulary.
pub fn is_reserved(kind: &str) -> bool {
    required_fields(kind).is_some()
}

/// Whether `kind` is one only the kernel may write.
///
/// The `budget_*` kinds are the balance: it is a fold of them, so writing
/// one *is* moving the account.  [`super::Session::append`] refuses them
/// from a caller for that reason, and the kernel writes them on its own
/// path — see the module docs.
pub fn is_kernel_only(kind: &str) -> bool {
    matches!(
        kind,
        KIND_BUDGET_GRANTED | KIND_BUDGET_RESERVED | KIND_BUDGET_REFUSED | KIND_BUDGET_SPENT
    )
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
///
/// `seq` and `epoch_ms` are the kernel's to assign, so a payload that
/// carries either gets the kernel's value instead.
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

/// Build an event with `kind` set, for the kernel's own reserved writes.
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
            // A model_response with no beat is valid: the kernel assigns it.
            json!({
                "kind": "model_response",
                "content": [{ "type": "text", "text": "ok" }],
                "usage": { "input_tokens": 3 }
            }),
            // …and one that carries a beat is valid too (an extra field).
            json!({
                "kind": "model_response",
                "beat": 1,
                "content": [{ "type": "text", "text": "ok" }],
                "usage": { "input_tokens": 3 }
            }),
            json!({
                "kind": "tool_call",
                "beat": 1,
                "call_id": "c1",
                "name": "sh",
                "args": { "cmd": "ls" }
            }),
            json!({
                "kind": "tool_result",
                "beat": 1,
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
            "content": []
        })))
        .expect_err("usage is required");
        assert!(err.reason().contains("usage"), "{err}");

        let err = validate_event(&obj(json!({
            "kind": "tool_call",
            "beat": 1,
            "call_id": "c1",
            "args": {}
        })))
        .expect_err("name is required");
        assert!(err.reason().contains("name"), "{err}");
    }

    #[test]
    fn model_response_does_not_require_a_beat() {
        // No beat field at all is fine — the kernel numbers it on append.
        validate_event(&obj(json!({
            "kind": "model_response",
            "content": [],
            "usage": {}
        })))
        .expect("beat is kernel-assigned, not caller-required");
    }

    #[test]
    fn reserved_kinds_reject_a_mistyped_required_field() {
        let err = validate_event(&obj(json!({ "kind": "run_finished", "reason": 7 })))
            .expect_err("reason must be a string");
        assert!(err.reason().contains("must be a string"), "{err}");
        assert!(err.reason().contains("number"), "{err}");

        let err = validate_event(&obj(json!({
            "kind": "model_response",
            "content": "not blocks",
            "usage": {}
        })))
        .expect_err("content must be an array");
        assert!(err.reason().contains("an array"), "{err}");

        let err = validate_event(&obj(json!({
            "kind": "tool_result",
            "beat": 1,
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
            "kind": "tool_result", "beat": 1, "call_id": "c", "ok": true, "result": null
        })))
        .expect("null result is present");

        let err = validate_event(&obj(json!({
            "kind": "tool_result", "beat": 1, "call_id": "c", "ok": true
        })))
        .expect_err("result is required");
        assert!(err.reason().contains("result"), "{err}");
    }

    #[test]
    fn whole_floats_count_as_integers() {
        // Lua numbers arrive as floats when they are written `1.0`.  A
        // tool_call's beat is a caller-supplied integer.
        validate_event(&obj(json!({
            "kind": "tool_call", "beat": 1.0, "call_id": "c", "name": "sh", "args": {}
        })))
        .expect("1.0 is a whole number");

        let err = validate_event(&obj(json!({
            "kind": "tool_call", "beat": 1.5, "call_id": "c", "name": "sh", "args": {}
        })))
        .expect_err("1.5 is not whole");
        assert!(err.reason().contains("whole number"), "{err}");
    }

    #[test]
    fn stamp_overwrites_caller_supplied_envelope_fields() {
        let mut event = obj(json!({
            "kind": "note", "seq": 999, "epoch_ms": 1
        }));
        stamp(&mut event, 7, 12_345);
        assert_eq!(event.get(FIELD_SEQ).and_then(Value::as_u64), Some(7));
        assert_eq!(
            event.get(FIELD_EPOCH_MS).and_then(Value::as_u64),
            Some(12_345)
        );
    }

    #[test]
    fn reserved_vocabulary_is_exactly_the_ten_kinds() {
        for kind in [
            "run_started",
            "run_finished",
            "msg_user",
            "model_response",
            "tool_call",
            "tool_result",
            "budget_granted",
            "budget_reserved",
            "budget_refused",
            "budget_spent",
        ] {
            assert!(is_reserved(kind), "{kind} must be reserved");
        }
        for kind in ["note", "decision", "carry", "user_msg", "run_start", ""] {
            assert!(!is_reserved(kind), "{kind} must be open");
        }
    }

    /// The budget ledger is the kernel's alone: writing one of these kinds
    /// is moving the account, so no other kind — reserved or open — joins
    /// them.
    #[test]
    fn only_the_budget_kinds_are_kernel_only() {
        for kind in [
            "budget_granted",
            "budget_reserved",
            "budget_refused",
            "budget_spent",
        ] {
            assert!(is_kernel_only(kind), "{kind} must be kernel-only");
            assert!(is_reserved(kind), "a kernel-only kind is reserved too");
        }
        for kind in [
            "run_started",
            "run_finished",
            "msg_user",
            "model_response",
            "tool_call",
            "tool_result",
            "note",
            "budget",
            "",
        ] {
            assert!(!is_kernel_only(kind), "{kind} must not be kernel-only");
        }
    }

    #[test]
    fn budget_kinds_require_their_amounts() {
        for event in [
            json!({ "kind": "budget_granted", "amount": 100 }),
            json!({ "kind": "budget_granted", "amount": 100, "tag": "tokens", "desc": "one run" }),
            json!({ "kind": "budget_reserved", "amount": 12, "tag": "tokens" }),
            json!({ "kind": "budget_spent", "amount": 3 }),
            json!({ "kind": "budget_refused", "amount": 40, "remaining": 7 }),
        ] {
            validate_event(&obj(event.clone())).unwrap_or_else(|e| panic!("{event}: {e}"));
        }

        let err = validate_event(&obj(json!({ "kind": "budget_reserved" })))
            .expect_err("amount is required");
        assert!(err.reason().contains("amount"), "{err}");

        // A refusal without the balance it refused against is half a fact.
        let err = validate_event(&obj(json!({ "kind": "budget_refused", "amount": 5 })))
            .expect_err("remaining is required");
        assert!(err.reason().contains("remaining"), "{err}");

        let err = validate_event(&obj(json!({ "kind": "budget_spent", "amount": "lots" })))
            .expect_err("amount must be a number");
        assert!(err.reason().contains("whole number"), "{err}");
    }

    /// Validation is about shape only: `run_*` and `model_response` are as
    /// acceptable from a caller as from the kernel, because a hand-written
    /// one moves no state — the run scope is the `closed` flag, and a
    /// session holds only its own events, so the usage fold has nothing
    /// foreign to count.  The `budget_*` kinds are the exception, and they
    /// are stopped by [`is_kernel_only`] on the append path rather than
    /// here: their shape is fine, it is the authorship that is not.
    #[test]
    fn the_kinds_the_kernel_writes_are_valid_from_either_side() {
        for event in [
            json!({ "kind": "run_started" }),
            json!({ "kind": "run_finished", "reason": "carried over" }),
            json!({
                "kind": "model_response",
                "content": [{ "type": "text", "text": "said last time" }],
                "usage": { "input_tokens": 9_000 }
            }),
        ] {
            validate_event(&obj(event.clone())).unwrap_or_else(|e| panic!("{event}: {e}"));
        }
    }
}
