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
//!
//! # Appends land; decisions are taken inside the write
//!
//! [`EventStore::append`] records a fact, and the store — not the caller —
//! decides where it lands.  It is *serialized per stream by the backend*
//! (SQLite: an `IMMEDIATE` transaction with a bounded busy retry; the
//! in-memory store: a single owner in a single process), so two handles on
//! one stream both write and the log interleaves in arrival order.  An
//! ordinary append is never refused for an out-of-date view of the head:
//! that would be asking a fact to prove it knew the future.
//!
//! A *command* with an invariant — "reserve n only if the balance covers
//! it" — is the other case, and it is expressed by
//! [`EventStore::append_if`]: the backend reads the stream, calls the
//! caller's `decide` and appends what it returns, all inside the same
//! serialized write.  The check therefore runs against the stream as it is
//! at that instant, not against a head someone cached earlier.
//!
//! # Stored shape change ⇒ upcaster, always — from the first release on
//!
//! Stored bytes are never rewritten.  Every change to the shape of a stored
//! event ships in the same round as (a) a bump of
//! [`CURRENT_SCHEMA_VERSION`], which every new event is stamped with, and
//! (b) an [`Upcaster`] for the `n → n+1` step, registered in
//! [`kernel_upcasters`] and applied at read time by
//! [`UpcastingEventStore`].  A round that renames a kind or a field without
//! an upcaster is incomplete: an old log would be silently misread, which is
//! the one failure an append-only store exists to prevent.
//!
//! That obligation starts at the first release.  Until then the stored shape
//! is still being settled, there is no log anyone has to keep, and a rename
//! is a rename — so [`kernel_upcasters`] returns an empty chain and
//! [`CURRENT_SCHEMA_VERSION`] stays at `1`.  The seam is built and tested all
//! the same, so the first step that is owed has one site to be registered at.

use std::sync::Arc;

use serde_json::{Map, Value};

use super::event::{seq_of, FIELD_EPOCH_MS};
use super::{History, KnlResult};

/// Reserved envelope key: the schema version an event was written under.
///
/// Read-time upcasting keys on it, so a stored event carries the version it
/// was written with and the stored bytes are never rewritten.  The `_`
/// prefix keeps it out of the caller's payload namespace, like the other
/// kernel-owned envelope fields.
pub const SCHEMA_VERSION_FIELD: &str = "_schema_version";

/// The schema version new events are stamped with.
///
/// `1`: the stored shape has never been released, so nothing has been
/// written under an older one and there is no step to take.  A shape change
/// *after* the first release bumps this and registers the matching
/// [`Upcaster`] — see the module docs.
pub const CURRENT_SCHEMA_VERSION: u64 = 1;

/// The upcaster chain every session reads through, newest step last.
///
/// [`super::Session::open_on`] and [`super::Session::resume`] wrap their
/// backend in an [`UpcastingEventStore`] carrying this chain, so every read a
/// session makes — the restore fold, the view folds, `events` — sees the
/// current shape while the stored bytes stay exactly as they were written.
///
/// Empty until the first release: there is no released shape to read yet, so
/// there is no step owed.  This is the one site a step is registered at, and
/// the seam around it is exercised by the tests below with a chain of their
/// own.
pub fn kernel_upcasters() -> Vec<Arc<dyn Upcaster>> {
    Vec::new()
}

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

