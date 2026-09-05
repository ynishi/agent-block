//! The event-store SPI and its in-memory backend.
//!
//! An [`EventStore`] is one session's append-only log behind a trait, so
//! the durable backend a later round adds (SQLite) can take the same
//! calls the in-memory one does.  The SPI is scoped to a *single* stream:
//! the session is the stream, so there is no `stream` parameter here —
//! multiple streams are a durable-backend concern.
//!
//! Two calls are outside that scoping, and both are outside it because one
//! transaction has to cover two streams — which is the one thing a caller
//! cannot build on top of the SPI for itself.
//! [`EventStore::append_if_many`] decides against this stream and writes to
//! this one and one other (a parent's ledger entry beside the child's opening
//! and grant), and [`EventStore::append_with_open_children`] scans the
//! database for the streams that name this one as their parent and appends an
//! event built from what it found.  Both take the kinds and the field names
//! they work with as arguments: the vocabulary stays the caller's, and the
//! backend only knows how to walk.  Both are answered by the durable backend
//! and by nothing else — a store that keeps one stream has no second one to
//! write ([`KnlError::Unsupported`]) and no children to find (an empty list).
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
//! Facts that belong together are written with [`EventStore::append_many`],
//! which the durable backend takes in one transaction: a session's opening
//! and the grant it opened with either both land or neither does, so no
//! reader ever meets a stream that opened without the quota it was opened
//! under.
//!
//! A *command* with an invariant — "reserve n only if the balance covers
//! it" — is the other case, and it is expressed by
//! [`EventStore::append_if`]: the backend reads the stream, calls the
//! caller's `decide` and appends what it returns, all inside the same
//! serialized write.  The check therefore runs against the stream as it is
//! at that instant, not against a head someone cached earlier.
//!
//! # Reads name the kinds they need
//!
//! [`EventStore::read_kinds`] takes the kinds a caller is folding over
//! (`None` for all of them), and [`EventStore::append_if`] filters the
//! decision's input the same way.  The kernel does not interpret the kinds
//! here — the caller names them, because the caller is the one that knows
//! what its fold reads: the balance folds the four `budget_*` kinds, a
//! resume asks whether a `session_closed` is there.  A durable backend
//! answers those off an index rather than walking the stream.
//!
//! # Stored shape change ⇒ upcaster, always — from the first release on
//!
//! Stored bytes are never rewritten.  Every change to the shape of a stored
//! event ships in the same round as (a) a bump of
//! [`CURRENT_SCHEMA_VERSION`], which every new event is stamped with, and
//! (b) an [`Upcaster`] for the `n → n+1` step, registered in
//! [`kernel_upcasters`] and applied at read time by [`CurrentStore`].  A
//! round that renames a kind or a field without an upcaster is incomplete:
//! an old log would be silently misread, which is the one failure an
//! append-only store exists to prevent.
//!
//! That obligation starts at the first release.  Until then the stored shape
//! is still being settled, there is no log anyone has to keep, and a rename
//! is a rename — so [`kernel_upcasters`] returns an empty chain and
//! [`CURRENT_SCHEMA_VERSION`] stays at `1`.  The seam is built and tested all
//! the same, so the first step that is owed has one site to be registered at.
//!
//! # Only upcasted events reach the domain
//!
//! The seam is a *type*, not a habit.  [`EventStore`] deals in raw
//! [`Value`]s — whatever shape the bytes were written in — while
//! [`CurrentStore`], which is deliberately **not** an [`EventStore`], reads
//! through the chain and hands back [`Current`]s.  Nothing else can build
//! one: the constructor is private to this module.  So every fold the kernel
//! runs takes `&[Current]`, and a read path that went round the chain does
//! not type-check rather than quietly folding a stale shape.

use std::ops::Deref;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde_json::{Map, Value};

use super::event::{FIELD_KIND, FIELD_SEQ};
// Only the `Vec`-backed test store below reads these — the durable backend
// stamps and selects in SQL.
#[cfg(test)]
use super::event::{kind_of, seq_of, validate_event, FIELD_EPOCH_MS};
#[cfg(test)]
use super::History;
use super::{KnlError, KnlResult};

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
/// backend in a [`CurrentStore`] carrying this chain, so every read a
/// session makes — the restore fold, the view folds, `events` — sees the
/// current shape while the stored bytes stay exactly as they were written.
///
/// Empty until the first release: there is no released shape to read yet, so
/// there is no step owed.  This is the one site a step is registered at, and
/// the seam around it is exercised by the tests below with a chain of their
/// own.
///
/// A step registered here that *renames a kind* owes one thing more: the
/// kind-filtered reads select on the stored name ([`EventStore::read_kinds`]),
/// so every caller that names that kind — [`super::Session::reserve`] and the
/// balance fold name the `budget_*` ones, a resume names `session_closed` —
/// has to name the old spelling beside the new one, or the older events fall
/// out of the fold.
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

/// An event as the current shape: read through the upcaster chain.
///
/// The proof that a fold is looking at today's shape, carried in the type.
/// A backend hands back what it stored, under whatever version it was
/// written with; only [`CurrentStore`] turns those into `Current`s, because
/// [`Current::from_upcasted`] is private to this module.  Every kernel fold
/// takes `&[Current]`, so a read that skipped the seam cannot be handed to
/// one — the call does not compile.
///
/// It derefs to the event's object, so a fold reads fields exactly as it did
/// when it was handed a plain map; [`Current::into_inner`] gives that map up
/// for a caller that has to own it (the Lua bridge, building tables).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current(Map<String, Value>);

impl Current {
    /// Take an upcasted event as the current shape.  Private to the seam:
    /// this is the one place a `Current` comes from.
    ///
    /// The debug assertion is the registration check.  Every stored event
    /// carries the version it was written under, and the chain's job is to
    /// bring it to [`CURRENT_SCHEMA_VERSION`]; an event that arrives here
    /// still older than that is a step nobody registered, which is exactly
    /// the mistake the seam exists to catch, and it is silent in every other
    /// way.  It is a `debug_assert` because it is a check on the kernel's own
    /// wiring, not on anything a caller passed in.
    ///
    /// A value that is not an object is corruption: the store's own writes
    /// are objects (the validator refuses anything else), so a non-object
    /// came back from the bytes rather than from a caller.
    fn from_upcasted(event: Value) -> KnlResult<Self> {
        let Value::Object(map) = event else {
            return Err(KnlError::Corruption(format!(
                "stored event is not an object: {event}"
            )));
        };
        debug_assert_eq!(
            map.get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION),
            "the upcaster chain must bring every event to the current schema \
             version; a step is missing from kernel_upcasters(): {map:?}"
        );
        Ok(Self(map))
    }

    /// Take a map as current *without* the chain — tests only.
    ///
    /// For the unit tests that drive a fold or a store directly, where there
    /// is no seam to read through and the fixture is written in today's shape
    /// by construction.  Not available outside `cfg(test)`, so it cannot
    /// become a way round [`CurrentStore`].
    #[cfg(test)]
    pub fn assume_current(event: Value) -> Self {
        match event {
            Value::Object(map) => Self(map),
            other => panic!("a test fixture event must be an object, got {other}"),
        }
    }

    /// The `kind` of the event (empty when absent).
    pub fn kind(&self) -> &str {
        self.0.get(FIELD_KIND).and_then(Value::as_str).unwrap_or("")
    }

    /// The store-assigned `seq` of the event (`0` when absent).
    pub fn seq(&self) -> u64 {
        self.0.get(FIELD_SEQ).and_then(Value::as_u64).unwrap_or(0)
    }

    /// Give up the event's object — for a caller that has to own it.
    pub fn into_inner(self) -> Map<String, Value> {
        self.0
    }
}

