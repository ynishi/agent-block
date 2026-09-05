//! The event envelope, and the kernel's own `kind` vocabulary.
//!
//! # The stored shape is envelope + meta + data
//!
//! An event is a JSON object with four caller-facing keys and three the
//! kernel stamps, **and no others**:
//!
//! | key                     | written by | what it is                                                            |
//! |-------------------------|------------|-----------------------------------------------------------------------|
//! | [`FIELD_KIND`]          | the caller | required, a string: what happened                                      |
//! | [`FIELD_BEAT`]          | the caller | optional, a string: the beat this fact belongs to                      |
//! | [`FIELD_META`]          | the caller | optional, a **shallow** object: string / number / boolean values only   |
//! | [`FIELD_DATA`]          | the caller | optional (default `{}`), an object: the kind's own content, any depth   |
//! | [`FIELD_SEQ`]           | the kernel | `u64`, starts at 1, strictly increasing                                |
//! | [`FIELD_EPOCH_MS`]      | the kernel | `u64`, wall clock at append time                                       |
//! | `_schema_version`       | the kernel | the shape the event was written under                                  |
//!
//! A top-level key that is none of those is refused
//! ([`KnlError::Validation`]).  That is the whole point of the split: an
//! event used to be one flat object, so an envelope key and a kind's own
//! field sat at the same level and a reader — a SQL view most of all — could
//! not tell which of them it was reading.  A change to what one kind records
//! then broke a `json_extract` path silently, because nothing said where a
//! kind's shape ended and the log's did.
//!
//! So the three levels are separated by rule:
//!
//! - the **envelope** is the stable contract.  Its keys are never renamed;
//!   they are columns of the log's table ([`super::sqlite_store`]) and a view
//!   built on them is not affected by any kind changing shape.
//! - **`meta`** is shallow *by rule* — its values are scalars — so it can be
//!   read without knowing the kind.  It is the place for a correlation value,
//!   a label, a flag: anything a reader groups or filters by.  A nested value
//!   there is refused, and the refusal says where it goes instead.
//! - **`data`** is the one place structured JSON lives, and its shape belongs
//!   to whoever writes the kind.  A view that reads a `data` path is updated
//!   in the same round as the kind whose shape it reads — which is a rule a
//!   reader can actually follow, because the paths that need watching are all
//!   under one key.
//!
//! # Who owns which shape
//!
//! The kernel validates the envelope for every event, and the `data` of its
//! own six kinds — the ones [`is_kernel_only`] names, which are the only ones
//! it writes:
//!
//! | kernel kind       | required `data`                                | optional `data`             |
//! |-------------------|------------------------------------------------|-----------------------------|
//! | `session_opened`  | `scope_id: string`, `owner: string`            | `parent`                    |
//! | `session_closed`  | `reason: string`                               | `detail`, `open_children`   |
//! | `budget_granted`  | `amount: integer`                              | `tag`, `desc`, `parent`     |
//! | `budget_reserved` | `amount: integer`                              | `tag`, `child`              |
//! | `budget_refused`  | `amount: integer`, `remaining: integer`        | `tag`, `child`              |
//! | `budget_spent`    | `amount: integer`                              | `tag`                       |
//!
//! The four optional fields on the right of that table are the whole of what
//! the kernel records about the *structure* between sessions, and they are
//! written on one occasion each: an allocation
//! ([`super::Session::open_child`]) records the child's [`FIELD_PARENT`] on
//! its opening and on the grant it opened with, and the parent's
//! [`FIELD_CHILD`] on the reservation (or the refusal) that paid for it; a
//! close records [`FIELD_OPEN_CHILDREN`] when children of this session had
//! not ended yet.  They are facts, not a tree: what a tree *is* — whether an
//! open child should stop a close, who may allocate to whom, what to do about
//! a subtree that outlived its root — belongs to the supervisor above the
//! kernel, which reads them back with a query.
//!
//! Every other kind passes with its envelope checked and its `data` untouched.
//! That includes the kinds a turn is made of — `msg_user`, `llm_request`,
//! `llm_response`, `llm_call_failed`, `tool_call`, `tool_result` — which the
//! Lua kernel writes and therefore owns: their shapes are declared over there
//! (`knl.shapes`), where the code that writes them is.  The kernel used to
//! hold a second copy of those requirements, which made it the one place a
//! shell could not change its own event without a Rust round.
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
//! Because a session's log is exactly the calls it made, an accounting of
//! what it consumed keys on the `kind` alone: an `llm_response` in the
//! history is one this run made, so a reader summing the counts has nothing
//! foreign to exclude and no author to key on.  That reader is not the
//! kernel.  The totals are a query view over the recorded `data`
//! (`knl.views.usage`, SQL over the published schema); what the kernel does
//! is keep the `data` verbatim.
//!
//! # Kernel-only kinds
//!
//! Six kinds are the kernel's alone to write, and [`is_kernel_only`] marks
//! them so [`super::Session::append`] refuses them from a caller.
//!
//! The four `budget_*` kinds are the ledger the budget counter is a fold of,
//! so writing one *is* moving the account: a caller that could write
//! `budget_granted` could grant itself the quota its owner set, and one that
//! could write `budget_reserved` could drain someone else's.
//!
//! `session_opened` / `session_closed` are the session's own boundaries.
//! The kernel has no separate "run" inside a session — the lifecycle *is*
//! the session's — so the two events that bracket it are written by
//! [`super::Session`] alone, on open and on close.  A hand-written boundary
//! would be a stream that claims an opening it never had, or an ending it
//! did not reach, and both are what a resume and an audit read.
//!
//! All six are written through the same append, so they carry the same
//! `seq` / `epoch_ms` guarantees as everything else.
//!
//! # `beat` is the caller's word
//!
//! [`FIELD_BEAT`] is an opaque, caller-declared string — the id of the beat
//! a fact belongs to, minted by the layer above (`knl.new_beat_id()` on the
//! Lua side).  The kernel neither requires it nor generates it: no kind lists
//! it among its required fields, and no append stamps it.  It is validated in
//! one respect only, and on every kind: when present it must be a string, so
//! a stream cannot mix a number and a string under one name.
//!
//! It is an envelope key rather than a `meta` entry because it is the one
//! correlation the log itself is indexed by — it has a column and an index of
//! its own ([`super::sqlite_store`]), so grouping a run by beat is a plain
//! `GROUP BY` and not a `json_extract`.
//!
//! The literal request/response bytes are not stored: they are derivable
//! from these facts by a projection, and byte-level fidelity is the dump
//! sink's job.  Storing both would be two sources of truth.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::event_store::SCHEMA_VERSION_FIELD;
use super::{KnlError, KnlResult};