/// What a command decides, given the stream it is decided against.
///
/// Handed the stream's events in `seq` order and returning the event to
/// record — or `None` to record nothing.  [`EventStore::append_if`] runs it
/// inside the backend's serialization, which is what makes the decision and
/// the write one step.
pub type Decision<'a> = dyn FnMut(&[Value]) -> Option<Map<String, Value>> + 'a;

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
    ///
    /// **Serialized per stream by the backend.**  The store assigns the
    /// `seq` and the ordering, and the append lands: two handles writing to
    /// one stream interleave in arrival order rather than one of them being
    /// refused for holding an out-of-date head.  SQLite takes an `IMMEDIATE`
    /// transaction (with a bounded busy retry) around the head read and the
    /// insert; the in-memory store is owned by one session in one process.
    fn append(&mut self, event: Map<String, Value>) -> KnlResult<Committed>;

    /// Decide *inside* the store's serialization: read the stream, ask
    /// `decide` what to write, and append its answer in the same write.
    ///
    /// The form a command with an invariant takes — "reserve `n` only if the
    /// balance covers it".  `decide` is handed the stream's events (in `seq`
    /// order) as they are under the backend's lock, and returns the event to
    /// record, or `None` to record nothing (`Ok(None)`, with the stream
    /// untouched).  Because the read and the write share the transaction, the
    /// decision cannot be raced by a concurrent writer — which a
    /// compare-and-swap against a cached head could only detect afterwards.
    ///
    /// `decide` may be called more than once when a contended backend retries
    /// its transaction, so it must be a pure function of the events it is
    /// given.
    ///
    /// The whole stream is read to decide, which is fine while streams are
    /// small; a future optimisation reads only the range a decision needs
    /// (a snapshot plus the events after it).
    fn append_if(&mut self, decide: &mut Decision<'_>) -> KnlResult<Option<Committed>>;

    /// Events with `seq >= from_seq`, at most `limit`, cloned in `seq`
    /// order (a paged range read).
    ///
    /// Fallible: a durable backend can hit a transient busy read or a row it
    /// cannot decode, and those must surface rather than be silently dropped
    /// (a dropped row would let [`super::Session::resume`] re-fold a truncated
    /// log into the wrong state). The in-memory backend never errors.
    fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Value>>;

    /// The current head: the highest `seq`, or `None` for an empty stream.
    ///
    /// Fallible for the same reason as [`EventStore::read`]: a durable
    /// backend can hit a transient busy read, and swallowing it would make
    /// a populated stream look empty — the caller deciding open-vs-resume
    /// (or a CAS comparing heads) must see the fault, not a wrong answer.
    fn head(&self) -> KnlResult<Option<u64>>;

    /// Number of recorded events.  Fallible like [`EventStore::head`].
    fn len(&self) -> KnlResult<usize>;

    /// Whether nothing has been recorded yet.
    fn is_empty(&self) -> KnlResult<bool> {
        Ok(self.len()? == 0)
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

    fn append_if(&mut self, decide: &mut Decision<'_>) -> KnlResult<Option<Committed>> {
        // One process, one owner: the read and the append below cannot be
        // interleaved with another writer's, which is all the serialization
        // this backend needs.
        let events = self.read(0, usize::MAX)?;
        match decide(&events) {
            Some(event) => self.append(event).map(Some),
            None => Ok(None),
        }
    }

    fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Value>> {
        // The in-memory history is infallible; the `Ok` is the SPI's shape,
        // not a failure the mem backend can actually produce.
        let mut events = self.history.since(from_seq);
        events.truncate(limit);
        Ok(events)
    }

    fn head(&self) -> KnlResult<Option<u64>> {
        // `seq` is monotonic and gap-free, so the last event carries the
        // highest one.  Infallible in memory; the `Ok` is the SPI's shape.
        Ok(self.history.events().last().map(seq_of))
    }

    fn len(&self) -> KnlResult<usize> {
        Ok(self.history.len())
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

    fn append_if(&mut self, decide: &mut Decision<'_>) -> KnlResult<Option<Committed>> {
        // The decision is a read, so it is upcasted like every other read: the
        // backend hands over the stored events, the chain projects them, and
        // `decide` sees the current shape — a v1 log decides the same way a
        // v2 one does.
        let chain = self.chain.clone();
        let mut upcasted = |events: &[Value]| decide(&apply_upcasters(&chain, events.to_vec()));
        self.inner.append_if(&mut upcasted)
    }

    fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Value>> {
        // The single read-time application point: read from the backend, then
        // fold the chain over the events before handing them back.
        let events = self.inner.read(from_seq, limit)?;
        Ok(apply_upcasters(&self.chain, events))
    }

    fn head(&self) -> KnlResult<Option<u64>> {
        self.inner.head()
    }

    fn len(&self) -> KnlResult<usize> {
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
        assert!(store.is_empty().expect("is_empty"));
        assert_eq!(store.len().expect("len"), 0);

        let a = store.append(ev(1)).expect("append e1");
        let b = store.append(ev(2)).expect("append e2");
        let c = store.append(ev(3)).expect("append e3");

        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
        assert!(a.epoch_ms >= 1 || a.epoch_ms == 0, "epoch is stamped");
        assert_eq!(store.len().expect("len"), 3);
        assert!(!store.is_empty().expect("is_empty"));

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
        assert_eq!(store.len().expect("len"), 0);
        assert_eq!(store.append(ev(1)).expect("append").seq, 1);
    }

    /// `append_if` decides on the stream the backend hands it and writes in
    /// the same step: a `Some` lands, a `None` writes nothing at all.
    #[test]
    fn append_if_decides_on_the_stream_and_writes_only_a_some() {
        let mut store = MemEventStore::new();
        store.append(ev(1)).expect("seed");

        // The decision sees the stream as it is, in seq order.
        let mut seen = 0;
        let committed = store
            .append_if(&mut |events| {
                seen = events.len();
                Some(ev(2))
            })
            .expect("append_if");
        assert_eq!(seen, 1, "decide was handed the whole stream");
        assert_eq!(committed.map(|c| c.seq), Some(2));

        // `None` is a decision too: nothing is written and no seq is burnt.
        let nothing = store.append_if(&mut |_| None).expect("append_if");
        assert_eq!(nothing, None);
        assert_eq!(store.len().expect("len"), 2, "a None writes nothing");
        assert_eq!(store.append(ev(3)).expect("append").seq, 3);
    }

    /// The event a decision returns is validated like any other: a malformed
    /// one is refused and the stream is untouched.
    #[test]
    fn append_if_validates_the_event_the_decision_returns() {
        let mut store = MemEventStore::new();
        store
            .append_if(&mut |_| Some(obj(json!({ "text": "no kind" }))))
            .expect_err("kind is required");
        assert_eq!(store.len().expect("len"), 0);
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
        assert_eq!(store.head().expect("head"), None);

        store.append(ev(1)).expect("append");
        assert_eq!(store.head().expect("head"), Some(1));
        store.append(ev(2)).expect("append");
        assert_eq!(store.head().expect("head"), Some(2));

        // A rejected append does not move the head.
        store
            .append(obj(json!({ "text": "no kind" })))
            .expect_err("kind is required");
        assert_eq!(store.head().expect("head"), Some(2));
    }

    #[test]
    fn default_matches_new_and_starts_seq_at_one() {
        let mut store = MemEventStore::default();
        assert!(store.is_empty().expect("is_empty"));
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
        let out = apply_upcasters(&chain, vec![json!({ "kind": "x" }), json!({ "kind": "y" })]);
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
        assert_eq!(store.head().expect("head"), Some(1));
        assert_eq!(store.len().expect("len"), 1);
        assert!(!store.is_empty().expect("is_empty"));

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
        assert_eq!(store.head().expect("head"), Some(2));
        assert_eq!(store.len().expect("len"), 2);
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
        assert!(store.is_empty().expect("is_empty"));

        store.append(ev(1)).expect("append");
        let read = store.read(0, usize::MAX).expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(
            kind_of(&read[0]),
            "e1",
            "the event passes through unchanged"
        );
        assert!(
            read[0].get("trace").is_none(),
            "an empty chain adds nothing: {}",
            read[0]
        );
        assert_eq!(store.head().expect("head"), Some(1));
        assert_eq!(store.len().expect("len"), 1);
    }

    /// A test-local `1 → 2` step, standing in for a real one: it renames a
    /// kind and marks the projection with the version it produced.  The
    /// kernel chain is empty until the first release, so the mechanism is
    /// exercised with a chain the tests own.
    struct RenameOldKind;

    impl Upcaster for RenameOldKind {
        fn upcast(&self, mut event: Value) -> Value {
            // Already at the newer shape — or not an object at all — so
            // there is nothing to do.  An upcaster is infallible: an event
            // it does not recognise comes back exactly as it went in.
            let version = event
                .get(SCHEMA_VERSION_FIELD)
                .and_then(Value::as_u64)
                .unwrap_or(1);
            if version >= 2 {
                return event;
            }
            let Some(map) = event.as_object_mut() else {
                return event;
            };
            if map.get(FIELD_KIND).and_then(Value::as_str) == Some("old_kind") {
                map.insert(FIELD_KIND.to_string(), Value::from("new_kind"));
            }
            map.insert(SCHEMA_VERSION_FIELD.to_string(), Value::from(2_u64));
            event
        }
    }

    /// The chain a session reads through is empty until the first release,
    /// so a stored event reads back exactly as it was written.
    #[test]
    fn the_kernel_chain_is_empty_and_the_current_version_is_one() {
        assert_eq!(CURRENT_SCHEMA_VERSION, 1);
        assert!(
            kernel_upcasters().is_empty(),
            "no shape has been released, so no step is owed"
        );

        let stored = json!({ "kind": "note", "seq": 1, SCHEMA_VERSION_FIELD: 1 });
        assert_eq!(
            apply_upcasters(&kernel_upcasters(), vec![stored.clone()]),
            vec![stored],
            "an empty chain reads the log back verbatim"
        );
    }

    /// A stored event carries the version it was written under.
    #[test]
    fn new_events_are_stamped_with_the_current_version() {
        let mut store = MemEventStore::new();
        store.append(ev(1)).expect("append");
        let stored = store.read(0, usize::MAX).expect("read");
        assert_eq!(
            stored[0].get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION),
            "{}",
            stored[0]
        );
    }

    /// An upcaster is total: an event already at the version it produces, and
    /// a value it does not recognise at all, both come back unchanged rather
    /// than failing or being guessed at.
    #[test]
    fn an_upcaster_leaves_a_current_or_unrecognised_event_unchanged() {
        let chain: Vec<Arc<dyn Upcaster>> = vec![Arc::new(RenameOldKind)];

        // Already at the newer shape: untouched, kind included.
        let current = json!({ "kind": "old_kind", "seq": 1, SCHEMA_VERSION_FIELD: 2 });
        assert_eq!(
            apply_upcasters(&chain, vec![current.clone()]),
            vec![current],
            "an event at the version the step produces is not stepped again"
        );

        // Not an object, and not a kind the step knows: neither panics, and
        // neither is invented into something else.
        let out = apply_upcasters(&chain, vec![json!(42), json!({ "kind": "note", "seq": 1 })]);
        assert_eq!(out[0], json!(42), "a non-object passes straight through");
        assert_eq!(kind_of(&out[1]), "note", "an unknown kind keeps its name");
    }
}