impl Deref for Current {
    type Target = Map<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for Current {
    /// The event as JSON — what a failure message about an event wants to
    /// show.  An object that will not serialise falls back to its debug
    /// form rather than failing the formatter: this runs while something is
    /// already being reported.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_string(&self.0) {
            Ok(json) => f.write_str(&json),
            Err(_) => write!(f, "{:?}", self.0),
        }
    }
}

/// What a command decides, given the stream it is decided against.
///
/// Handed the (kind-filtered) events in `seq` order and returning the event
/// to record — or `None` to record nothing.  [`EventStore::append_if`] runs
/// it inside the backend's serialization, which is what makes the decision
/// and the write one step.
///
/// **Owned, `Send` and `'static`, and it takes its input by value.**  That is
/// the whole shape of it, and it is what lets the decision travel to wherever
/// the backend's serialization actually lives — the SQLite backend's own
/// connection thread — so the read, the decision and the insert are one job
/// rather than two parties waiting on each other across a channel with the
/// write lock held.  A borrowed closure could not go anywhere, which is what
/// that round trip was paying for.
///
/// `FnOnce`, because one call to `append_if` takes one decision: a backend
/// that wanted to retry a contended transaction would have to be handed a
/// fresh one, which is why neither backend retries this call.
///
/// The backend's own form, in raw [`Value`]s: the kernel's commands are
/// written against [`CurrentDecision`] and [`CurrentStore`] projects between
/// the two.
pub type Decision = Box<dyn FnOnce(Vec<Value>) -> Option<Map<String, Value>> + Send + 'static>;

/// [`Decision`] as the kernel writes one: decided on upcasted events.
pub type CurrentDecision = Box<dyn FnOnce(Vec<Current>) -> Option<Map<String, Value>> + Send>;

/// What a two-stream command writes, split by where each part lands.
///
/// The SPI is otherwise scoped to a single stream, and this is the one shape
/// that is not: an allocation is a move *between* two ledgers, so the events
/// that record it belong to two streams and either both land or neither may
/// ([`EventStore::append_if_many`]).  Naming the two halves is what keeps
/// that from becoming a list the backend has to guess the routing of.
///
/// `own` is this store's stream — the one it was opened on — and `other` is
/// the stream named at the call.  Either may be empty: a refused allocation
/// writes the refusal on the parent and opens nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split<T> {
    /// What lands on this store's own stream.
    pub own: Vec<T>,
    /// What lands on the other stream of the same database.
    pub other: Vec<T>,
}

impl<T> Split<T> {
    /// A split that writes only this store's own stream.
    pub fn own(own: Vec<T>) -> Self {
        Self {
            own,
            other: Vec::new(),
        }
    }
}

/// A [`Decision`] that writes to two streams: the backend's form, in raw
/// [`Value`]s.
pub type SplitDecision =
    Box<dyn FnOnce(Vec<Value>) -> Option<Split<Map<String, Value>>> + Send + 'static>;

/// [`SplitDecision`] as the kernel writes one: decided on upcasted events.
pub type CurrentSplitDecision =
    Box<dyn FnOnce(Vec<Current>) -> Option<Split<Map<String, Value>>> + Send>;

/// What a closing event is built from once the store has found this stream's
/// open children ([`EventStore::append_with_open_children`]).
///
/// Not an `Option`: the event is recorded whatever the scan found, because
/// the children are something to *record* and never a reason to refuse a
/// close.  The ids arrive in the order the scan produced them.
pub type ChildrenDecision = Box<dyn FnOnce(Vec<String>) -> Map<String, Value> + Send + 'static>;

/// How a backend recognises the streams that were opened *from* this one.
///
/// The kernel's kind vocabulary is not the store's — [`EventStore`] takes the
/// kinds it reads as arguments everywhere else, and this is the same rule for
/// a scan that has to look at other streams: the caller says which kind
/// records an opening, which one records an ending, and which `data` field of
/// the opening names the parent.  The backend does the walk and knows nothing
/// about what the words mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildScan {
    /// The kind that records a stream's opening.
    pub opened: String,
    /// The kind that records a stream's ending.
    pub closed: String,
    /// The `data` field of `opened` that names the parent's stream.
    pub parent_field: String,
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
///
/// # Every call that can wait is `async`
///
/// A durable backend waits on something outside the process — a lock, a page,
/// a disk — and the caller here is, in the end, the Lua VM's thread, which is
/// the *only* worker of the runtime that also drives every other coroutine,
/// timer and cancellation that VM owns.  A store method that blocked would
/// stop all of them.  So the SPI is `async` and the waiting happens by
/// yielding: [`super::SqliteEventStore`] sends each call to the thread that
/// owns its connection and suspends on the answer.  The in-memory test store
/// waits on nothing and is `async` only to fit the shape.
///
/// [`EventStore::detach_append`] is the single exception, and deliberately so:
/// it is the drop backstop's path, where there is no caller left to wait.
#[async_trait]
pub trait EventStore: Send + Sync {
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
    async fn append(&mut self, event: Map<String, Value>) -> KnlResult<Committed>;

    /// Append `events` as one write, in the order given.
    ///
    /// For facts that are one fact: a session's `session_opened` and the
    /// `budget_granted` it opened under are not two things that happened,
    /// they are one opening, and a reader that can see the first without the
    /// second is reading a stream that never existed.
    ///
    /// **All or nothing on a backend that can do it.**  SQLite takes a
    /// single `IMMEDIATE` transaction, so a batch that fails part-way leaves
    /// the stream exactly as it was.  The default below is the most a
    /// backend with no transaction can offer — it appends one at a time, so
    /// a failure part-way leaves what already landed — and the in-memory
    /// store overrides it to validate the whole batch before writing any of
    /// it, which is the only way its writes fail.
    async fn append_many(&mut self, events: Vec<Map<String, Value>>) -> KnlResult<Vec<Committed>> {
        let mut committed = Vec::with_capacity(events.len());
        for event in events {
            committed.push(self.append(event).await?);
        }
        Ok(committed)
    }