/// Kernel-owned event field: strictly increasing sequence number.
pub const FIELD_SEQ: &str = "seq";
/// Kernel-owned event field: wall-clock append time in milliseconds.
pub const FIELD_EPOCH_MS: &str = "epoch_ms";
/// Caller-owned envelope key that every event must carry.
pub const FIELD_KIND: &str = "kind";
/// Caller-owned envelope key: which beat the fact belongs to.
///
/// An opaque string the caller declares (`knl.new_beat_id()` mints a
/// time-ordered one).  The kernel never requires it and never generates it;
/// it only insists that a present `beat` is a string — see the module docs.
pub const FIELD_BEAT: &str = "beat";
/// Caller-owned envelope key: a shallow object of scalars.
///
/// Values are a string, a number or a boolean; a nested object or array is
/// refused, because the point of `meta` is that it can be read without
/// knowing the kind.  Structure goes under [`FIELD_DATA`].
pub const FIELD_META: &str = "meta";
/// Caller-owned envelope key: the kind's own content, at any depth.
///
/// Defaults to `{}` when the caller writes none, so every stored event has
/// one.  The shape under here belongs to whoever writes the kind — the
/// kernel checks only its own six ([`is_kernel_only`]).
pub const FIELD_DATA: &str = "data";

/// Every key an event may carry at the top level.
///
/// The closed list the stray-key check reads.  Four are the caller's
/// ([`FIELD_KIND`] / [`FIELD_BEAT`] / [`FIELD_META`] / [`FIELD_DATA`]) and
/// three are stamped by the kernel; anything else is a kind's own field that
/// belongs under `data`.
pub const ENVELOPE_FIELDS: &[&str] = &[
    FIELD_KIND,
    FIELD_BEAT,
    FIELD_META,
    FIELD_DATA,
    FIELD_SEQ,
    FIELD_EPOCH_MS,
    SCHEMA_VERSION_FIELD,
];

