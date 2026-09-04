//! The event-store SPI and its in-memory backend.
//!
//! An [`EventStore`] is one session's append-only log behind a trait, so
//! the durable backend a later round adds (SQLite) can take the same
//! calls the in-memory one does.  The SPI is scoped to a *single* stream:
//! the session is the stream, so there is no `stream` parameter here —
//! multiple streams are a durable-backend concern.
//!
//! # Append-only is the shape, not a runtime check
//!
//! The trait has no `update`, `delete` or `overwrite`.  Immutability is
//! guaranteed by what the trait *cannot* express, the same way [`History`]
//! guarantees it by having no mutation API.
//!
//! # Store-assigned coordinates, returned inline
//!
//! A write returns a [`Committed`]: the `seq` and `epoch_ms` the store
//! assigned.  A caller never has to read back to learn where its event
//! landed, and never supplies those fields — they are the store's to give.

use std::sync::Arc;

use serde_json::{Map, Value};

use super::event::{seq_of, FIELD_EPOCH_MS};
use super::{History, KnlError, KnlResult};

/// Reserved envelope key: the schema version an event was written under.
///
/// Read-time upcasting keys on it, so a stored event carries the version it
/// was written with and the stored bytes are never rewritten.  The `_`
/// prefix keeps it out of the caller's payload namespace, like the other
/// kernel-owned envelope fields.
pub const SCHEMA_VERSION_FIELD: &str = "_schema_version";

/// The schema version new events are stamped with.
///
/// Starts at `1`; a future shape change bumps it and registers an
/// [`Upcaster`] for the `n → n+1` step.
pub const CURRENT_SCHEMA_VERSION: u64 = 1;

/// Stamp [`CURRENT_SCHEMA_VERSION`] onto an event, overwriting any
/// caller-supplied value (the version is the store's to assign, like `seq`).
///
/// Every backend calls this on append so future events carry the version an
/// [`Upcaster`] dispatches on; `event.rs` owns `seq` / `epoch_ms` stamping
/// and is left untouched, so the version is stamped here in the store layer.
pub(super) fn stamp_schema_version(event: &mut Map<String, Value>) {
    event.insert(
        SCHEMA_VERSION_FIELD.to_string(),
        Value::from(CURRENT_SCHEMA_VERSION),
    );
}

/// A read-time transform from one event shape to the next.
///
/// Pure and infallible: an event whose version the upcaster does not
/// recognise passes through unchanged.  Upcasting happens on *read* — the
/// stored bytes are never rewritten — so an old log stays readable by new
/// code without a migration pass.
pub trait Upcaster: Send + Sync {
    /// Transform one event, or return it unchanged when it does not apply.
    fn upcast(&self, event: Value) -> Value;
}

/// Apply an upcaster `chain` to every event, in registration order.
///
/// The read-time application point: each event is folded through the chain
/// front to back, so a two-step migration (`1 → 2`, then `2 → 3`) composes.
/// An empty chain is the identity — the shape is fixed now even though no
/// upcaster is registered yet.
pub fn apply_upcasters(chain: &[Arc<dyn Upcaster>], events: Vec<Value>) -> Vec<Value> {
    events
        .into_iter()
        .map(|event| chain.iter().fold(event, |event, up| up.upcast(event)))
        .collect()
}

/// The coordinates a store assigns to an appended event.
///
/// Returned inline from every write so no follow-up read is needed to
/// learn the `seq` / `epoch_ms` the store stamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    /// The store-assigned sequence number (gap-free, monotonic, from 1).
    pub seq: u64,
    /// The wall-clock append time the store stamped, in milliseconds.
    pub epoch_ms: u64,
}

/// One session's append-only event log.
///
/// Scoped to a single stream: the session *is* the stream.  The trait is
/// deliberately append-only — there is no mutation method, so a backend
/// cannot offer one.
pub trait EventStore {
    /// Validate, stamp and append an event, returning its coordinates.
    ///
    /// A rejected event leaves no trace and consumes no sequence number.
    fn append(&mut self, event: Map<String, Value>) -> KnlResult<Committed>;

    /// Append iff the current head is `expected_head` (compare-and-swap).
    ///
    /// `expected_head == 0` means "expect the stream to be empty".  On a
    /// mismatch the append does not happen and a head-conflict error is
    /// returned carrying the expected and the actual head.
    fn append_if_head(
        &mut self,
        event: Map<String, Value>,
        expected_head: u64,
    ) -> KnlResult<Committed>;

    /// Events with `seq >= from_seq`, at most `limit`, cloned in `seq`
    /// order (a paged range read).
    ///
    /// Fallible: a durable backend can hit a transient busy read or a row it
    /// cannot decode, and those must surface rather than be silently dropped
    /// (a dropped row would let [`super::Session::resume`] re-fold a truncated
    /// log into the wrong state). The in-memory backend never errors.
    fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Value>>;