    /// Decide *inside* the store's serialization: read the stream, ask
    /// `decide` what to write, and append its answer in the same write.
    ///
    /// `kinds` filters what the decision is shown, exactly as
    /// [`EventStore::read_kinds`] filters a read (`None` = the whole
    /// stream): a decision that folds the ledger asks for the `budget_*`
    /// kinds and is handed those, in `seq` order.  What it *writes* is
    /// unfiltered — the event it returns is appended whatever its kind.
    ///
    /// The form a command with an invariant takes — "reserve `n` only if the
    /// balance covers it".  `decide` is handed the stream's events (in `seq`
    /// order) as they are under the backend's lock, and returns the event to
    /// record, or `None` to record nothing (`Ok(None)`, with the stream
    /// untouched).  Because the read and the write share the transaction, the
    /// decision cannot be raced by a concurrent writer — which a
    /// compare-and-swap against a cached head could only detect afterwards.
    ///
    /// `decide` is called exactly once — it is a [`Decision`], which is
    /// `FnOnce` — so neither backend retries a contended `append_if`: what a
    /// second attempt would need is a second decision, and there is only one.
    /// A contended write surfaces as [`KnlError::Busy`] instead, which is the
    /// class that says another *call* is worth making.
    ///
    /// The kinds the decision asked for are read whole, which is what makes
    /// the invariant exact; naming them is what keeps that from meaning the
    /// whole stream.
    async fn append_if(
        &mut self,
        kinds: Option<&[&str]>,
        decide: Decision,
    ) -> KnlResult<Option<Committed>>;

    /// [`EventStore::append_if`] over two streams of one database: decide
    /// against this stream, and write to this one *and* `other` in the same
    /// transaction.
    ///
    /// The single-stream scoping above holds for everything else, and this is
    /// the one operation that cannot live inside it.  An allocation moves
    /// units from a parent's ledger to a child's: the reservation on one side
    /// and the opening plus the grant on the other are three records of one
    /// event, and a reader that could see either side alone would be reading
    /// units that had left one ledger without arriving in another — or a
    /// session that opened with a quota nobody paid for.
    ///
    /// `kinds` filters what the decision is shown *from this stream*, exactly
    /// as it does for [`EventStore::append_if`]; the other stream is written,
    /// never read.  A `None` decision writes nothing at all
    /// (`Ok(None)`), and a [`Split`] with an empty `other` writes only this
    /// stream — which is how a refusal is recorded without opening anything.
    ///
    /// **Both streams must be in one database**, which is the caller's to
    /// arrange ([`EventStore::database`] says which one this store is on): a
    /// backend has one connection and one transaction, so two databases have
    /// no atomicity to offer.
    ///
    /// The default refuses.  A store that keeps a single stream has no other
    /// stream to write to, and the request being well-formed while this
    /// backend cannot serve it is exactly [`KnlError::Unsupported`] — the
    /// same answer [`EventStore::query`] gives a store that is not a
    /// database.
    async fn append_if_many(
        &mut self,
        other: &str,
        kinds: Option<&[&str]>,
        decide: SplitDecision,
    ) -> KnlResult<Option<Split<Committed>>> {
        let _ = (other, kinds, decide);
        Err(KnlError::Unsupported(
            "this store keeps one stream, so it cannot write two in one transaction".to_string(),
        ))
    }

    /// Append the event a decision builds from the ids of this stream's *open
    /// children*, in one transaction with the scan that found them.
    ///
    /// The close path.  Which streams named this one as their parent, and
    /// which of those have not ended, is a question about the whole database,
    /// and asking it before the write would answer about a moment the write
    /// does not happen in — a child could end, or a new one open, in between,
    /// and the boundary would record something that was true just now.  So
    /// the scan and the insert share the transaction, and the decision runs
    /// between them.
    ///
    /// The decision is handed the ids and returns the event; there is no
    /// `Option`, because open children are a fact to record and never a
    /// reason to refuse a close ([`super::Session::close`]).
    ///
    /// The default is not a refusal but the truthful answer for a store that
    /// keeps one stream: it has no other streams, therefore no children, so
    /// the decision is shown an empty list and its event is appended
    /// normally.
    async fn append_with_open_children(
        &mut self,
        scan: &ChildScan,
        decide: ChildrenDecision,
    ) -> KnlResult<Committed> {
        let _ = scan;
        self.append(decide(Vec::new())).await
    }

    /// Which database this store's stream lives in, or `None` for a backend
    /// that is not one.
    ///
    /// Two stores answer with the same string exactly when they are the same
    /// database, which is the whole of what it is for: an allocation writes
    /// two streams in one transaction, so the kernel checks that the child's
    /// store is on the parent's database before it starts
    /// ([`super::Session::open_child`]) rather than discovering it as a
    /// half-written tree.  It is an identity, not a location a caller should
    /// take apart.
    fn database(&self) -> Option<&str> {
        None
    }

    /// Events of `kinds` with `seq >= from_seq`, at most `limit`, cloned in
    /// `seq` order.
    ///
    /// `None` reads every kind — the whole stream, as a plain range read.
    /// `Some(kinds)` reads only those, and `limit` counts what came back
    /// rather than what was skipped: a caller asking for two `budget_*`
    /// events gets two, however much else is in between.  An empty slice
    /// selects nothing.
    ///
    /// **The kind is the stored one.**  The selection happens in the backend,
    /// before [`CurrentStore`] runs the upcaster chain, so an event is found
    /// under the kind its bytes carry rather than the one it reads as.  While
    /// no registered step renames a kind the two are the same word; a step
    /// that *does* rename one obliges every filtered read of it to name both,
    /// and [`kernel_upcasters`] is the site that has to say so.
    ///
    /// Fallible: a durable backend can hit a transient busy read or a row it
    /// cannot decode, and those must surface rather than be silently dropped
    /// (a dropped row would let [`super::Session::resume`] re-fold a truncated
    /// log into the wrong state). The in-memory backend never errors.
    async fn read_kinds(
        &self,
        kinds: Option<&[&str]>,
        from_seq: u64,
        limit: usize,
    ) -> KnlResult<Vec<Value>>;

    /// Every event with `seq >= from_seq`, at most `limit`: the unfiltered
    /// [`EventStore::read_kinds`].
    async fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Value>> {
        self.read_kinds(None, from_seq, limit).await
    }