/// Kernel-only kind: the session opened (appended by the kernel).
pub const KIND_SESSION_OPENED: &str = "session_opened";
/// Kernel-only kind: the session ended (appended by the kernel on close).
pub const KIND_SESSION_CLOSED: &str = "session_closed";
/// Kernel-only kind: an owner granted the run a quota.
pub const KIND_BUDGET_GRANTED: &str = "budget_granted";
/// Kernel-only kind: a reservation was allowed, and the balance fell by it.
pub const KIND_BUDGET_RESERVED: &str = "budget_reserved";
/// Kernel-only kind: a reservation was refused; the balance did not move.
pub const KIND_BUDGET_REFUSED: &str = "budget_refused";
/// Kernel-only kind: a settlement after the fact, deducted from the balance.
pub const KIND_BUDGET_SPENT: &str = "budget_spent";

/// The four kinds the ledger is made of — everything
/// [`super::fold_balance`] reads, and nothing else.
///
/// Named here so the reads that fold the balance
/// ([`super::EventStore::read_kinds`]) ask for exactly the kinds the fold
/// looks at: one list, so a kind cannot be added to the ledger and left out
/// of the read that folds it.  `budget_refused` moves no balance and is in
/// the list all the same — it is a ledger entry, and a fold that could not
/// see it would be reading a different log from the one an audit reads.
pub const BUDGET_KINDS: &[&str] = &[
    KIND_BUDGET_GRANTED,
    KIND_BUDGET_RESERVED,
    KIND_BUDGET_REFUSED,
    KIND_BUDGET_SPENT,
];

/// `data` field of `session_closed`.
pub const FIELD_REASON: &str = "reason";
/// Optional `data` field of `session_closed`: free text about the close
/// that wrote it — the message of the error that ended a scope, say.  It is
/// kept out of [`FIELD_REASON`] on purpose: the reason is a small vocabulary
/// a reader can fold on, and an error message is not part of it.
pub const FIELD_DETAIL: &str = "detail";
/// `data` field of every `budget_*` kind: how much, in the grant's unit.
pub const FIELD_AMOUNT: &str = "amount";
/// `data` field of `budget_*`: the grant's unit / identity, if it named
/// one.  Optional and kernel-uninterpreted — it rides along so a log can
/// be read without asking the shell what the numbers counted.
pub const FIELD_TAG: &str = "tag";
/// `data` field of `budget_granted`: the owner's free-text note.
pub const FIELD_DESC: &str = "desc";
/// `data` field of `budget_refused`: the balance at the moment of the
/// refusal, which the refusal did not change.
pub const FIELD_REMAINING: &str = "remaining";
/// `data` field of `session_opened`: the principal the session's scope
/// belongs to (a real id, or the reserved [`super::ANON`] /
/// [`super::SYSTEM`]).
///
/// Recorded so a later [`super::Session::resume`] can restore the scope from
/// the log alone rather than being told what it was.
pub const FIELD_OWNER: &str = "owner";
/// `data` field of `session_opened` and of every `budget_*` kind: the
/// kernel-issued id of the scope the event was written under
/// ([`super::Scope`]).
///
/// It rides on the events that define the boundary — the session's opening
/// and every move of its balance — so a reader can tell whose authority a
/// ledger entry was written with from the log alone.
pub const FIELD_SCOPE_ID: &str = "scope_id";
/// Optional `data` field of `session_opened` and of the `budget_granted`
/// that opened a child: the stream of the session this one was allocated
/// from.
///
/// Absent on a root, which is what makes a root a root: there is no
/// "parent = null" state to tell from an unrecorded one.  Written by
/// [`super::Session::open_child`] alone, on a kind only the kernel writes,
/// so a session cannot claim a parent it was not given one by.
pub const FIELD_PARENT: &str = "parent";
/// Optional `data` field of `budget_reserved` / `budget_refused`: the stream
/// the amount was allocated to (or would have been).
///
/// The other end of [`FIELD_PARENT`], on the parent's side of the ledger, so
/// the entry that paid for a child names it — an allocation reads as a spend
/// with a destination rather than as an unexplained reservation.
pub const FIELD_CHILD: &str = "child";
/// Optional `data` field of `session_closed`: the streams that named this
/// session as their parent and had not recorded an ending of their own when
/// it closed.
///
/// A record, not a refusal.  The log never turns a write away, so a close
/// with open children lands like any other and says so; what to do about it
/// is the supervisor's.  Absent when there were none.
pub const FIELD_OPEN_CHILDREN: &str = "open_children";

/// Expected JSON shape of a required `data` field on a kernel kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// A JSON string.
    Str,
    /// A whole number.
    Integer,
}