    /// The current head: the highest `seq`, or `None` for an empty stream.
    fn head(&self) -> Option<u64>;

    /// Number of recorded events.
    fn len(&self) -> usize;

    /// Whether nothing has been recorded yet.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// In-memory [`EventStore`]: the existing [`History`] behind the SPI.
///
/// Storage is a [`History`], so the in-memory behaviour — seq assignment,
/// validation on append, `since`-style reads — is exactly today's, and the
/// inner history is available to the projection folds that still fold over
/// a `&History`.
#[derive(Debug, Clone)]
pub struct MemEventStore {
    /// The append-only history that holds the events.
    history: History,
}

impl MemEventStore {
    /// A fresh, empty in-memory store.
    pub fn new() -> Self {
        Self {
            history: History::new(),
        }
    }

    /// Borrow the inner history for the projection folds, which read a
    /// `&History` directly.
    pub fn history(&self) -> &History {
        &self.history
    }
}

impl Default for MemEventStore {
    /// The same fresh store as [`MemEventStore::new`] — note this is *not*
    /// `History::default()`, whose `next_seq` would start at `0`.
    fn default() -> Self {
        Self::new()
    }
}

impl EventStore for MemEventStore {
    fn append(&mut self, mut event: Map<String, Value>) -> KnlResult<Committed> {
        // Stamp the schema version before the history validates and stamps
        // `seq` / `epoch_ms`, so a stored event carries all three; a rejected
        // event is dropped here and leaves no trace, as before.
        stamp_schema_version(&mut event);
        let seq = self.history.append(event)?;
        // The append stamped `epoch_ms` on the event it just pushed; read
        // it back off the tail (an O(1) index, not a round-trip) so the
        // exact stored value is returned inline.
        let epoch_ms = self
            .history
            .events()
            .last()
            .and_then(|event| event.get(FIELD_EPOCH_MS))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Ok(Committed { seq, epoch_ms })
    }

    fn append_if_head(
        &mut self,
        event: Map<String, Value>,
        expected_head: u64,
    ) -> KnlResult<Committed> {
        let actual = self.head();
        let matches = match expected_head {
            0 => actual.is_none(),
            expected => actual == Some(expected),
        };
        if !matches {
            return Err(KnlError::new(format!(
                "head conflict: expected {expected_head}, actual {actual:?}"
            )));
        }
        self.append(event)
    }

    fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Value>> {
        // The in-memory history is infallible; the `Ok` is the SPI's shape,
        // not a failure the mem backend can actually produce.
        let mut events = self.history.since(from_seq);
        events.truncate(limit);
        Ok(events)
    }

    fn head(&self) -> Option<u64> {
        // `seq` is monotonic and gap-free, so the last event carries the
        // highest one.
        self.history.events().last().map(seq_of)
    }

    fn len(&self) -> usize {
        self.history.len()
    }
}

/// An [`EventStore`] decorator that upcasts on read.
///
/// Wraps any backend and a `chain` of [`Upcaster`]s.  Reads fold the chain
/// over the events before returning them (read-time projection); every write
/// passes straight through, so the stored bytes are never rewritten — the same
/// old-log-stays-readable discipline [`Upcaster`] describes, established once
/// here as the single seam a future upcaster registers into.
///
/// An empty chain is a functional no-op, which is the state today: v1 has no
/// upcaster, so the decorator changes nothing, but a later shape change
/// registers its `n → n+1` step here and every read path picks it up.
pub struct UpcastingEventStore {
    /// The wrapped backend that actually holds the events.
    inner: Box<dyn EventStore>,
    /// The read-time upcaster chain, applied front to back on every read.
    chain: Vec<Arc<dyn Upcaster>>,
}

impl UpcastingEventStore {
    /// Wrap `inner` so its reads are upcasted through `chain`.
    ///
    /// An empty `chain` makes this an identity decorator over `inner`.
    pub fn new(inner: Box<dyn EventStore>, chain: Vec<Arc<dyn Upcaster>>) -> Self {
        Self { inner, chain }
    }
}

impl EventStore for UpcastingEventStore {
    fn append(&mut self, event: Map<String, Value>) -> KnlResult<Committed> {
        // Write path untouched: upcasting is read-time only.
        self.inner.append(event)
    }

    fn append_if_head(
        &mut self,
        event: Map<String, Value>,
        expected_head: u64,
    ) -> KnlResult<Committed> {
        self.inner.append_if_head(event, expected_head)
    }

    fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Value>> {
        // The single read-time application point: read from the backend, then
        // fold the chain over the events before handing them back.
        let events = self.inner.read(from_seq, limit)?;
        Ok(apply_upcasters(&self.chain, events))
    }

    fn head(&self) -> Option<u64> {
        self.inner.head()
    }

    fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::{kind_of, FIELD_KIND};
    use serde_json::json;

    /// Object map for an event literal.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
    }

    /// An open-kind event named `e{i}`.
    fn ev(i: usize) -> Map<String, Value> {
        obj(json!({ "kind": format!("e{i}") }))
    }

    #[test]
    fn append_assigns_gap_free_monotonic_seq_from_one() {
        let mut store = MemEventStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        let a = store.append(ev(1)).expect("append e1");
        let b = store.append(ev(2)).expect("append e2");
        let c = store.append(ev(3)).expect("append e3");

        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
        assert!(a.epoch_ms >= 1 || a.epoch_ms == 0, "epoch is stamped");
        assert_eq!(store.len(), 3);
        assert!(!store.is_empty());

        // The stamped epoch is what is stored.
        let stored = store.read(0, usize::MAX).expect("read");
        let stored_epoch = stored[0]
            .get(FIELD_EPOCH_MS)
            .and_then(Value::as_u64)
            .expect("epoch is on the stored event");
        assert_eq!(stored_epoch, a.epoch_ms);
    }

    #[test]
    fn a_rejected_append_records_nothing_and_burns_no_seq() {
        let mut store = MemEventStore::new();
        store
            .append(obj(json!({ "text": "no kind" })))
            .expect_err("kind is required");
        assert_eq!(store.len(), 0);
        assert_eq!(store.append(ev(1)).expect("append").seq, 1);
    }

    #[test]
    fn append_if_head_is_a_compare_and_swap() {
        let mut store = MemEventStore::new();

        // `expected_head == 0` means "expect empty" and succeeds on a fresh
        // store, assigning seq 1.
        let first = store.append_if_head(ev(1), 0).expect("empty CAS");
        assert_eq!(first.seq, 1);

        // The same "expect empty" now conflicts: the head is 1, not empty.
        let err = store
            .append_if_head(ev(2), 0)
            .expect_err("no longer empty");
        assert!(err.reason().contains("head conflict"), "{err}");
        assert!(err.reason().contains("expected 0"), "{err}");
        assert!(err.reason().contains("actual Some(1)"), "{err}");
        assert_eq!(store.len(), 1, "the conflicting append did not happen");

        // Matching the real head succeeds and advances it.
        let second = store.append_if_head(ev(2), 1).expect("head matches");
        assert_eq!(second.seq, 2);

        // A wrong (non-zero) expectation conflicts and reports both sides.
        let err = store
            .append_if_head(ev(3), 5)
            .expect_err("stale head");
        assert!(err.reason().contains("expected 5"), "{err}");
        assert!(err.reason().contains("actual Some(2)"), "{err}");
        assert_eq!(store.len(), 2, "no append on conflict");
    }

    #[test]
    fn read_pages_by_from_seq_and_limit() {
        let mut store = MemEventStore::new();
        for i in 1..=5 {
            store.append(ev(i)).expect("append");
        }

        // from_seq filters, limit caps.
        assert_eq!(store.read(0, usize::MAX).expect("read").len(), 5);
        assert_eq!(store.read(1, usize::MAX).expect("read").len(), 5);
        assert_eq!(store.read(3, usize::MAX).expect("read").len(), 3);
        assert_eq!(store.read(6, usize::MAX).expect("read").len(), 0);

        let page = store.read(2, 2).expect("read");
        assert_eq!(page.len(), 2);
        assert_eq!(kind_of(&page[0]), "e2");
        assert_eq!(kind_of(&page[1]), "e3");

        // A zero limit returns nothing even when events match.
        assert!(store.read(0, 0).expect("read").is_empty());
    }

    #[test]
    fn head_is_none_when_empty_then_tracks_the_max() {
        let mut store = MemEventStore::new();
        assert_eq!(store.head(), None);

        store.append(ev(1)).expect("append");
        assert_eq!(store.head(), Some(1));
        store.append(ev(2)).expect("append");
        assert_eq!(store.head(), Some(2));

        // A rejected append does not move the head.
        store
            .append(obj(json!({ "text": "no kind" })))
            .expect_err("kind is required");
        assert_eq!(store.head(), Some(2));
    }

    #[test]
    fn default_matches_new_and_starts_seq_at_one() {
        let mut store = MemEventStore::default();
        assert!(store.is_empty());
        // Guards against `History::default()` (next_seq == 0) leaking in.
        assert_eq!(store.append(ev(1)).expect("append").seq, 1);
    }

    #[test]
    fn reads_are_copies_so_the_store_cannot_be_reached_through_them() {
        let mut store = MemEventStore::new();
        store.append(ev(1)).expect("append");
        let mut copy = store.read(0, usize::MAX).expect("read");
        copy[0][FIELD_KIND] = Value::String("TAMPERED".into());
        assert_eq!(kind_of(&store.read(0, usize::MAX).expect("read")[0]), "e1");
    }