    /// The current head: the highest `seq`, or `None` for an empty stream.
    ///
    /// Fallible for the same reason as [`EventStore::read`]: a durable
    /// backend can hit a transient busy read, and swallowing it would make
    /// a populated stream look empty — the caller deciding open-vs-resume
    /// (or a CAS comparing heads) must see the fault, not a wrong answer.
    async fn head(&self) -> KnlResult<Option<u64>>;

    /// Number of recorded events.  Fallible like [`EventStore::head`].
    async fn len(&self) -> KnlResult<usize>;

    /// Whether nothing has been recorded yet.
    async fn is_empty(&self) -> KnlResult<bool> {
        Ok(self.len().await? == 0)
    }

    /// Answer a caller's own SQL over the log ([`super::query`]).
    ///
    /// The read side of a store that keeps its events in a table it can be
    /// asked about — which the product backend is, in both its file and its
    /// in-memory form.  The `plan` has already been validated (one statement,
    /// and it reads) and carries what the reserved parameters bind to; a
    /// backend answers it on a connection that cannot write.
    ///
    /// The default refuses, because a store that is not a database has no
    /// answer to give: the request is well-formed and this backend cannot
    /// serve it, which is what [`KnlError::Unsupported`] says.  Only the test
    /// doubles take it.
    ///
    /// **Queries read the stored shape, not the upcasted one.**  Every other
    /// read a session makes goes through the upcaster seam
    /// ([`CurrentStore`]); SQL runs against the bytes as they were written,
    /// because the chain is Rust and the query is SQLite's.  A caller reading
    /// across a schema change is reading the versions it finds — which is why
    /// `schema_version` is a column.
    async fn query(&self, plan: &super::query::QueryPlan) -> KnlResult<super::query::QueryRows> {
        let _ = plan;
        Err(KnlError::Unsupported(
            "this store keeps no queryable table".to_string(),
        ))
    }

    /// Submit `event` and do not wait for it to land — the drop backstop.
    ///
    /// The one synchronous call on this trait, because it is the one call with
    /// no caller left: a handle nobody closed records its `session_closed`
    /// from `Drop`, which cannot await and must not block (it runs inside a
    /// Lua collection cycle, on the VM's thread).  So the event is handed to
    /// whatever owns the writing, and whether it landed is reported to the log
    /// rather than to anyone.
    ///
    /// The default says so and records nothing: a backend that cannot accept a
    /// write without being awaited has nowhere to put this, and silently
    /// dropping it would leave the stream looking open forever with no trace
    /// of why.  [`super::SqliteEventStore`] overrides it.
    fn detach_append(&self, event: Map<String, Value>) {
        // Read out before the macro: `tracing`'s own `Value` trait is in
        // scope inside the expansion and would shadow `serde_json`'s here.
        let kind = event
            .get(FIELD_KIND)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        tracing::warn!(
            %kind,
            "knl: this store cannot record a detached append; the event was not written"
        );
    }
}

/// A [`Vec`]-backed [`EventStore`] — **tests only**.
///
/// The product has one backend: a session's log is a SQLite table whether it
/// is a file or an in-memory database ([`super::SqliteEventStore`]), because a
/// log that cannot be queried cannot serve the view layer that reads it with
/// SQL.  This one stays because the SPI, the upcasting seam and the folds are
/// worth exercising without a database underneath, and because the failure
/// injection the bridge's lifecycle tests need is easier to build on a
/// [`Vec`] than on a connection.  It is `#[cfg(test)]` so it cannot become a
/// second production backend by accident.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct MemEventStore {
    /// The append-only history that holds the events.
    history: History,
}

#[cfg(test)]
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

#[cfg(test)]
impl Default for MemEventStore {
    /// The same fresh store as [`MemEventStore::new`] — note this is *not*
    /// `History::default()`, whose `next_seq` would start at `0`.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[async_trait]
impl EventStore for MemEventStore {
    async fn append(&mut self, mut event: Map<String, Value>) -> KnlResult<Committed> {
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

    async fn append_many(&mut self, events: Vec<Map<String, Value>>) -> KnlResult<Vec<Committed>> {
        // Validation is the only way an in-memory append fails, so checking
        // the whole batch first is all this backend needs to make a batch
        // all-or-nothing: past this loop every append below lands.
        for event in &events {
            validate_event(event)?;
        }
        let mut committed = Vec::with_capacity(events.len());
        for event in events {
            committed.push(self.append(event).await?);
        }
        Ok(committed)
    }

    async fn append_if(
        &mut self,
        kinds: Option<&[&str]>,
        decide: Decision,
    ) -> KnlResult<Option<Committed>> {
        // One process, one owner: the read and the append below cannot be
        // interleaved with another writer's, which is all the serialization
        // this backend needs.
        let events = self.read_kinds(kinds, 0, usize::MAX).await?;
        match decide(events) {
            Some(event) => self.append(event).await.map(Some),
            None => Ok(None),
        }
    }

    async fn read_kinds(
        &self,
        kinds: Option<&[&str]>,
        from_seq: u64,
        limit: usize,
    ) -> KnlResult<Vec<Value>> {
        // The in-memory history is infallible; the `Ok` is the SPI's shape,
        // not a failure the mem backend can actually produce.
        let mut events = self.history.since(from_seq);
        if let Some(kinds) = kinds {
            // Filtered before the cap, so `limit` counts what the caller
            // asked for rather than what was skipped on the way.
            events.retain(|event| kinds.contains(&kind_of(event)));
        }
        events.truncate(limit);
        Ok(events)
    }

    async fn head(&self) -> KnlResult<Option<u64>> {
        // `seq` is monotonic and gap-free, so the last event carries the
        // highest one.  Infallible in memory; the `Ok` is the SPI's shape.
        Ok(self.history.events().last().map(seq_of))
    }

    async fn len(&self) -> KnlResult<usize> {
        Ok(self.history.len())
    }
}

/// The seam: the only way to read a stream as the current shape.
///
/// Wraps a backend and a `chain` of [`Upcaster`]s.  Reads fold the chain over
/// the events and hand back [`Current`]s (read-time projection); every write
/// passes straight through, so the stored bytes are never rewritten — the same
/// old-log-stays-readable discipline [`Upcaster`] describes, established once
/// here as the single site a future upcaster registers into.
///
/// **Not an [`EventStore`], on purpose.**  It offers the same calls, but its
/// reads are `Vec<Current>` rather than `Vec<Value>`, so it cannot stand in
/// for a backend and a backend cannot stand in for it.  [`super::Session`]
/// holds one of these and never a bare `Box<dyn EventStore>`, which is what
/// makes "the folds only ever see upcasted events" a property of the types
/// instead of a rule someone has to remember.
///
/// An empty chain is a functional no-op, which is the state today: v1 has no
/// upcaster, so the projection changes nothing, but a later shape change
/// registers its `n → n+1` step here and every read path picks it up.
pub struct CurrentStore {
    /// The wrapped backend that actually holds the events.
    inner: Box<dyn EventStore>,
    /// The read-time upcaster chain, applied front to back on every read.
    chain: Vec<Arc<dyn Upcaster>>,
}

impl CurrentStore {
    /// Wrap `inner` so its reads are upcasted through `chain`.
    ///
    /// An empty `chain` projects the events unchanged — they are already the
    /// current shape.
    pub fn new(inner: Box<dyn EventStore>, chain: Vec<Arc<dyn Upcaster>>) -> Self {
        Self { inner, chain }
    }