impl Shape {
    /// Lua-facing wording used in error messages.
    fn name(self) -> &'static str {
        match self {
            Shape::Str => "a string",
            Shape::Integer => "a whole number",
        }
    }

    /// Whether `value` satisfies this shape.
    fn accepts(self, value: &Value) -> bool {
        match self {
            Shape::Str => value.is_string(),
            Shape::Integer => is_whole_number(value),
        }
    }
}

/// Required `data` of `session_opened`: the scope the session opened under.
///
/// Both are the kernel's own writes, and both are what a resume reads back to
/// restore the scope — so a `session_opened` without them is an opening that
/// cannot be resumed, and it is refused here rather than discovered there.
const SESSION_OPENED_DATA: &[(&str, Shape)] =
    &[(FIELD_SCOPE_ID, Shape::Str), (FIELD_OWNER, Shape::Str)];
/// Required `data` of `session_closed` (`detail` is optional).
const SESSION_CLOSED_DATA: &[(&str, Shape)] = &[(FIELD_REASON, Shape::Str)];
/// Required `data` of `budget_granted` (`tag` / `desc` are optional).
const BUDGET_GRANTED_DATA: &[(&str, Shape)] = &[(FIELD_AMOUNT, Shape::Integer)];
/// Required `data` of `budget_reserved` / `budget_spent` (`tag` optional).
const BUDGET_MOVE_DATA: &[(&str, Shape)] = &[(FIELD_AMOUNT, Shape::Integer)];
/// Required `data` of `budget_refused`: what was asked for, and what there
/// was — the pair that makes the refusal readable without a fold.
const BUDGET_REFUSED_DATA: &[(&str, Shape)] = &[
    (FIELD_AMOUNT, Shape::Integer),
    (FIELD_REMAINING, Shape::Integer),
];

/// The required `data` fields of a kernel kind, or `None` for every other
/// kind — whose `data` shape belongs to whoever writes it.
fn required_data(kind: &str) -> Option<&'static [(&'static str, Shape)]> {
    match kind {
        KIND_SESSION_OPENED => Some(SESSION_OPENED_DATA),
        KIND_SESSION_CLOSED => Some(SESSION_CLOSED_DATA),
        KIND_BUDGET_GRANTED => Some(BUDGET_GRANTED_DATA),
        KIND_BUDGET_RESERVED | KIND_BUDGET_SPENT => Some(BUDGET_MOVE_DATA),
        KIND_BUDGET_REFUSED => Some(BUDGET_REFUSED_DATA),
        _ => None,
    }
}

/// Whether the kernel checks this kind's `data`.
///
/// Exactly the six kinds of [`is_kernel_only`], and that is not a
/// coincidence: the kernel holds the shape of what it writes itself, and of
/// nothing else.  The two predicates answer different questions — "is this
/// shape mine to check" and "is this kind mine to write" — and today they
/// have the same answer for every kind, which is what "the writer owns the
/// shape" means when it is followed all the way through.
pub fn is_reserved(kind: &str) -> bool {
    required_data(kind).is_some()
}