    #[test]
    fn append_stamps_the_current_schema_version() {
        let mut store = MemEventStore::new();
        store.append(ev(1)).expect("append");
        let stored = store.read(0, usize::MAX).expect("read");
        assert_eq!(
            stored[0].get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION),
            "a stored event carries the version it was written under: {}",
            stored[0]
        );
    }

    #[test]
    fn an_empty_upcaster_chain_is_the_identity() {
        let events = vec![
            json!({ "kind": "a", "seq": 1 }),
            json!({ "kind": "b", "seq": 2 }),
        ];
        assert_eq!(apply_upcasters(&[], events.clone()), events);
    }

    #[test]
    fn upcasters_compose_per_event_in_registration_order() {
        use std::sync::Arc;

        // Each upcaster pushes its label onto a `trace` array, so the final
        // order proves the chain walked front to back, once per event.
        struct Tag(&'static str);
        impl Upcaster for Tag {
            fn upcast(&self, mut event: Value) -> Value {
                let map = event.as_object_mut().expect("event is an object");
                let trace = map
                    .entry("trace")
                    .or_insert_with(|| Value::Array(Vec::new()));
                trace
                    .as_array_mut()
                    .expect("trace is an array")
                    .push(Value::from(self.0));
                event
            }
        }

        let chain: Vec<Arc<dyn Upcaster>> = vec![Arc::new(Tag("first")), Arc::new(Tag("second"))];
        let out = apply_upcasters(
            &chain,
            vec![json!({ "kind": "x" }), json!({ "kind": "y" })],
        );
        assert_eq!(out[0]["trace"], json!(["first", "second"]));
        assert_eq!(out[1]["trace"], json!(["first", "second"]));
    }

    /// (Fix 4) `UpcastingEventStore` projects the chain on read while leaving
    /// the write path untouched: an appended event is stored raw, and the
    /// marker only appears in the read projection — it never accumulates, so
    /// the stored bytes carry no upcaster field.
    #[test]
    fn upcasting_store_projects_on_read_and_leaves_writes_untouched() {
        use std::sync::Arc;

        // Pushes a marker onto a `trace` array, so a value upcasted twice would
        // show two entries — this distinguishes a read-time projection from a
        // stored rewrite.
        struct Mark;
        impl Upcaster for Mark {
            fn upcast(&self, mut event: Value) -> Value {
                let map = event.as_object_mut().expect("event is an object");
                map.entry("trace")
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .expect("trace is an array")
                    .push(Value::from("mark"));
                event
            }
        }

        let chain: Vec<Arc<dyn Upcaster>> = vec![Arc::new(Mark)];
        let mut store = UpcastingEventStore::new(Box::new(MemEventStore::new()), chain);

        // Write path passes through: coordinates and counters are the backend's.
        let a = store.append(ev(1)).expect("append e1");
        assert_eq!(a.seq, 1);
        assert_eq!(store.head(), Some(1));
        assert_eq!(store.len(), 1);
        assert!(!store.is_empty());

        // read projects the marker on.
        let first = store.read(0, usize::MAX).expect("read");
        assert_eq!(first[0]["trace"], json!(["mark"]), "read applies the chain");

        // A second read is consistent — the marker does not accumulate, proving
        // the append stored no marker (the write path is not upcasted).
        let second = store.read(0, usize::MAX).expect("read again");
        assert_eq!(
            second[0]["trace"],
            json!(["mark"]),
            "stored bytes carry no marker; read adds exactly one"
        );

        // A freshly appended event reads back consistently under the same chain,
        // and head / len stay in step with the backend.
        let b = store.append(ev(2)).expect("append e2");
        assert_eq!(b.seq, 2);
        assert_eq!(store.head(), Some(2));
        assert_eq!(store.len(), 2);
        let both = store.read(0, usize::MAX).expect("read both");
        assert_eq!(both.len(), 2);
        assert_eq!(kind_of(&both[1]), "e2");
        assert_eq!(both[1]["trace"], json!(["mark"]));
    }

    /// (Fix 4) An empty chain makes the decorator an identity over its backend:
    /// reads return the events unchanged, with no upcaster-added field, and the
    /// coordinates track the backend exactly.
    #[test]
    fn upcasting_store_with_an_empty_chain_returns_events_unchanged() {
        let mut store = UpcastingEventStore::new(Box::new(MemEventStore::new()), Vec::new());
        assert!(store.is_empty());

        store.append(ev(1)).expect("append");
        let read = store.read(0, usize::MAX).expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(kind_of(&read[0]), "e1", "the event passes through unchanged");
        assert!(
            read[0].get("trace").is_none(),
            "an empty chain adds nothing: {}",
            read[0]
        );
        assert_eq!(store.head(), Some(1));
        assert_eq!(store.len(), 1);
    }
}