    /// Hand `event` to the backend without waiting for it to land
    /// ([`EventStore::detach_append`]).
    ///
    /// Straight through and not upcasted, like every other write.
    pub fn detach_append(&self, event: Map<String, Value>) {
        self.inner.detach_append(event);
    }

    /// Fold `chain` over `events` and take the results as [`Current`].
    ///
    /// The one place a `Current` is minted, so every one of them has been
    /// through the chain by construction.
    fn project(chain: &[Arc<dyn Upcaster>], events: Vec<Value>) -> KnlResult<Vec<Current>> {
        apply_upcasters(chain, events)
            .into_iter()
            .map(Current::from_upcasted)
            .collect()
    }

    /// Validate, stamp and append an event ([`EventStore::append`]).
    pub async fn append(&mut self, event: Map<String, Value>) -> KnlResult<Committed> {
        // Write path untouched: upcasting is read-time only.
        self.inner.append(event).await
    }

    /// Append events as one write ([`EventStore::append_many`]).
    pub async fn append_many(
        &mut self,
        events: Vec<Map<String, Value>>,
    ) -> KnlResult<Vec<Committed>> {
        self.inner.append_many(events).await
    }

    /// Decide inside the store's serialization ([`EventStore::append_if`]),
    /// on events projected to the current shape.
    pub async fn append_if(
        &mut self,
        kinds: Option<&[&str]>,
        decide: CurrentDecision,
    ) -> KnlResult<Option<Committed>> {
        // The decision is a read, so it is upcasted like every other read: the
        // backend hands over the stored events, the chain projects them, and
        // `decide` sees the current shape — a v1 log decides the same way a
        // v2 one does.  The projection travels with the decision, because the
        // decision travels: both run wherever the backend serializes its
        // writes, which for the durable one is its connection thread.
        let chain = self.chain.clone();
        // A projection failure has nowhere to go through the backend's
        // `Decision`, which answers with an event or nothing.  So it is parked
        // in a cell both sides can reach and raised below: `decide` is not
        // called, nothing is written, and the caller is told the read failed
        // rather than being handed the `None` that would read as "the
        // invariant said no".
        let failure: Arc<Mutex<Option<KnlError>>> = Arc::new(Mutex::new(None));
        let parked = Arc::clone(&failure);
        let upcasted: Decision =
            Box::new(
                move |events: Vec<Value>| match Self::project(&chain, events) {
                    Ok(current) => decide(current),
                    Err(fault) => {
                        *parked.lock().unwrap_or_else(PoisonError::into_inner) = Some(fault);
                        None
                    }
                },
            );
        let committed = self.inner.append_if(kinds, upcasted).await;
        let parked = failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match parked {
            Some(fault) => Err(fault),
            None => committed,
        }
    }

    /// Decide inside the store's serialization and write two streams of one
    /// database ([`EventStore::append_if_many`]), on events projected to the
    /// current shape.
    ///
    /// The same projection [`CurrentStore::append_if`] makes, for the same
    /// reason and with the same parking of a failure the backend's decision
    /// has nowhere to report one through: a read that could not be projected
    /// must not reach the decision as an empty stream, which is what a
    /// balance would fold to zero from.
    pub async fn append_if_many(
        &mut self,
        other: &str,
        kinds: Option<&[&str]>,
        decide: CurrentSplitDecision,
    ) -> KnlResult<Option<Split<Committed>>> {
        let chain = self.chain.clone();
        let failure: Arc<Mutex<Option<KnlError>>> = Arc::new(Mutex::new(None));
        let parked = Arc::clone(&failure);
        let upcasted: SplitDecision =
            Box::new(
                move |events: Vec<Value>| match Self::project(&chain, events) {
                    Ok(current) => decide(current),
                    Err(fault) => {
                        *parked.lock().unwrap_or_else(PoisonError::into_inner) = Some(fault);
                        None
                    }
                },
            );
        let committed = self.inner.append_if_many(other, kinds, upcasted).await;
        let parked = failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match parked {
            Some(fault) => Err(fault),
            None => committed,
        }
    }

    /// Append a closing event built from this stream's open children
    /// ([`EventStore::append_with_open_children`]).
    ///
    /// Straight through, and nothing to project: the decision is shown stream
    /// *ids*, not events, so there is no stored shape here for the chain to
    /// bring forward.
    pub async fn append_with_open_children(
        &mut self,
        scan: &ChildScan,
        decide: ChildrenDecision,
    ) -> KnlResult<Committed> {
        self.inner.append_with_open_children(scan, decide).await
    }

    /// Which database the backend's stream lives in ([`EventStore::database`]).
    pub fn database(&self) -> Option<&str> {
        self.inner.database()
    }

    /// Events of `kinds` from `from_seq` on, as the current shape
    /// ([`EventStore::read_kinds`]).
    pub async fn read_kinds(
        &self,
        kinds: Option<&[&str]>,
        from_seq: u64,
        limit: usize,
    ) -> KnlResult<Vec<Current>> {
        // The single read-time application point: read from the backend, then
        // fold the chain over the events before handing them back.
        let events = self.inner.read_kinds(kinds, from_seq, limit).await?;
        Self::project(&self.chain, events)
    }

    /// Every event from `from_seq` on, as the current shape.
    pub async fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Current>> {
        self.read_kinds(None, from_seq, limit).await
    }

    /// The backend's head ([`EventStore::head`]).
    pub async fn head(&self) -> KnlResult<Option<u64>> {
        self.inner.head().await
    }

    /// How many events the backend holds ([`EventStore::len`]).
    pub async fn len(&self) -> KnlResult<usize> {
        self.inner.len().await
    }

    /// Whether the backend holds nothing yet.
    pub async fn is_empty(&self) -> KnlResult<bool> {
        self.inner.is_empty().await
    }