/// Whether `kind` is one only the kernel may write.
///
/// The `budget_*` kinds are the balance: it is a fold of them, so writing
/// one *is* moving the account.  `session_opened` / `session_closed` are the
/// session's own boundaries, and a caller that could write one could claim
/// an opening the stream never had.  [`super::Session::append`] refuses all
/// six from a caller, and the kernel writes them on its own path — see the
/// module docs.
pub fn is_kernel_only(kind: &str) -> bool {
    matches!(
        kind,
        KIND_SESSION_OPENED
            | KIND_SESSION_CLOSED
            | KIND_BUDGET_GRANTED
            | KIND_BUDGET_RESERVED
            | KIND_BUDGET_REFUSED
            | KIND_BUDGET_SPENT
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

/// A field of a stored event's `data`, or `None` when either is absent.
///
/// The one reading of the split every fold in the kernel goes through, so
/// "the kind's own fields live under `data`" is written once rather than
/// spelled out at each call site.
pub fn data_field<'a>(event: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    event.get(FIELD_DATA)?.as_object()?.get(name)
}

/// Validate an event before it enters the history.
///
/// Two checks, in this order:
///
/// 1. **the envelope**, on every kind — `kind` is a string, a present `beat`
///    is a string, a present `meta` is an object of scalars, a present `data`
///    is an object, and there is no other top-level key;
/// 2. **the `data` of a kernel kind** — the table in the module docs.  Every
///    other kind's `data` is its writer's, and passes through untouched.
pub fn validate_event(obj: &Map<String, Value>) -> KnlResult<()> {
    let kind = match obj.get(FIELD_KIND) {
        Some(Value::String(kind)) => kind.as_str(),
        Some(other) => {
            return Err(KnlError::Validation(format!(
                "kind must be a string, got {}",
                json_type_name(other)
            )));
        }
        None => {
            return Err(KnlError::Validation(
                "kind is required (string)".to_string(),
            ))
        }
    };

    // The envelope is a closed set of keys.  A field that is none of them is
    // a kind's own, and a kind's own fields live under `data` — where a
    // reader can tell them apart from the log's own vocabulary, and where a
    // change to them cannot be mistaken for a change to the envelope.
    for key in obj.keys() {
        if !ENVELOPE_FIELDS.contains(&key.as_str()) {
            return Err(KnlError::Validation(format!(
                "{key:?} is not part of the envelope (kind / beat / meta / data); a kind's own \
                 fields go under data"
            )));
        }
    }

    // The beat is the caller's to declare and never the kernel's to mint,
    // but it is one name across the whole stream: a present `beat` is a
    // string on every kind, so a reader never has to ask whether this one is
    // a number.
    match obj.get(FIELD_BEAT) {
        None => {}
        Some(Value::String(_)) => {}
        Some(other) => {
            return Err(KnlError::Validation(format!(
                "beat must be a string, got {}",
                json_type_name(other)
            )));
        }
    }

    // `meta` is shallow by rule, which is what lets a reader group or filter
    // on it without knowing the kind.  Nesting is refused rather than
    // flattened, and the refusal says where the nested value belongs.
    match obj.get(FIELD_META) {
        None => {}
        Some(Value::Object(meta)) => {
            for (name, value) in meta {
                if !matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
                    return Err(KnlError::Validation(format!(
                        "meta is shallow: {name:?} must be a string, a number or a boolean, got \
                         {} (nest it under data)",
                        json_type_name(value)
                    )));
                }
            }
        }
        Some(other) => {
            return Err(KnlError::Validation(format!(
                "meta must be a table, got {}",
                json_type_name(other)
            )));
        }
    }

    let data = match obj.get(FIELD_DATA) {
        None => None,
        Some(Value::Object(data)) => Some(data),
        Some(other) => {
            return Err(KnlError::Validation(format!(
                "data must be a table, got {}",
                json_type_name(other)
            )));
        }
    };

    // Not a kernel kind: its `data` belongs to whoever writes it, so there is
    // nothing here to check.
    let Some(fields) = required_data(kind) else {
        return Ok(());
    };

    // An absent `data` is the empty one — it defaults to `{}` on the way in
    // ([`stamp`]) — so a kernel kind that needs a field reports the field it
    // is missing rather than the `data` it has none of.
    let empty = Map::new();
    let data = data.unwrap_or(&empty);
    for (name, shape) in fields {
        match data.get(*name) {
            None => {
                return Err(KnlError::Validation(format!(
                    "kernel kind {kind:?} requires data.{name} ({})",
                    shape.name()
                )));
            }
            Some(value) if !shape.accepts(value) => {
                return Err(KnlError::Validation(format!(
                    "kernel kind {kind:?}: data.{name} must be {}, got {}",
                    shape.name(),
                    json_type_name(value)
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Stamp the kernel-owned envelope fields and fill the envelope's defaults.
///
/// `seq` and `epoch_ms` are the kernel's to assign, so an event that carries
/// either gets the kernel's value instead.  `meta` and `data` are filled in
/// with `{}` when the caller wrote none, so every *stored* event carries the
/// whole envelope: a reader never has to ask whether the key it wants is
/// missing because nothing was written or because the writer left it out.
///
/// Called on every write path, right before the event is recorded, which is
/// what makes the default a property of the stored shape rather than of one
/// backend.
pub fn stamp(obj: &mut Map<String, Value>, seq: u64, epoch_ms: u64) {
    obj.insert(FIELD_SEQ.to_string(), Value::from(seq));
    obj.insert(FIELD_EPOCH_MS.to_string(), Value::from(epoch_ms));
    obj.entry(FIELD_META).or_insert_with(empty_object);
    obj.entry(FIELD_DATA).or_insert_with(empty_object);
}

/// An empty JSON object — the default of `meta` and of `data`.
fn empty_object() -> Value {
    Value::Object(Map::new())
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

/// Build a kernel-written event: its `kind`, and the `data` it records.
///
/// The kernel's own writes go through this one constructor, so a kernel kind
/// cannot be built with its fields left at the top level — the split is in
/// the shape of the call rather than in what each site remembered to do.
pub fn kernel_event(kind: &str, data: Map<String, Value>) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert(FIELD_KIND.to_string(), Value::String(kind.to_string()));
    obj.insert(FIELD_DATA.to_string(), Value::Object(data));
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
        let err =
            validate_event(&obj(json!({ "data": { "text": "hi" } }))).expect_err("missing kind");
        assert!(err.reason().contains("kind is required"), "{err}");

        let err = validate_event(&obj(json!({ "kind": 42 }))).expect_err("numeric kind");
        assert!(err.reason().contains("kind must be a string"), "{err}");
        assert!(err.reason().contains("number"), "{err}");
    }

    /// The envelope is a closed set of keys.  A kind's own field at the top
    /// level is the mistake the split exists to stop, so it is refused with
    /// the place it belongs in the message.
    #[test]
    fn a_key_outside_the_envelope_is_refused() {
        let err = validate_event(&obj(json!({ "kind": "msg_user", "content": "hi" })))
            .expect_err("a stray top-level key");
        assert!(err.reason().contains("\"content\""), "{err}");
        assert!(err.reason().contains("under data"), "{err}");

        // Every envelope key is accepted together, the kernel's stamps
        // included: those are reserved, not stray.
        validate_event(&obj(json!({
            "kind": "note",
            "beat": "b1",
            "meta": { "tag": "a" },
            "data": { "text": "hi" },
            "seq": 1,
            "epoch_ms": 0,
            "_schema_version": 1
        })))
        .expect("the whole envelope");
    }

    /// `meta` is shallow so a reader can use it without knowing the kind:
    /// scalars in, nesting out, and the refusal names `data`.
    #[test]
    fn meta_takes_scalars_and_refuses_nesting() {
        validate_event(&obj(json!({
            "kind": "note",
            "meta": { "label": "a", "attempt": 2, "retried": true }
        })))
        .expect("a shallow meta");
        validate_event(&obj(json!({ "kind": "note", "meta": {} }))).expect("an empty meta");

        for nested in [json!({ "deep": { "a": 1 } }), json!({ "deep": [1, 2] })] {
            let err = validate_event(&obj(json!({ "kind": "note", "meta": nested })))
                .expect_err("a nested meta value");
            assert!(err.reason().contains("meta is shallow"), "{err}");
            assert!(err.reason().contains("under data"), "{err}");
        }

        // A null is not one of the three scalars either: an absent key says
        // "nothing here" already.
        let err = validate_event(&obj(json!({ "kind": "note", "meta": { "x": null } })))
            .expect_err("a null meta value");
        assert!(err.reason().contains("meta is shallow"), "{err}");

        let err = validate_event(&obj(json!({ "kind": "note", "meta": "a" })))
            .expect_err("meta must be a table");
        assert!(err.reason().contains("meta must be a table"), "{err}");
    }

    /// `data` is an object at any depth, and it is optional on the way in.
    #[test]
    fn data_is_an_object_of_any_depth_and_may_be_left_out() {
        validate_event(&obj(json!({ "kind": "note" }))).expect("no data at all");
        validate_event(&obj(json!({ "kind": "note", "data": {} }))).expect("empty data");
        validate_event(&obj(json!({
            "kind": "llm_response",
            "data": { "content": [{ "type": "text", "text": "ok" }], "usage": { "in": 3 } }
        })))
        .expect("nested data");

        let err = validate_event(&obj(json!({ "kind": "note", "data": [1, 2] })))
            .expect_err("data must be a table");
        assert!(err.reason().contains("data must be a table"), "{err}");
    }

    /// An event written without `data` is stored with an empty one, so a
    /// reader never meets the key missing.
    #[test]
    fn a_stored_event_always_carries_meta_and_data() {
        let mut event = obj(json!({ "kind": "note" }));
        stamp(&mut event, 1, 0);
        assert_eq!(event[FIELD_DATA], json!({}));
        assert_eq!(event[FIELD_META], json!({}));

        // What the caller did write is left exactly as it was.
        let mut written = obj(json!({
            "kind": "note", "meta": { "label": "a" }, "data": { "text": "hi" }
        }));
        stamp(&mut written, 2, 0);
        assert_eq!(written[FIELD_META], json!({ "label": "a" }));
        assert_eq!(written[FIELD_DATA], json!({ "text": "hi" }));
    }

    /// A kind the kernel does not write passes with its `data` untouched —
    /// including the kinds a turn is made of, whose shapes the Lua kernel
    /// owns now.
    #[test]
    fn the_data_of_a_callers_kind_is_not_the_kernels_business() {
        for event in [
            json!({ "kind": "decision" }),
            json!({ "kind": "note", "data": { "any": { "nested": [1, 2] } } }),
            // The turn's kinds: no required field, whatever the shell writes.
            json!({ "kind": "msg_user", "data": { "content": "hi" } }),
            json!({ "kind": "msg_user", "data": {} }),
            json!({ "kind": "llm_request", "data": { "model": "m", "messages": [] } }),
            json!({ "kind": "llm_response", "data": { "anything": true } }),
            json!({ "kind": "llm_call_failed", "data": { "error": "boom" } }),
            json!({ "kind": "tool_call", "data": { "name": "sh" } }),
            json!({ "kind": "tool_result", "data": { "ok": false } }),
        ] {
            validate_event(&obj(event.clone())).unwrap_or_else(|e| panic!("{event}: {e}"));
        }
    }

    #[test]
    fn kernel_kinds_accept_their_documented_data() {
        for event in [
            json!({ "kind": "session_opened", "data": { "scope_id": "s1", "owner": "anon" } }),
            json!({ "kind": "session_closed", "data": { "reason": "closed" } }),
            json!({ "kind": "session_closed", "data": { "reason": "error", "detail": "boom" } }),
            json!({ "kind": "budget_granted", "data": { "amount": 100 } }),
            json!({
                "kind": "budget_granted",
                "data": { "amount": 100, "tag": "tokens", "desc": "one run" }
            }),
            json!({ "kind": "budget_reserved", "data": { "amount": 12, "tag": "tokens" } }),
            json!({ "kind": "budget_spent", "data": { "amount": 3 } }),
            json!({ "kind": "budget_refused", "data": { "amount": 40, "remaining": 7 } }),
        ] {
            validate_event(&obj(event.clone())).unwrap_or_else(|e| panic!("{event}: {e}"));
        }
    }

    #[test]
    fn kernel_kinds_reject_a_missing_data_field() {
        let err = validate_event(&obj(json!({ "kind": "session_closed", "data": {} })))
            .expect_err("reason is required");
        assert!(err.reason().contains("session_closed"), "{err}");
        assert!(err.reason().contains("reason"), "{err}");

        // …and an absent `data` reports the field, not the absence.
        let err =
            validate_event(&obj(json!({ "kind": "session_closed" }))).expect_err("no data at all");
        assert!(err.reason().contains("reason"), "{err}");

        let err = validate_event(&obj(json!({
            "kind": "session_opened", "data": { "scope_id": "s1" }
        })))
        .expect_err("owner is required");
        assert!(err.reason().contains("owner"), "{err}");

        let err = validate_event(&obj(json!({ "kind": "budget_reserved", "data": {} })))
            .expect_err("amount is required");
        assert!(err.reason().contains("amount"), "{err}");

        // A refusal without the balance it refused against is half a fact.
        let err = validate_event(&obj(json!({
            "kind": "budget_refused", "data": { "amount": 5 }
        })))
        .expect_err("remaining is required");
        assert!(err.reason().contains("remaining"), "{err}");
    }

    #[test]
    fn kernel_kinds_reject_a_mistyped_data_field() {
        let err = validate_event(&obj(json!({
            "kind": "session_closed", "data": { "reason": 7 }
        })))
        .expect_err("reason must be a string");
        assert!(err.reason().contains("must be a string"), "{err}");
        assert!(err.reason().contains("number"), "{err}");

        let err = validate_event(&obj(json!({
            "kind": "budget_spent", "data": { "amount": "lots" }
        })))
        .expect_err("amount must be a number");
        assert!(err.reason().contains("whole number"), "{err}");
    }

    /// The beat is the caller's word: never required, on any kind, and the
    /// kernel does not mint one to put in its place.
    #[test]
    fn no_kind_requires_a_beat() {
        for event in [
            json!({ "kind": "llm_response", "data": { "content": [] } }),
            json!({ "kind": "tool_call", "data": { "name": "sh" } }),
            json!({ "kind": "budget_spent", "data": { "amount": 1 } }),
            json!({ "kind": "note" }),
        ] {
            validate_event(&obj(event.clone()))
                .unwrap_or_else(|e| panic!("{event}: a beat must not be required: {e}"));
        }
    }

    /// A declared beat is a string on every kind — the kernel's own or a
    /// caller's — so a reader never has to ask whether this one is a number.
    #[test]
    fn a_declared_beat_must_be_a_string_on_any_kind() {
        for event in [
            json!({ "kind": "note", "beat": "b1" }),
            json!({ "kind": "llm_response", "beat": "b1", "data": { "content": [] } }),
            json!({ "kind": "budget_spent", "beat": "b1", "data": { "amount": 1 } }),
        ] {
            validate_event(&obj(event.clone()))
                .unwrap_or_else(|e| panic!("{event}: a string beat must be accepted: {e}"));
        }

        for event in [
            json!({ "kind": "note", "beat": 1 }),
            json!({ "kind": "llm_response", "beat": 1, "data": {} }),
            json!({ "kind": "tool_call", "beat": [], "data": {} }),
        ] {
            let err =
                validate_event(&obj(event.clone())).expect_err("a non-string beat must be refused");
            assert!(
                err.reason().contains("beat must be a string"),
                "{event}: {err}"
            );
        }
    }

    #[test]
    fn whole_floats_count_as_integers() {
        // Lua numbers arrive as floats when they are written `1.0`.  A
        // budget amount is a caller-supplied integer.
        validate_event(&obj(
            json!({ "kind": "budget_granted", "data": { "amount": 1.0 } }),
        ))
        .expect("1.0 is a whole number");

        let err = validate_event(&obj(
            json!({ "kind": "budget_granted", "data": { "amount": 1.5 } }),
        ))
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

    /// A kernel write is built with its fields under `data` by construction:
    /// there is no way to reach the constructor and leave them at the top.
    #[test]
    fn a_kernel_event_carries_its_fields_under_data() {
        let mut data = Map::new();
        data.insert(FIELD_REASON.to_string(), Value::from("closed"));
        let event = kernel_event(KIND_SESSION_CLOSED, data);

        assert_eq!(event[FIELD_KIND], json!("session_closed"));
        assert_eq!(event[FIELD_DATA], json!({ "reason": "closed" }));
        validate_event(&event).expect("what the kernel builds, the kernel accepts");
        assert_eq!(
            data_field(&event, FIELD_REASON).and_then(Value::as_str),
            Some("closed")
        );
        assert_eq!(data_field(&event, "nothing"), None);
    }

    /// The kernel checks the shape of what it writes itself, and of nothing
    /// else: the two lists coincide, kind for kind.
    #[test]
    fn the_shapes_the_kernel_checks_are_the_kinds_it_writes() {
        for kind in [
            "session_opened",
            "session_closed",
            "budget_granted",
            "budget_reserved",
            "budget_refused",
            "budget_spent",
        ] {
            assert!(is_kernel_only(kind), "{kind} must be kernel-only");
            assert!(is_reserved(kind), "{kind}'s data shape is the kernel's");
        }
        for kind in [
            // The turn's kinds are the Lua kernel's now, shape and all.
            "msg_user",
            "llm_request",
            "llm_response",
            "llm_call_failed",
            "tool_call",
            "tool_result",
            // …as is every other kind a shell invents.
            "note",
            "decision",
            "carry",
            "budget",
            "session",
            "",
        ] {
            assert!(!is_kernel_only(kind), "{kind} must not be kernel-only");
            assert!(!is_reserved(kind), "{kind}'s data shape is its writer's");
        }
    }

    /// Validation is about shape only.  The kernel-only kinds pass here:
    /// their shape is fine, it is the authorship that is not, and that is
    /// stopped by [`is_kernel_only`] on the append path rather than here.
    #[test]
    fn shape_validation_says_nothing_about_who_may_write_a_kind() {
        for event in [
            json!({ "kind": "session_opened", "data": { "scope_id": "s", "owner": "anon" } }),
            json!({ "kind": "session_closed", "data": { "reason": "carried over" } }),
            json!({ "kind": "budget_granted", "data": { "amount": 1_000_000 } }),
        ] {
            validate_event(&obj(event.clone())).unwrap_or_else(|e| panic!("{event}: {e}"));
        }
    }
}