    /// Answer a caller's SQL ([`EventStore::query`]).
    ///
    /// Straight through, and deliberately *not* upcasted: the chain is a Rust
    /// transform over whole events and a query selects columns, so there is
    /// nothing here to project.  A query reads the stored shape, which is why
    /// the version each row was written under is a column of the table.
    pub async fn query(
        &self,
        plan: &super::query::QueryPlan,
    ) -> KnlResult<super::query::QueryRows> {
        self.inner.query(plan).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::{KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT};
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

    /// A decision as [`EventStore::append_if`] takes one: owned, and handed
    /// its input by value.
    fn decide(
        f: impl FnOnce(Vec<Value>) -> Option<Map<String, Value>> + Send + 'static,
    ) -> Decision {
        Box::new(f)
    }

    /// The same, at the current shape ([`CurrentStore::append_if`]).
    fn decide_current(
        f: impl FnOnce(Vec<Current>) -> Option<Map<String, Value>> + Send + 'static,
    ) -> CurrentDecision {
        Box::new(f)
    }

    #[tokio::test]
    async fn append_assigns_gap_free_monotonic_seq_from_one() {
        let mut store = MemEventStore::new();
        assert!(store.is_empty().await.expect("is_empty"));
        assert_eq!(store.len().await.expect("len"), 0);

        let a = store.append(ev(1)).await.expect("append e1");
        let b = store.append(ev(2)).await.expect("append e2");
        let c = store.append(ev(3)).await.expect("append e3");

        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
        assert!(a.epoch_ms >= 1 || a.epoch_ms == 0, "epoch is stamped");
        assert_eq!(store.len().await.expect("len"), 3);
        assert!(!store.is_empty().await.expect("is_empty"));

        // The stamped epoch is what is stored.
        let stored = store.read(0, usize::MAX).await.expect("read");
        let stored_epoch = stored[0]
            .get(FIELD_EPOCH_MS)
            .and_then(Value::as_u64)
            .expect("epoch is on the stored event");
        assert_eq!(stored_epoch, a.epoch_ms);
    }

    #[tokio::test]
    async fn a_rejected_append_records_nothing_and_burns_no_seq() {
        let mut store = MemEventStore::new();
        store
            .append(obj(json!({ "text": "no kind" })))
            .await
            .expect_err("kind is required");
        assert_eq!(store.len().await.expect("len"), 0);
        assert_eq!(store.append(ev(1)).await.expect("append").seq, 1);
    }

    /// `append_if` decides on the stream the backend hands it and writes in
    /// the same step: a `Some` lands, a `None` writes nothing at all.
    #[tokio::test]
    async fn append_if_decides_on_the_stream_and_writes_only_a_some() {
        let mut store = MemEventStore::new();
        store.append(ev(1)).await.expect("seed");

        // The decision sees the stream as it is, in seq order.  It is owned
        // now, so what it saw comes back through a shared cell rather than a
        // borrow of a local.
        let seen = Arc::new(Mutex::new(0_usize));
        let counted = Arc::clone(&seen);
        let committed = store
            .append_if(
                None,
                decide(move |events| {
                    *counted.lock().expect("not poisoned") = events.len();
                    Some(ev(2))
                }),
            )
            .await
            .expect("append_if");
        assert_eq!(
            *seen.lock().expect("not poisoned"),
            1,
            "decide was handed the whole stream"
        );
        assert_eq!(committed.map(|c| c.seq), Some(2));

        // `None` is a decision too: nothing is written and no seq is burnt.
        let nothing = store
            .append_if(None, decide(|_| None))
            .await
            .expect("append_if");
        assert_eq!(nothing, None);
        assert_eq!(store.len().await.expect("len"), 2, "a None writes nothing");
        assert_eq!(store.append(ev(3)).await.expect("append").seq, 3);
    }

    /// The event a decision returns is validated like any other: a malformed
    /// one is refused and the stream is untouched.
    #[tokio::test]
    async fn append_if_validates_the_event_the_decision_returns() {
        let mut store = MemEventStore::new();
        store
            .append_if(None, decide(|_| Some(obj(json!({ "text": "no kind" })))))
            .await
            .expect_err("kind is required");
        assert_eq!(store.len().await.expect("len"), 0);
    }

    /// A decision names the kinds it folds, and is handed those and nothing
    /// else — in `seq` order, from the whole stream, however much else is in
    /// between.  What it *writes* is not filtered.
    #[tokio::test]
    async fn append_if_shows_the_decision_only_the_kinds_it_asked_for() {
        let mut store = MemEventStore::new();
        store
            .append(obj(
                json!({ "kind": KIND_BUDGET_GRANTED, "data": { "amount": 100 } }),
            ))
            .await
            .expect("the grant");
        store.append(ev(1)).await.expect("noise");
        store.append(ev(2)).await.expect("more noise");

        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = Arc::clone(&seen);
        let committed = store
            .append_if(
                Some(&[KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT]),
                decide(move |events| {
                    *recorded.lock().expect("not poisoned") =
                        events.iter().map(|e| kind_of(e).to_string()).collect();
                    Some(obj(
                        json!({ "kind": KIND_BUDGET_SPENT, "data": { "amount": 10 } }),
                    ))
                }),
            )
            .await
            .expect("append_if");
        assert_eq!(
            *seen.lock().expect("not poisoned"),
            [KIND_BUDGET_GRANTED],
            "only the kinds asked for"
        );
        assert_eq!(
            committed.map(|c| c.seq),
            Some(4),
            "the write is not filtered"
        );

        // The next decision sees what the last one wrote, since it is one of
        // the kinds it asked for.
        let recorded = Arc::clone(&seen);
        store
            .append_if(
                Some(&[KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT]),
                decide(move |events| {
                    *recorded.lock().expect("not poisoned") =
                        events.iter().map(|e| kind_of(e).to_string()).collect();
                    None
                }),
            )
            .await
            .expect("append_if");
        assert_eq!(
            *seen.lock().expect("not poisoned"),
            [KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT]
        );
    }

    /// A batch is one write: the events land in order, and a batch with a
    /// bad event in it lands nothing at all — the stream is as it was.
    #[tokio::test]
    async fn append_many_records_the_batch_in_order_or_records_nothing() {
        let mut store = MemEventStore::new();

        let committed = store
            .append_many(vec![ev(1), ev(2)])
            .await
            .expect("the batch");
        assert_eq!(
            committed.iter().map(|c| c.seq).collect::<Vec<_>>(),
            [1, 2],
            "the batch lands in the order it was given"
        );
        let stored = store.read(0, usize::MAX).await.expect("read");
        let kinds: Vec<&str> = stored.iter().map(kind_of).collect();
        assert_eq!(kinds, ["e1", "e2"]);

        // The second event is malformed, so neither is recorded: a batch that
        // half-lands is the shape `append_many` exists to rule out.
        store
            .append_many(vec![ev(3), obj(json!({ "text": "no kind" }))])
            .await
            .expect_err("kind is required");
        assert_eq!(
            store.len().await.expect("len"),
            2,
            "a failed batch wrote nothing"
        );
        assert_eq!(
            store.append(ev(4)).await.expect("append").seq,
            3,
            "no seq burnt"
        );
    }

    /// `read_kinds` selects by kind, pages by `from_seq` / `limit`, and reads
    /// nothing at all for an empty selection.
    #[tokio::test]
    async fn read_kinds_selects_by_kind_and_still_pages() {
        let mut store = MemEventStore::new();
        store
            .append(obj(
                json!({ "kind": KIND_BUDGET_GRANTED, "data": { "amount": 100 } }),
            ))
            .await
            .expect("grant");
        store.append(ev(1)).await.expect("noise");
        store
            .append(obj(
                json!({ "kind": KIND_BUDGET_SPENT, "data": { "amount": 10 } }),
            ))
            .await
            .expect("spend");
        store.append(ev(2)).await.expect("more noise");

        let ledger = store
            .read_kinds(
                Some(&[KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT]),
                0,
                usize::MAX,
            )
            .await
            .expect("read_kinds");
        let kinds: Vec<&str> = ledger.iter().map(kind_of).collect();
        assert_eq!(kinds, [KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT]);
        assert_eq!(
            seq_of(&ledger[1]),
            3,
            "the seq is the stream's, not the fold's"
        );

        // `from_seq` and `limit` still apply, and the cap counts what came
        // back rather than what was skipped.
        assert_eq!(
            store
                .read_kinds(Some(&[KIND_BUDGET_GRANTED]), 2, usize::MAX)
                .await
                .expect("read_kinds")
                .len(),
            0,
            "the grant is before from_seq"
        );
        assert_eq!(
            store
                .read_kinds(Some(&[KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT]), 0, 1)
                .await
                .expect("read_kinds")
                .len(),
            1
        );

        // An empty selection selects nothing; `None` is the whole stream.
        assert!(store
            .read_kinds(Some(&[]), 0, usize::MAX)
            .await
            .expect("read_kinds")
            .is_empty());
        assert_eq!(
            store
                .read_kinds(None, 0, usize::MAX)
                .await
                .expect("read_kinds")
                .len(),
            4,
            "read() is read_kinds(None, ..)"
        );
        assert_eq!(store.read(0, usize::MAX).await.expect("read").len(), 4);
    }

    #[tokio::test]
    async fn read_pages_by_from_seq_and_limit() {
        let mut store = MemEventStore::new();
        for i in 1..=5 {
            store.append(ev(i)).await.expect("append");
        }

        // from_seq filters, limit caps.
        assert_eq!(store.read(0, usize::MAX).await.expect("read").len(), 5);
        assert_eq!(store.read(1, usize::MAX).await.expect("read").len(), 5);
        assert_eq!(store.read(3, usize::MAX).await.expect("read").len(), 3);
        assert_eq!(store.read(6, usize::MAX).await.expect("read").len(), 0);

        let page = store.read(2, 2).await.expect("read");
        assert_eq!(page.len(), 2);
        assert_eq!(kind_of(&page[0]), "e2");
        assert_eq!(kind_of(&page[1]), "e3");

        // A zero limit returns nothing even when events match.
        assert!(store.read(0, 0).await.expect("read").is_empty());
    }

    #[tokio::test]
    async fn head_is_none_when_empty_then_tracks_the_max() {
        let mut store = MemEventStore::new();
        assert_eq!(store.head().await.expect("head"), None);

        store.append(ev(1)).await.expect("append");
        assert_eq!(store.head().await.expect("head"), Some(1));
        store.append(ev(2)).await.expect("append");
        assert_eq!(store.head().await.expect("head"), Some(2));

        // A rejected append does not move the head.
        store
            .append(obj(json!({ "text": "no kind" })))
            .await
            .expect_err("kind is required");
        assert_eq!(store.head().await.expect("head"), Some(2));
    }

    #[tokio::test]
    async fn default_matches_new_and_starts_seq_at_one() {
        let mut store = MemEventStore::default();
        assert!(store.is_empty().await.expect("is_empty"));
        // Guards against `History::default()` (next_seq == 0) leaking in.
        assert_eq!(store.append(ev(1)).await.expect("append").seq, 1);
    }

    #[tokio::test]
    async fn reads_are_copies_so_the_store_cannot_be_reached_through_them() {
        let mut store = MemEventStore::new();
        store.append(ev(1)).await.expect("append");
        let mut copy = store.read(0, usize::MAX).await.expect("read");
        copy[0][FIELD_KIND] = Value::String("TAMPERED".into());
        let again = store.read(0, usize::MAX).await.expect("read");
        assert_eq!(kind_of(&again[0]), "e1");
    }

    #[tokio::test]
    async fn append_stamps_the_current_schema_version() {
        let mut store = MemEventStore::new();
        store.append(ev(1)).await.expect("append");
        let stored = store.read(0, usize::MAX).await.expect("read");
        assert_eq!(
            stored[0].get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION),
            "a stored event carries the version it was written under: {}",
            stored[0]
        );
    }

    /// A store that keeps one stream has no second one to write, and no
    /// database to share with another store: both answers are "not this
    /// backend", said plainly, rather than a write that lands somewhere
    /// unexpected.
    #[tokio::test]
    async fn a_single_stream_store_cannot_write_two_streams() {
        let mut store = MemEventStore::new();
        assert_eq!(store.database(), None, "one stream, no database to share");

        let err = store
            .append_if_many(
                "another-stream",
                None,
                Box::new(|_| {
                    panic!("the decision must not run: there is nowhere for its other half to go")
                }),
            )
            .await
            .expect_err("two streams are a durable backend's");
        assert_eq!(err.kind(), KnlError::UNSUPPORTED, "{err}");
        assert!(!err.is_retryable(), "asking again changes nothing: {err}");
        assert_eq!(
            store.len().await.expect("len"),
            0,
            "and nothing was written"
        );
    }

    /// The same store has no *children* either — there are no other streams
    /// for one to be in — so a close over it is shown an empty list and its
    /// event is appended like any other.  That is the truthful answer, not a
    /// refusal: the question was asked and the answer is none.
    #[tokio::test]
    async fn a_single_stream_store_finds_no_children_and_appends_anyway() {
        let mut store = MemEventStore::new();
        let scan = ChildScan {
            opened: "session_opened".to_string(),
            closed: "session_closed".to_string(),
            parent_field: "parent".to_string(),
        };
        let seen: Arc<Mutex<Option<usize>>> = Arc::default();
        let counted = Arc::clone(&seen);
        let committed = store
            .append_with_open_children(
                &scan,
                Box::new(move |children| {
                    *counted.lock().expect("not poisoned") = Some(children.len());
                    ev(1)
                }),
            )
            .await
            .expect("the close lands");

        assert_eq!(*seen.lock().expect("not poisoned"), Some(0));
        assert_eq!(committed.seq, 1);
        assert_eq!(store.len().await.expect("len"), 1);
    }

    /// A store that is not a database says so, rather than answering a query
    /// with an empty result — which would read as "there is nothing there".
    /// The product backend is SQLite in every build; this is the answer the
    /// `Vec`-backed test store gives.
    #[tokio::test]
    async fn a_store_with_no_table_refuses_a_query() {
        use crate::knl::query::{plan, QueryOpts, QueryParams};

        let store = MemEventStore::new();
        let asked = plan(
            "SELECT 1",
            QueryParams::None,
            &QueryOpts::default(),
            "a-stream",
        )
        .expect("a plan");
        let err = store
            .query(&asked)
            .await
            .expect_err("there is no table to query");
        assert_eq!(err.kind(), KnlError::UNSUPPORTED, "{err}");
        assert!(!err.is_retryable(), "asking again changes nothing: {err}");
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

    /// (Fix 4) `CurrentStore` projects the chain on read while leaving the
    /// write path untouched: an appended event is stored raw, and the marker
    /// only appears in the read projection — it never accumulates, so the
    /// stored bytes carry no upcaster field.
    #[tokio::test]
    async fn the_seam_projects_on_read_and_leaves_writes_untouched() {
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
        let mut store = CurrentStore::new(Box::new(MemEventStore::new()), chain);

        // Write path passes through: coordinates and counters are the backend's.
        let a = store.append(ev(1)).await.expect("append e1");
        assert_eq!(a.seq, 1);
        assert_eq!(store.head().await.expect("head"), Some(1));
        assert_eq!(store.len().await.expect("len"), 1);
        assert!(!store.is_empty().await.expect("is_empty"));

        // read projects the marker on.
        let first = store.read(0, usize::MAX).await.expect("read");
        assert_eq!(first[0]["trace"], json!(["mark"]), "read applies the chain");

        // A second read is consistent — the marker does not accumulate, proving
        // the append stored no marker (the write path is not upcasted).
        let second = store.read(0, usize::MAX).await.expect("read again");
        assert_eq!(
            second[0]["trace"],
            json!(["mark"]),
            "stored bytes carry no marker; read adds exactly one"
        );

        // A freshly appended event reads back consistently under the same chain,
        // and head / len stay in step with the backend.
        let b = store.append(ev(2)).await.expect("append e2");
        assert_eq!(b.seq, 2);
        assert_eq!(store.head().await.expect("head"), Some(2));
        assert_eq!(store.len().await.expect("len"), 2);
        let both = store.read(0, usize::MAX).await.expect("read both");
        assert_eq!(both.len(), 2);
        assert_eq!(both[1].kind(), "e2");
        assert_eq!(both[1].seq(), 2, "a Current keeps its coordinates");
        assert_eq!(both[1]["trace"], json!(["mark"]));
    }

    /// (Fix 4) An empty chain makes the seam an identity over its backend:
    /// reads return the events unchanged, with no upcaster-added field, and the
    /// coordinates track the backend exactly.
    #[tokio::test]
    async fn the_seam_with_an_empty_chain_returns_events_unchanged() {
        let mut store = CurrentStore::new(Box::new(MemEventStore::new()), Vec::new());
        assert!(store.is_empty().await.expect("is_empty"));

        store.append(ev(1)).await.expect("append");
        let read = store.read(0, usize::MAX).await.expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].kind(), "e1", "the event passes through unchanged");
        assert!(
            read[0].get("trace").is_none(),
            "an empty chain adds nothing: {:?}",
            read[0]
        );
        assert_eq!(store.head().await.expect("head"), Some(1));
        assert_eq!(store.len().await.expect("len"), 1);
    }

    /// The kind filter selects on the kind that is *stored*; the chain runs
    /// after it.  A step that renames a kind therefore has to be read for
    /// under its old name — which is what the note on
    /// [`EventStore::read_kinds`] obliges a future rename to do, and this
    /// pins the behaviour so it cannot change silently.
    #[tokio::test]
    async fn the_kind_filter_selects_on_the_stored_kind_and_the_chain_runs_after() {
        /// Renames `old_spent` to the kind the kernel knows today.
        struct RenameSpent;
        impl Upcaster for RenameSpent {
            fn upcast(&self, mut event: Value) -> Value {
                let Some(map) = event.as_object_mut() else {
                    return event;
                };
                if map.get(FIELD_KIND).and_then(Value::as_str) == Some("old_spent") {
                    map.insert(FIELD_KIND.to_string(), Value::from(KIND_BUDGET_SPENT));
                }
                event
            }
        }

        let chain: Vec<Arc<dyn Upcaster>> = vec![Arc::new(RenameSpent)];
        let mut store = CurrentStore::new(Box::new(MemEventStore::new()), chain);
        store
            .append(obj(
                json!({ "kind": KIND_BUDGET_GRANTED, "data": { "amount": 100 } }),
            ))
            .await
            .expect("the grant");
        store
            .append(obj(
                json!({ "kind": "old_spent", "data": { "amount": 10 } }),
            ))
            .await
            .expect("a settlement under the older name");

        // Asked for by the name it is stored under, it comes back projected.
        let renamed = store
            .read_kinds(Some(&["old_spent"]), 0, usize::MAX)
            .await
            .expect("read_kinds");
        assert_eq!(renamed.len(), 1);
        assert_eq!(
            renamed[0].kind(),
            KIND_BUDGET_SPENT,
            "the chain ran after the selection"
        );

        // Asked for by the name it reads *as*, it is not selected at all.
        assert!(
            store
                .read_kinds(Some(&[KIND_BUDGET_SPENT]), 0, usize::MAX)
                .await
                .expect("read_kinds")
                .is_empty(),
            "the filter cannot see a kind the chain has not produced yet"
        );

        // The decision is handed the projected events either way.
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = Arc::clone(&seen);
        store
            .append_if(
                None,
                decide_current(move |events| {
                    *recorded.lock().expect("not poisoned") =
                        events.iter().map(|e| e.kind().to_string()).collect();
                    None
                }),
            )
            .await
            .expect("append_if");
        assert_eq!(
            *seen.lock().expect("not poisoned"),
            [KIND_BUDGET_GRANTED, KIND_BUDGET_SPENT]
        );
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
    #[tokio::test]
    async fn new_events_are_stamped_with_the_current_version() {
        let mut store = MemEventStore::new();
        store.append(ev(1)).await.expect("append");
        let stored = store.read(0, usize::MAX).await.expect("read");
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
