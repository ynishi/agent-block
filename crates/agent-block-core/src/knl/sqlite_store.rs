//! The durable [`EventStore`]: one stream of an eventsdb log.
//!
//! [`SqliteEventStore`] is an *adapter*.  The kernel's [`EventStore`] is the
//! kernel's SPI and does not move; underneath it is
//! [`eventsdb_sqlite`] — a SQLite event log with a writer thread of its own, a
//! pool of read-only connections beside it, a migration ladder for the table's
//! shape and a transaction hatch for the two writes the kernel cannot express
//! any other way.  What is left here is the translation: the kernel's
//! vocabulary in, eventsdb's out, and back.
//!
//! ```text
//!   knl::Logs ──▶ SqliteEventLog ──stream_handle(id)──▶ eventsdb SqliteEventStore
//!                       │                                        ▲
//!                       │ with_transaction(TxnContext)           │ delegate
//!                       ▼                                        │
//!            append_if_many / append_with_open_children     append / append_many
//!            (two streams, and the child scan)              append_if / reads
//! ```
//!
//! # What the adapter owns, and what it hands over
//!
//! Two things are the kernel's and stay here:
//!
//! - **[`validate_event`]**, the kernel's own rules — the envelope, and the
//!   `data` of the six kinds the kernel writes ([`super::event`]).  eventsdb
//!   checks the envelope too and knows nothing of a kind's shape, so the
//!   kernel's check runs first, on every write path including the ones a
//!   decision produces;
//! - **the schema version.**  eventsdb takes the version from the event's
//!   author and only fills in a default for an author who did not say
//!   ([`eventsdb_core::event::stamp`]), so every append here stamps
//!   [`CURRENT_SCHEMA_VERSION`] on the way past.  `seq` and `epoch_ms` are
//!   removed for the same reason in reverse: they are the store's to assign,
//!   so a caller-supplied one is dropped rather than trusted.
//!
//! Everything else is eventsdb's: the `IMMEDIATE` transaction every write
//! takes, the busy retry, the per-stream `seq` counter, the global `position`,
//! the upcaster chain, and the read-only connections a query runs on.
//!
//! # The chain runs once, and it runs down there
//!
//! [`kernel_upcasters`] is registered on the *log* ([`super::Logs`]), because
//! eventsdb applies it to everything it reads — `read_kinds`, `read_last`, and
//! the events a decision is shown inside its transaction.  So the seam above
//! this ([`super::CurrentStore`]) carries an **empty** chain: its job here is
//! the type, not the transform.  It still checks what it is handed
//! ([`super::Current`]), which is what keeps "only upcasted events reach the
//! domain" a property rather than a convention.
//!
//! # The two writes that go through the hatch
//!
//! [`EventStore::append_if_many`] and [`EventStore::append_with_open_children`]
//! are the two operations that are not about one stream, and both are one
//! transaction by necessity rather than for convenience: an allocation moves
//! units between two ledgers, and a close records the children that had not
//! ended *as of the write that records it*.  `log.with_transaction` hands over
//! a [`TxnContext`] — the log's own stamped `append` / `append_many` / `read`,
//! and a raw [`rusqlite::Transaction`] underneath for the child scan's
//! `SELECT`.  Raw writes to `events` are refused there by SQLite's own
//! authorizer, which is the point: an append cannot skip validation or
//! sequencing by going round the side.
//!
//! # The read side
//!
//! [`EventStore::query`] is [`SqliteEventLog::query_timeout`]: the caller's
//! statement, positional values ([`super::query`] resolved them), a deadline,
//! and a read-only connection that is not the writer.  The row cap is the
//! kernel's and is applied by *wrapping* the statement — `SELECT * FROM (…)
//! LIMIT n + 1` — so one more row than the caller allowed is read and the
//! extra one is what says the answer was cut ([`QueryRows::truncated`]).
//!
//! A `NULL` column comes back from eventsdb as a JSON null and is dropped from
//! the row here, so the Lua side reads an absent key as `nil`, which is what a
//! missing column means there.
//!
//! [`TxnContext`]: eventsdb_sqlite::TxnContext

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use eventsdb_core::store::EventStore as EventsdbStore;
use eventsdb_core::upcast::Current as Upcasted;
use eventsdb_sqlite::SqliteEventLog;
use serde_json::{Map, Value};

use super::event::{validate_event, FIELD_EPOCH_MS, FIELD_SEQ};
use super::event_store::{
    stamp_schema_version, ChildScan, ChildrenDecision, Committed, Decision, EventStore, Split,
    SplitDecision,
};
use super::logs::Logs;
use super::query::{QueryPlan, QueryRows};
use super::{KnlError, KnlResult};

/// The table the log lives in — published as the read contract
/// ([`events_schema`]).
pub const EVENTS_TABLE: &str = "events";

/// The index the close-time child scan reads by.
///
/// Created once per log open ([`super::Logs`]) rather than declared in a DDL,
/// because the table's shape is eventsdb's and this index is the kernel's:
/// which openings name *this* stream as their parent is a question about the
/// whole database, and without an index it is answered by walking every event
/// in it.
///
/// It is a *partial expression* index and both halves are load-bearing.  The
/// expression is written exactly as the scan writes it, because SQLite matches
/// an indexed expression against a query's by form — a path bound as a
/// parameter would never match one written as a literal, which is why
/// [`child_scan_sql`] spells its words out.  The `WHERE` keeps the index to
/// the openings: `parent` lives on `session_opened` and nowhere else, so
/// indexing every row would be storing a NULL per event to find the handful
/// that are not.
///
/// That is the kernel's vocabulary sitting in the store's schema, which the
/// rest of this backend avoids ([`ChildScan`] is an argument, not a constant).
/// The price is that those words are settled at open time; what it buys is
/// that a close on a large log looks the openings up instead of walking the
/// table.  A scan under some other vocabulary still reads correctly — it just
/// reads without the index.
pub(super) const CHILD_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
     events_session_opened_parent \
         ON events (json_extract(data, '$.parent')) \
      WHERE kind = 'session_opened';";

/// One column of [`EVENTS_TABLE`].
///
/// Published to the shell so a caller writing SQL against the log reads the
/// column names and types from the kernel rather than from a list somebody
/// retyped — and so a test can hold the shell's declaration of the schema
/// against the table that actually exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaColumn {
    /// The column name.
    pub name: String,
    /// Its declared type, as written in the DDL.
    pub declared_type: String,
    /// Whether it is part of the primary key.
    pub pk: bool,
}

/// The columns of the `events` table, in the order SQLite reports them.
///
/// A constant rather than a `PRAGMA table_info` against a throwaway database,
/// for two reasons: the table is the store's and its DDL runs inside a
/// migration ladder that is `async` — while `knl.api()` is a declaration of
/// the surface, which should not have to be awaited — and a pragma is one of
/// the things the store's hatch refuses, since setting one is how the ladder's
/// own marker would be changed underneath it.  What keeps the constant honest
/// is a test: it opens a real log and holds this list against what SQLite says
/// the table has, so the two cannot drift apart unnoticed.
///
/// `position` is the key — the global order, dense and gap-free as read —
/// and `(stream, seq)` is a unique constraint beside it rather than the
/// primary key it used to be.  There is no `beat` column: the beat is a label
/// of `meta` ([`super::event`]) and a read reaches it with
/// `json_extract(meta, '$.beat')`, which the log has an index for.
const EVENTS_COLUMNS: [(&str, &str, bool); 8] = [
    ("position", "INTEGER", true),
    ("stream", "TEXT", false),
    ("seq", "INTEGER", false),
    ("epoch_ms", "INTEGER", false),
    ("kind", "TEXT", false),
    ("schema_version", "INTEGER", false),
    ("meta", "TEXT", false),
    ("data", "TEXT", false),
];

/// The columns of the `events` table, without a session to ask.
///
/// The read contract, as data: what a caller's SQL may name.  It is what
/// `knl.api()` publishes, and it is fallible only because it always was —
/// there is nothing here that can fail now, and the shape is kept so a later
/// backend that has to open something to answer can.
pub fn events_schema() -> KnlResult<Vec<SchemaColumn>> {
    Ok(EVENTS_COLUMNS
        .iter()
        .map(|(name, declared_type, pk)| SchemaColumn {
            name: (*name).to_string(),
            declared_type: (*declared_type).to_string(),
            pk: *pk,
        })
        .collect())
}

/// A durable [`EventStore`] backed by an eventsdb log, scoped to one `stream`.
///
/// The session *is* the stream: one instance serves one session's log.  Several
/// instances may point at the same log with different streams, and that is
/// what a session tree is.
pub struct SqliteEventStore {
    /// The log the stream lives in.  Held so the database-level calls — the
    /// transaction hatch, the query, the detached append — are reachable, and
    /// so the log outlives every handle it issued.
    log: Arc<SqliteEventLog>,
    /// eventsdb's own handle on this stream: the per-stream calls delegate
    /// straight to it.
    handle: eventsdb_sqlite::SqliteEventStore,
    /// The stream this store is scoped to — the session id.
    stream: String,
}

impl SqliteEventStore {
    /// Open (creating if absent) the log at `path`, scoped to `stream`.
    ///
    /// `logs` is where the open log is kept: a file is opened once per process
    /// and shared from then on, so two sessions on one file are two streams of
    /// one log rather than two logs racing for one file ([`Logs`]).
    pub async fn open(path: &Path, stream: impl Into<String>, logs: &Logs) -> KnlResult<Self> {
        Ok(Self::on(logs.file(path).await?, stream))
    }

    /// Open on the in-memory log, scoped to `stream`.
    ///
    /// One database per [`Logs`], not per stream: an ephemeral session is a
    /// stream in it like any other, so it can have children and can be resumed
    /// by name for as long as the host lives.  What it cannot do is survive
    /// the process, and it does not pretend to.
    pub async fn open_memory(stream: impl Into<String>, logs: &Logs) -> KnlResult<Self> {
        Ok(Self::on(logs.memory().await?, stream))
    }

    /// A store on a log that is already open.
    ///
    /// The form a child takes: it is opened on its parent's log, which the
    /// caller already has ([`Logs::database`]), and issuing a handle on it
    /// waits for nothing.
    pub fn on(log: Arc<SqliteEventLog>, stream: impl Into<String>) -> Self {
        let stream = stream.into();
        let handle = log.stream_handle(&stream);
        Self {
            log,
            handle,
            stream,
        }
    }
}

/// The kinds a read was asked for, owned, so the selection can travel into a
/// closure that outlives the caller's slice.
fn owned_kinds(kinds: Option<&[&str]>) -> Option<Vec<String>> {
    kinds.map(|kinds| kinds.iter().map(|kind| (*kind).to_string()).collect())
}

/// Borrow an owned kind list back into the shape the read takes.
fn borrowed_kinds(kinds: &Option<Vec<String>>) -> Option<Vec<&str>> {
    kinds
        .as_ref()
        .map(|kinds| kinds.iter().map(String::as_str).collect())
}

/// An event on its way to the store: the kernel's coordinates removed, and
/// the kernel's schema version stamped.
///
/// `seq` and `epoch_ms` are the store's to assign, so an event that carries
/// either has it dropped rather than trusted — eventsdb refuses a stored
/// coordinate on a new write, and silently accepting one would be a caller
/// choosing where its event lands.  The version goes the other way: eventsdb
/// takes it from the author and only defaults it, and the kernel *is* the
/// author.
fn prepared(mut event: Map<String, Value>) -> Map<String, Value> {
    event.remove(FIELD_SEQ);
    event.remove(FIELD_EPOCH_MS);
    stamp_schema_version(&mut event);
    event
}

/// eventsdb's coordinates as the kernel's.
///
/// The global `position` is dropped: the kernel's SPI is scoped to one stream,
/// and `seq` is the coordinate inside it.
fn committed_of(committed: eventsdb_core::position::Committed) -> Committed {
    Committed {
        seq: committed.seq,
        epoch_ms: committed.epoch_ms,
    }
}

/// Upcasted events as the raw [`Value`]s the kernel's SPI deals in.
///
/// eventsdb has already run the chain, so what comes back is the current
/// shape; the seam above turns these back into [`super::Current`]s, which is
/// where the version is checked.
fn values_of(events: Vec<Upcasted>) -> Vec<Value> {
    events
        .into_iter()
        .map(|event| Value::Object(event.into_inner()))
        .collect()
}

/// The same, for a decision's input, which arrives borrowed.
fn values_of_ref(events: &[Upcasted]) -> Vec<Value> {
    events
        .iter()
        .map(|event| Value::Object((**event).clone()))
        .collect()
}

/// Where a kernel error goes when it happens inside a closure that has no way
/// to report one.
///
/// eventsdb's decisions answer with an event or with nothing, and its hatch
/// answers in eventsdb's own error language.  A kernel refusal — an event a
/// decision built wrong — is neither, so it is parked in a cell both sides can
/// reach and raised by the caller: nothing is written, and the caller is told
/// what was wrong rather than being handed the `Ok(None)` that would read as
/// "the invariant said no".
type Parked = Arc<Mutex<Option<KnlError>>>;

/// Take whatever was parked, if anything.
fn taken(parked: &Parked) -> Option<KnlError> {
    parked.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Park `error` for the caller to raise.
fn park(parked: &Parked, error: KnlError) {
    *parked.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
}

/// The refusal handed to eventsdb when a kernel error was parked: it rolls the
/// transaction back, and the caller replaces it with the parked one.
fn rolled_back() -> eventsdb_core::Error {
    eventsdb_core::Error::validation("the kernel refused an event this write was to record")
}

#[async_trait]
impl EventStore for SqliteEventStore {
    async fn append(&mut self, event: Map<String, Value>) -> KnlResult<Committed> {
        // Reject before touching the stream: a rejected event burns no seq.
        validate_event(&event)?;
        self.handle
            .append(prepared(event))
            .await
            .map(committed_of)
            .map_err(KnlError::from)
    }

    async fn append_many(&mut self, events: Vec<Map<String, Value>>) -> KnlResult<Vec<Committed>> {
        // Validate before the transaction is opened: a batch with a malformed
        // event in it never takes the write lock at all.
        for event in &events {
            validate_event(event)?;
        }
        let events: Vec<Map<String, Value>> = events.into_iter().map(prepared).collect();
        self.handle
            .append_many(events)
            .await
            .map(|committed| committed.into_iter().map(committed_of).collect())
            .map_err(KnlError::from)
    }

    async fn append_if(
        &mut self,
        kinds: Option<&[&str]>,
        decide: Decision,
    ) -> KnlResult<Option<Committed>> {
        // The read, the decision and the insert share one IMMEDIATE
        // transaction on the log's own thread, so the invariant `decide`
        // checks holds at the instant the event lands.  The decision travels
        // with the job — it is owned and `Send` — so nothing waits on
        // anything else with the write lock held.
        let parked: Parked = Arc::default();
        let sink = Arc::clone(&parked);
        let answer: eventsdb_core::store::Decision = Box::new(move |seen: &[Upcasted]| {
            let event = decide(values_of_ref(seen))?;
            // The decision's event is the kernel's to check: eventsdb checks
            // the envelope and knows nothing of a kernel kind's `data`.  A
            // refusal parks and writes nothing, rather than reading as a
            // decision that said no.
            match validate_event(&event) {
                Ok(()) => Some(prepared(event)),
                Err(refusal) => {
                    park(&sink, refusal);
                    None
                }
            }
        });
        let committed = self.handle.append_if(kinds, answer).await;
        match taken(&parked) {
            Some(refusal) => Err(refusal),
            None => committed
                .map(|committed| committed.map(committed_of))
                .map_err(KnlError::from),
        }
    }

    async fn append_if_many(
        &mut self,
        other: &str,
        kinds: Option<&[&str]>,
        decide: SplitDecision,
    ) -> KnlResult<Option<Split<Committed>>> {
        // One transaction over both streams: they are rows of one table on one
        // connection, so "two streams" costs the write nothing beyond a second
        // counter read.  Not retried, because the decision is a `FnOnce` and
        // an attempt consumes it.
        let stream = self.stream.clone();
        let other = other.to_string();
        let kinds = owned_kinds(kinds);
        let parked: Parked = Arc::default();
        let sink = Arc::clone(&parked);

        let committed = self
            .log
            .with_transaction(move |tx| {
                let selection = borrowed_kinds(&kinds);
                let seen = Split {
                    own: values_of(tx.read(&stream, selection.as_deref(), 0, usize::MAX)?),
                    // Unfiltered and capped at one: the question is "is there
                    // an event", not "which", so a kind filter could only make
                    // an occupied stream look empty.
                    other: values_of(tx.read(&other, None, 0, 1)?),
                };
                let Some(split) = decide(seen) else {
                    // Nothing to write: the transaction is rolled back.
                    return Ok(None);
                };
                for event in split.own.iter().chain(split.other.iter()) {
                    if let Err(refusal) = validate_event(event) {
                        park(&sink, refusal);
                        return Err(rolled_back());
                    }
                }
                let own = tx.append_many(
                    &stream,
                    split.own.into_iter().map(prepared).collect::<Vec<_>>(),
                )?;
                let elsewhere = tx.append_many(
                    &other,
                    split.other.into_iter().map(prepared).collect::<Vec<_>>(),
                )?;
                Ok(Some(Split {
                    own: own.into_iter().map(committed_of).collect(),
                    other: elsewhere.into_iter().map(committed_of).collect(),
                }))
            })
            .await;

        match taken(&parked) {
            Some(refusal) => Err(refusal),
            None => committed.map_err(KnlError::from),
        }
    }

    async fn append_with_open_children(
        &mut self,
        scan: &ChildScan,
        decide: ChildrenDecision,
    ) -> KnlResult<Committed> {
        // The scan reads other streams and the insert writes this one, so they
        // share the transaction: what the boundary records is what was true at
        // the instant it landed, not a moment before it.
        let stream = self.stream.clone();
        let scan = scan.clone();
        let parked: Parked = Arc::default();
        let sink = Arc::clone(&parked);

        let committed = self
            .log
            .with_transaction(move |tx| {
                // The raw transaction underneath the context: the scan is a
                // `SELECT` over `events`, which the hatch allows and has no
                // stamped equivalent of.
                let conn: &rusqlite::Connection = tx;
                let children = open_children_in(conn, &stream, &scan)
                    .map_err(|e| eventsdb_core::Error::storage(e.to_string()))?;
                let event = decide(children);
                if let Err(refusal) = validate_event(&event) {
                    park(&sink, refusal);
                    return Err(rolled_back());
                }
                tx.append(&stream, prepared(event)).map(committed_of)
            })
            .await;

        match taken(&parked) {
            Some(refusal) => Err(refusal),
            None => committed.map_err(KnlError::from),
        }
    }

    fn database(&self) -> Option<&str> {
        Some(self.log.database())
    }

    async fn read_kinds(
        &self,
        kinds: Option<&[&str]>,
        from_seq: u64,
        limit: usize,
    ) -> KnlResult<Vec<Value>> {
        self.handle
            .read_kinds(kinds, from_seq, limit)
            .await
            .map(values_of)
            .map_err(KnlError::from)
    }

    async fn read_last(&self, n: usize) -> KnlResult<Vec<Value>> {
        self.handle
            .read_last(n)
            .await
            .map(values_of)
            .map_err(KnlError::from)
    }

    async fn head(&self) -> KnlResult<Option<u64>> {
        self.handle.head().await.map_err(KnlError::from)
    }

    async fn len(&self) -> KnlResult<usize> {
        self.handle.len().await.map_err(KnlError::from)
    }

    async fn query(&self, plan: &QueryPlan) -> KnlResult<QueryRows> {
        // The cap is the kernel's, and the statement is the caller's, so the
        // one is put around the other: `limit + 1` rows are asked for and the
        // extra one is what says the answer was cut.  `plan.sql` is one
        // statement with no trailing `;` ([`super::query`]), which is what
        // makes it a subquery rather than a splice.
        let cap = i64::try_from(plan.limit)
            .unwrap_or(i64::MAX)
            .saturating_add(1);
        let sql = format!("SELECT * FROM ({}) LIMIT {cap}", plan.sql);
        let rows = self
            .log
            .query_timeout(&sql, plan.values.clone(), plan.timeout)
            .await
            .map_err(KnlError::from)?;

        let truncated = rows.len() > plan.limit;
        Ok(QueryRows {
            rows: rows
                .into_iter()
                .take(plan.limit)
                // A NULL is an absent key rather than a null value: the Lua
                // side reads it as `nil`, which is what a missing column means
                // there.
                .map(|row| {
                    row.into_iter()
                        .filter(|(_, value)| !value.is_null())
                        .collect()
                })
                .collect(),
            truncated,
        })
    }

    fn detach_append(&self, event: Map<String, Value>) {
        // The drop backstop's path, and the one write nobody awaits.  A handle
        // that was collected has no caller left to raise to and no task left to
        // wait in, so the job is handed to the log's own queue and let go of:
        // it lands before the host drains that queue at shutdown
        // ([`Logs::shutdown`]).
        if let Err(e) = validate_event(&event) {
            tracing::warn!(error = %e, "knl: a detached append was refused before it was submitted");
            return;
        }
        if let Err(e) = self.log.detach_append(&self.stream, prepared(event)) {
            tracing::warn!(error = %e, "knl: a detached append was not accepted by the log");
        }
    }
}

/// `text` as an SQL string literal, with any quote in it doubled.
///
/// For the two places a *word* rather than a value has to go into a statement
/// ([`child_scan_sql`]): `json_extract`'s path argument is not a value SQLite
/// will take a parameter for, and a term the planner has to compare against a
/// partial index's `WHERE` cannot be one either.  Doubling is the whole of
/// SQLite's escaping rule for a single-quoted literal, so this closes the hole
/// that interpolating text otherwise opens.
fn sql_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The statement [`open_children_in`] runs, with the scan's two words written
/// into it as literals.
///
/// The kind and the JSON path are literals rather than parameters *so that the
/// planner can see them*: [`CHILD_INDEX_DDL`] is a partial index on an
/// expression, and both halves are matched by form — a `kind = ?` term proves
/// nothing about `WHERE kind = 'session_opened'`, and a bound path never
/// matches an indexed one.  The parent being looked for stays a parameter,
/// because it is a value.  A test holds the query plan against the index name,
/// so this cannot quietly become a table scan again.
fn child_scan_sql(scan: &ChildScan) -> String {
    let opened = sql_literal(&scan.opened);
    let closed = sql_literal(&scan.closed);
    let path = sql_literal(&format!("$.{}", scan.parent_field));
    format!(
        "SELECT opened.stream \
           FROM events AS opened \
          WHERE opened.kind = {opened} \
            AND json_extract(opened.data, {path}) = ?1 \
            AND NOT EXISTS ( \
                SELECT 1 FROM events AS ending \
                 WHERE ending.stream = opened.stream AND ending.kind = {closed} \
            ) \
          ORDER BY opened.epoch_ms, opened.stream"
    )
}

/// The streams that name `stream` as their parent and carry no ending.
///
/// The vocabulary is the caller's ([`ChildScan`]): which kind opens a stream,
/// which kind ends one, and where in the opening's `data` the parent is named.
/// Those three words are written into the statement ([`child_scan_sql`])
/// rather than bound, which is what lets the scan read by the parent index
/// instead of walking every event in the database.
///
/// Ordered by when each child opened, so a close records its children in the
/// order they were started rather than in whatever order the rows came back.
fn open_children_in(
    conn: &rusqlite::Connection,
    stream: &str,
    scan: &ChildScan,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&child_scan_sql(scan))?;
    let rows = stmt.query_map(rusqlite::params![stream], |row| row.get::<_, String>(0))?;
    rows.collect()
}

/// Whether a rusqlite error is a retryable lock contention (matched on the
/// SQLite error *code*, never the message text).
fn is_retryable(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

/// Classify a rusqlite error into the kernel's vocabulary.
///
/// The store's own SQL goes through eventsdb, which has its own classification
/// ([`From<eventsdb_core::Error>`]); what is left on this path is the SQL the
/// kernel runs itself — the legacy migration's plain connection
/// ([`super::logs`]).  The split is the one the caller can act on: a contended
/// lock is [`KnlError::Busy`] — the same call may succeed if it is made again
/// — and everything else is [`KnlError::Storage`], a fault the kernel cannot
/// promise anything about.  Matched on the SQLite error *code*, never the
/// message text, so the classification does not drift with a library's
/// wording.
impl From<rusqlite::Error> for KnlError {
    fn from(error: rusqlite::Error) -> Self {
        if is_retryable(&error) {
            return KnlError::Busy(format!("sqlite: busy/locked: {error}"));
        }
        KnlError::Storage(format!("sqlite: {error}"))
    }
}

/// Translate the store's failure into the kernel's vocabulary.
///
/// One class each, because the two vocabularies were drawn along the same line
/// — what a caller can *do* about it — and the six that exist on both sides
/// mean the same thing on both sides.  The message travels as it was written:
/// it is the reason, and the kernel renders the class itself.
///
/// The rest go to [`KnlError::Storage`] with their text.  `Truncated`,
/// `HeadMismatch`, and the two retention refusals are answers to calls this
/// kernel does not make — nothing here removes history, and no append names
/// the head it expects — so a caller meeting one is meeting the store failing
/// to do the work, which is what `Storage` says.  The enum is
/// `#[non_exhaustive]`, so a class added later lands there too rather than
/// failing to compile.
impl From<eventsdb_core::Error> for KnlError {
    fn from(error: eventsdb_core::Error) -> Self {
        use eventsdb_core::Error as Failure;
        match error {
            Failure::Validation(reason) => KnlError::Validation(reason),
            Failure::Busy(reason) => KnlError::Busy(reason),
            Failure::Timeout(reason) => KnlError::Timeout(reason),
            Failure::Storage(reason) => KnlError::Storage(reason),
            Failure::Corruption(reason) => KnlError::Corruption(reason),
            Failure::Unsupported(reason) => KnlError::Unsupported(reason),
            other => KnlError::Storage(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::{kind_of, seq_of, FIELD_DATA, FIELD_META};
    use crate::knl::query::{self, QueryOpts, QueryParams};
    use crate::knl::CURRENT_SCHEMA_VERSION;
    use serde_json::json;

    /// Object map for an event literal.
    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture must be an object, got {other}"),
        }
    }

    /// An event of a caller's own kind, named `e{i}`.
    fn ev(i: usize) -> Map<String, Value> {
        obj(json!({ "kind": format!("e{i}") }))
    }

    /// A `budget_*` event of `amount`, as the kernel writes one.
    fn budget(kind: &str, amount: i64) -> Map<String, Value> {
        obj(json!({ "kind": kind, "data": { "amount": amount } }))
    }

    /// A store on an in-memory log of its very own.
    ///
    /// The [`Logs`] comes back with the store because the caller has to hold
    /// it: it owns the log, and a test that dropped it early would be pulling
    /// the database out from under its own assertions.  One `Logs` per test is
    /// also what keeps two tests running in parallel out of each other's log.
    async fn mem_store() -> (SqliteEventStore, Logs) {
        let logs = Logs::new();
        let store = SqliteEventStore::open_memory(uuid::Uuid::new_v4().to_string(), &logs)
            .await
            .expect("open");
        (store, logs)
    }

    /// A decision as [`EventStore::append_if`] takes one: owned, and handed
    /// its input by value.
    fn decide(
        f: impl FnOnce(Vec<Value>) -> Option<Map<String, Value>> + Send + 'static,
    ) -> Decision {
        Box::new(f)
    }

    #[tokio::test]
    async fn append_assigns_gap_free_monotonic_seq_from_one() {
        let (mut store, _logs) = mem_store().await;
        assert!(store.is_empty().await.expect("is_empty"));
        assert_eq!(store.len().await.expect("len"), 0);

        let a = store.append(ev(1)).await.expect("append e1");
        let b = store.append(ev(2)).await.expect("append e2");
        let c = store.append(ev(3)).await.expect("append e3");

        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
        assert_eq!(store.len().await.expect("len"), 3);
        assert!(!store.is_empty().await.expect("is_empty"));

        // The stamped epoch is what is stored.
        let stored = store.read(0, usize::MAX).await.expect("read");
        let stored_epoch = stored[0]
            .get("epoch_ms")
            .and_then(Value::as_u64)
            .expect("epoch is on the stored event");
        assert_eq!(stored_epoch, a.epoch_ms);
    }

    #[tokio::test]
    async fn a_rejected_append_records_nothing_and_burns_no_seq() {
        let (mut store, _logs) = mem_store().await;
        store
            .append(obj(json!({ "text": "no kind" })))
            .await
            .expect_err("kind is required");
        assert_eq!(store.len().await.expect("len"), 0);
        assert_eq!(store.append(ev(1)).await.expect("append").seq, 1);
    }

    /// The coordinates are the store's: an event that arrives carrying `seq`
    /// or `epoch_ms` has them replaced rather than honoured, so a caller
    /// cannot choose where its event lands or when it says it happened.
    #[tokio::test]
    async fn a_caller_supplied_coordinate_is_overwritten() {
        let (mut store, _logs) = mem_store().await;
        store.append(ev(1)).await.expect("seed");

        let committed = store
            .append(obj(json!({ "kind": "e2", "seq": 99, "epoch_ms": 7 })))
            .await
            .expect("append");
        assert_eq!(committed.seq, 2, "the store numbers the stream");
        assert_ne!(committed.epoch_ms, 7, "the store reads the clock");

        let stored = store.read(0, usize::MAX).await.expect("read");
        assert_eq!(seq_of(&stored[1]), 2);
        assert_eq!(
            stored[1].get("_schema_version").and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION),
            "every append is stamped with the kernel's version"
        );
    }

    /// `append_if` decides on the stream inside its transaction: the events
    /// it is handed are the durable ones, a `Some` lands at the next seq, and
    /// a `None` commits nothing.
    #[tokio::test]
    async fn append_if_decides_inside_the_transaction_and_writes_only_a_some() {
        let (mut store, _logs) = mem_store().await;
        store.append(ev(1)).await.expect("seed");

        // The decision runs on the connection's own thread, so what it saw
        // comes back through a shared cell rather than a borrow.
        let seen_kinds: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = Arc::clone(&seen_kinds);
        let committed = store
            .append_if(
                None,
                decide(move |events| {
                    *recorded.lock().expect("not poisoned") =
                        events.iter().map(|e| kind_of(e).to_string()).collect();
                    Some(ev(2))
                }),
            )
            .await
            .expect("append_if");
        assert_eq!(
            *seen_kinds.lock().expect("not poisoned"),
            ["e1"],
            "decide saw the durable stream"
        );
        assert_eq!(committed.map(|c| c.seq), Some(2));

        let nothing = store
            .append_if(None, decide(|_| None))
            .await
            .expect("append_if");
        assert_eq!(nothing, None);
        assert_eq!(store.len().await.expect("len"), 2, "a None commits nothing");
        assert_eq!(store.append(ev(3)).await.expect("append").seq, 3);
    }

    /// A malformed decision is refused and leaves the stream alone — and the
    /// caller is told, rather than being handed the `None` that would read as
    /// a decision that said no.
    #[tokio::test]
    async fn append_if_validates_the_event_the_decision_returns() {
        let (mut store, _logs) = mem_store().await;
        let err = store
            .append_if(None, decide(|_| Some(obj(json!({ "text": "no kind" })))))
            .await
            .expect_err("kind is required");
        assert_eq!(err.kind(), KnlError::VALIDATION, "{err}");
        assert_eq!(store.len().await.expect("len"), 0);
    }

    /// The kernel's own rules reach a decision's event too: a kernel kind
    /// whose `data` is missing a required field is refused, which eventsdb
    /// (which knows no kind) would have accepted.
    #[tokio::test]
    async fn append_if_holds_a_kernel_kind_to_its_data() {
        let (mut store, _logs) = mem_store().await;
        let err = store
            .append_if(
                None,
                decide(|_| Some(obj(json!({ "kind": "budget_spent", "data": {} })))),
            )
            .await
            .expect_err("a kernel kind needs its data");
        assert_eq!(err.kind(), KnlError::VALIDATION, "{err}");
        assert!(err.reason().contains("amount"), "{}", err.reason());
        assert_eq!(store.len().await.expect("len"), 0);
    }

    /// A batch is one transaction: the events land together, numbered on from
    /// the live head — and a batch that fails part-way leaves the stream
    /// exactly as it was, which is the whole reason it is one call.
    #[tokio::test]
    async fn append_many_is_one_transaction_that_lands_whole_or_not_at_all() {
        let (mut store, _logs) = mem_store().await;
        store.append(ev(1)).await.expect("seed");

        let committed = store
            .append_many(vec![ev(2), ev(3)])
            .await
            .expect("the batch");
        assert_eq!(
            committed.iter().map(|c| c.seq).collect::<Vec<_>>(),
            [2, 3],
            "numbered on from the head that was there"
        );
        let stored = store.read(0, usize::MAX).await.expect("read");
        let kinds: Vec<&str> = stored.iter().map(kind_of).collect();
        assert_eq!(kinds, ["e1", "e2", "e3"]);

        // A malformed event refuses the whole batch, and the one before it in
        // the same call is not in the log either.
        store
            .append_many(vec![ev(4), obj(json!({ "text": "no kind" }))])
            .await
            .expect_err("kind is required");
        assert_eq!(
            store.len().await.expect("len"),
            3,
            "a batch that fails lands nothing"
        );
    }

    /// A two-stream write is one transaction: each side is numbered from its
    /// own head, both land together, and a `None` decision — or a malformed
    /// event on either side — leaves both streams exactly as they were.
    #[tokio::test]
    async fn append_if_many_writes_both_streams_or_neither() {
        let logs = Logs::new();
        let parent = uuid::Uuid::new_v4().to_string();
        let child = uuid::Uuid::new_v4().to_string();
        let log = logs.memory().await.expect("the log");
        let mut ledger = SqliteEventStore::on(Arc::clone(&log), parent.clone());
        let opened = SqliteEventStore::on(Arc::clone(&log), child.clone());
        assert_eq!(
            ledger.database(),
            opened.database(),
            "both streams are in one database"
        );

        ledger
            .append(budget("budget_granted", 100))
            .await
            .expect("the grant");

        // The decision is shown its own stream, filtered, and the other
        // stream's first event — which is nothing, since it is empty.
        let seen: Arc<Mutex<(usize, usize)>> = Arc::default();
        let recorded = Arc::clone(&seen);
        let child_stream = child.clone();
        let committed = ledger
            .append_if_many(
                &child,
                Some(&["budget_granted"]),
                Box::new(move |split: Split<Value>| {
                    *recorded.lock().expect("not poisoned") = (split.own.len(), split.other.len());
                    Some(Split {
                        own: vec![obj(json!({
                            "kind": "budget_reserved",
                            "data": { "amount": 10, "child": child_stream },
                        }))],
                        other: vec![
                            obj(json!({
                                "kind": "session_opened",
                                "data": { "scope_id": "sc-1", "owner": "o", "parent": "p" },
                            })),
                            budget("budget_granted", 10),
                        ],
                    })
                }),
            )
            .await
            .expect("append_if_many")
            .expect("a Some writes");
        assert_eq!(
            *seen.lock().expect("not poisoned"),
            (1, 0),
            "its own kinds, and an empty other stream"
        );
        assert_eq!(committed.own.iter().map(|c| c.seq).collect::<Vec<_>>(), [2]);
        assert_eq!(
            committed.other.iter().map(|c| c.seq).collect::<Vec<_>>(),
            [1, 2],
            "the other stream is numbered from its own head"
        );

        // A None writes nothing at all.
        let nothing = ledger
            .append_if_many(&child, None, Box::new(|_| None))
            .await
            .expect("append_if_many");
        assert_eq!(nothing, None);
        assert_eq!(ledger.len().await.expect("len"), 2);
        assert_eq!(opened.len().await.expect("len"), 2);

        // A malformed event on the far side leaves both streams as they were.
        let err = ledger
            .append_if_many(
                &child,
                None,
                Box::new(|_| {
                    Some(Split {
                        own: vec![budget("budget_spent", 1)],
                        other: vec![obj(json!({ "text": "no kind" }))],
                    })
                }),
            )
            .await
            .expect_err("kind is required");
        assert_eq!(err.kind(), KnlError::VALIDATION, "{err}");
        assert_eq!(ledger.len().await.expect("len"), 2);
        assert_eq!(opened.len().await.expect("len"), 2);
    }

    /// The close-time child scan reads by the parent index instead of walking
    /// every event in the database.
    ///
    /// The plan is the assertion because the alternative is silent: a bound
    /// `kind` proves nothing about the index's `WHERE kind = 'session_opened'`
    /// and a bound path never matches an indexed expression, so getting either
    /// wrong still answers correctly — it just answers by reading the whole
    /// table, on the one query that is not scoped to a stream.
    #[tokio::test]
    async fn the_child_scan_reads_by_the_parent_index() {
        let logs = Logs::new();
        let log = logs.memory().await.expect("the log");
        let scan = ChildScan {
            opened: "session_opened".to_string(),
            closed: "session_closed".to_string(),
            parent_field: "parent".to_string(),
        };
        let rows = log
            .query(
                &format!("EXPLAIN QUERY PLAN {}", child_scan_sql(&scan)),
                vec![Value::from("p-1")],
            )
            .await
            .expect("the plan");
        let plan: String = rows
            .iter()
            .filter_map(|row| row.get("detail").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            plan.contains("events_session_opened_parent"),
            "the scan must read by the parent index: {plan}"
        );
    }

    /// The scan's words go into the statement as literals, so a quote in one
    /// of them is doubled rather than closing the string early.
    ///
    /// They are the kernel's own constants today, which is why writing them
    /// in is safe *and* why nothing would notice if they stopped being: the
    /// vocabulary is an argument ([`ChildScan`]), and a word that ended the
    /// literal would turn the rest of the statement into SQL somebody else
    /// wrote.
    #[test]
    fn a_word_written_into_the_scan_stays_one_word() {
        let sql = child_scan_sql(&ChildScan {
            opened: "it's opened".to_string(),
            closed: "it's closed".to_string(),
            parent_field: "it's parent".to_string(),
        });
        assert!(sql.contains("'it''s opened'"), "{sql}");
        assert!(sql.contains("'it''s closed'"), "{sql}");
        assert!(sql.contains("'$.it''s parent'"), "{sql}");
        assert_eq!(
            sql.matches('\'').count() % 2,
            0,
            "every literal is closed: {sql}"
        );
    }

    /// `database` names the database, not the stream: two stores on one file
    /// answer with the same string and a store on another file does not.
    /// That is the whole of what the identity is for — deciding whether one
    /// transaction can cover both.
    #[tokio::test]
    async fn database_is_the_same_for_two_streams_of_one_database() {
        let logs = Logs::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let here = dir.path().join("knl.db");
        let there = dir.path().join("other.db");

        let a = SqliteEventStore::open(&here, "s-1", &logs)
            .await
            .expect("open a");
        let b = SqliteEventStore::open(&here, "s-2", &logs)
            .await
            .expect("open b");
        let elsewhere = SqliteEventStore::open(&there, "s-1", &logs)
            .await
            .expect("open elsewhere");

        assert_eq!(a.database(), b.database());
        assert_ne!(a.database(), elsewhere.database());

        // The in-memory log is a database of its own, and says so.
        let (mem, _mem_logs) = mem_store().await;
        assert_ne!(mem.database(), a.database());
        assert!(mem.database().is_some());
    }

    /// The child scan finds the streams that name this one as their parent
    /// and carry no ending — and nobody else's children, and not the ones
    /// that already closed.
    #[tokio::test]
    async fn open_children_are_the_unended_streams_that_name_this_one() {
        let logs = Logs::new();
        let log = logs.memory().await.expect("the log");
        let scan = ChildScan {
            opened: "session_opened".to_string(),
            closed: "session_closed".to_string(),
            parent_field: "parent".to_string(),
        };

        /// A stream that opened, naming `parent`.
        async fn opened(log: &Arc<SqliteEventLog>, id: &str, parent: &str) -> SqliteEventStore {
            let mut store = SqliteEventStore::on(Arc::clone(log), id);
            store
                .append(obj(json!({
                    "kind": "session_opened",
                    "data": { "scope_id": "sc", "owner": "o", "parent": parent },
                })))
                .await
                .expect("the opening");
            store
        }

        let mut parent = SqliteEventStore::on(Arc::clone(&log), "parent");
        let _open_child = opened(&log, "child-open", "parent").await;
        let mut ended = opened(&log, "child-ended", "parent").await;
        let _elsewhere = opened(&log, "child-of-other", "another").await;
        ended
            .append(obj(json!({
                "kind": "session_closed",
                "data": { "reason": "done" },
            })))
            .await
            .expect("the ending");

        let recorded: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&recorded);
        let committed = parent
            .append_with_open_children(
                &scan,
                Box::new(move |children| {
                    *seen.lock().expect("not poisoned") = children.clone();
                    obj(json!({
                        "kind": "session_closed",
                        "data": { "reason": "done", "open_children": children },
                    }))
                }),
            )
            .await
            .expect("the close");
        assert_eq!(committed.seq, 1, "the close is the parent's first event");
        assert_eq!(
            *recorded.lock().expect("not poisoned"),
            ["child-open"],
            "only this stream's children, and only the open ones"
        );
    }

    /// A kind-filtered read is answered off the index: only the kinds asked
    /// for come back, in `seq` order, still carrying the `seq` the stream gave
    /// them.  `None` is the whole stream, an empty selection is nothing.
    #[tokio::test]
    async fn read_kinds_selects_by_kind_and_keeps_the_streams_order() {
        let (mut store, _logs) = mem_store().await;
        store
            .append(budget("budget_granted", 100))
            .await
            .expect("grant");
        store.append(ev(1)).await.expect("noise");
        store
            .append(budget("budget_spent", 10))
            .await
            .expect("spend");

        let all = store.read(0, usize::MAX).await.expect("read");
        assert_eq!(
            all.iter().map(kind_of).collect::<Vec<_>>(),
            ["budget_granted", "e1", "budget_spent"]
        );

        let ledger = store
            .read_kinds(Some(&["budget_granted", "budget_spent"]), 0, usize::MAX)
            .await
            .expect("read_kinds");
        assert_eq!(
            ledger.iter().map(seq_of).collect::<Vec<_>>(),
            [1, 3],
            "the seq the stream gave them, not a fresh numbering"
        );

        let nothing = store
            .read_kinds(Some(&[]), 0, usize::MAX)
            .await
            .expect("read_kinds");
        assert!(nothing.is_empty(), "an empty selection selects nothing");
    }

    /// A decision that names its kinds is shown those and nothing else, and
    /// its write is still numbered against the whole stream — the filter is
    /// what the decision *reads*, not where its answer goes.
    #[tokio::test]
    async fn append_if_filters_the_decisions_input_and_numbers_against_the_stream() {
        let (mut store, _logs) = mem_store().await;
        store
            .append(budget("budget_granted", 100))
            .await
            .expect("grant");
        store.append(ev(1)).await.expect("noise");

        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = Arc::clone(&seen);
        let committed = store
            .append_if(
                Some(&["budget_granted", "budget_spent"]),
                decide(move |events| {
                    *recorded.lock().expect("not poisoned") =
                        events.iter().map(|e| kind_of(e).to_string()).collect();
                    Some(budget("budget_spent", 10))
                }),
            )
            .await
            .expect("append_if");
        assert_eq!(
            *seen.lock().expect("not poisoned"),
            ["budget_granted"],
            "only the kinds asked for"
        );
        assert_eq!(
            committed.map(|c| c.seq),
            Some(3),
            "numbered against the whole stream"
        );
    }

    /// Two handles on one stream, one invariant: each decides inside its own
    /// transaction, so the second sees what the first wrote and exactly one
    /// of them may write.
    #[tokio::test]
    async fn append_if_across_two_handles_decides_on_the_other_handles_write() {
        let logs = Logs::new();
        let log = logs.memory().await.expect("the log");
        let stream = uuid::Uuid::new_v4().to_string();
        let mut a = SqliteEventStore::on(Arc::clone(&log), stream.clone());
        let mut b = SqliteEventStore::on(Arc::clone(&log), stream);

        // "Write the claim, but only if nobody has."
        fn claim() -> Decision {
            decide(|events: Vec<Value>| {
                if events.is_empty() {
                    Some(obj(json!({ "kind": "claim" })))
                } else {
                    None
                }
            })
        }
        assert!(a
            .append_if(Some(&["claim"]), claim())
            .await
            .expect("a")
            .is_some());
        assert!(
            b.append_if(Some(&["claim"]), claim())
                .await
                .expect("b")
                .is_none(),
            "the second handle decided against what the first wrote"
        );
        assert_eq!(a.len().await.expect("len"), 1);
    }

    #[tokio::test]
    async fn read_pages_by_from_seq_and_limit() {
        let (mut store, _logs) = mem_store().await;
        for i in 1..=5 {
            store.append(ev(i)).await.expect("append");
        }
        let page = store.read(2, 2).await.expect("read");
        assert_eq!(page.iter().map(seq_of).collect::<Vec<_>>(), [2, 3]);
        let rest = store.read(4, usize::MAX).await.expect("read");
        assert_eq!(rest.iter().map(seq_of).collect::<Vec<_>>(), [4, 5]);
        assert!(store.read(6, 10).await.expect("read").is_empty());
    }

    /// The last `n` come back in `seq` order.
    #[tokio::test]
    async fn read_last_takes_the_end_of_the_stream_in_seq_order() {
        let (mut store, _logs) = mem_store().await;
        for i in 1..=5 {
            store.append(ev(i)).await.expect("append");
        }
        let tail = store.read_last(2).await.expect("read_last");
        assert_eq!(tail.iter().map(seq_of).collect::<Vec<_>>(), [4, 5]);
        assert!(store.read_last(0).await.expect("read_last").is_empty());
        assert_eq!(store.read_last(50).await.expect("read_last").len(), 5);
    }

    #[tokio::test]
    async fn head_is_none_when_empty_then_tracks_the_max() {
        let (mut store, _logs) = mem_store().await;
        assert_eq!(store.head().await.expect("head"), None);
        store.append(ev(1)).await.expect("append");
        assert_eq!(store.head().await.expect("head"), Some(1));
        store.append(ev(2)).await.expect("append");
        assert_eq!(store.head().await.expect("head"), Some(2));
    }

    /// A read rebuilds the object that was written: the envelope out of its
    /// columns, `meta` and `data` out of theirs, and the beat inside the
    /// `meta` it was written in.
    #[tokio::test]
    async fn read_reconstructs_the_written_event_out_of_its_columns() {
        let (mut store, _logs) = mem_store().await;
        let committed = store
            .append(obj(json!({
                "kind": "llm_response",
                "meta": { "beat": "b-1", "attempt": 2, "final": true },
                "data": { "content": { "text": "hi" }, "usage": { "input_tokens": 3 } },
            })))
            .await
            .expect("append");

        let stored = store.read(0, usize::MAX).await.expect("read");
        let event = &stored[0];
        assert_eq!(kind_of(event), "llm_response");
        assert_eq!(seq_of(event), committed.seq);
        assert_eq!(
            event.get(FIELD_META),
            Some(&json!({ "beat": "b-1", "attempt": 2, "final": true }))
        );
        assert_eq!(
            event.get(FIELD_DATA),
            Some(&json!({ "content": { "text": "hi" }, "usage": { "input_tokens": 3 } }))
        );
        assert_eq!(
            event.get("_schema_version").and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION)
        );
    }

    /// The beat is a label of `meta` and a read reaches it there — and the
    /// log carries an index on exactly that expression, so a by-beat read is
    /// a range rather than a scan.
    #[tokio::test]
    async fn the_beat_is_a_meta_label_with_an_index() {
        let (mut store, _logs) = mem_store().await;
        store
            .append(obj(json!({ "kind": "e1", "meta": { "beat": "b-1" } })))
            .await
            .expect("append");
        store.append(ev(2)).await.expect("append");

        let rows = ask(
            &store,
            "SELECT json_extract(meta, '$.beat') AS beat FROM events \
              WHERE stream = $stream ORDER BY seq",
        )
        .await
        .expect("query");
        assert_eq!(rows.rows[0].get("beat"), Some(&json!("b-1")));
        assert!(
            !rows.rows[1].contains_key("beat"),
            "an undeclared beat reads as nil: {:?}",
            rows.rows[1]
        );

        let indexes = ask(
            &store,
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'events'",
        )
        .await
        .expect("query");
        let names: Vec<&str> = indexes
            .rows
            .iter()
            .filter_map(|row| row.get("name").and_then(Value::as_str))
            .collect();
        assert!(
            names.contains(&"events_meta_beat"),
            "the beat label is indexed: {names:?}"
        );
        assert!(
            !names.contains(&"events_stream_beat_seq"),
            "and the column's old index is gone: {names:?}"
        );
    }

    #[tokio::test]
    async fn events_persist_across_a_reopen_of_the_same_path_and_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let stream = "s-1";

        {
            let logs = Logs::new();
            let mut store = SqliteEventStore::open(&path, stream, &logs)
                .await
                .expect("open");
            store.append(ev(1)).await.expect("append");
            store.append(ev(2)).await.expect("append");
            assert!(logs.shutdown().await.is_empty(), "the log closed cleanly");
        }

        let logs = Logs::new();
        let reopened = SqliteEventStore::open(&path, stream, &logs)
            .await
            .expect("reopen");
        let stored = reopened.read(0, usize::MAX).await.expect("read");
        assert_eq!(stored.iter().map(kind_of).collect::<Vec<_>>(), ["e1", "e2"]);
        assert_eq!(reopened.head().await.expect("head"), Some(2));
    }

    #[tokio::test]
    async fn two_streams_in_one_db_file_do_not_see_each_others_events() {
        let logs = Logs::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");

        let mut a = SqliteEventStore::open(&path, "s-a", &logs)
            .await
            .expect("open a");
        let mut b = SqliteEventStore::open(&path, "s-b", &logs)
            .await
            .expect("open b");
        a.append(ev(1)).await.expect("a");
        b.append(ev(2)).await.expect("b");

        assert_eq!(
            a.read(0, usize::MAX)
                .await
                .expect("read a")
                .iter()
                .map(kind_of)
                .collect::<Vec<_>>(),
            ["e1"]
        );
        assert_eq!(
            b.read(0, usize::MAX)
                .await
                .expect("read b")
                .iter()
                .map(kind_of)
                .collect::<Vec<_>>(),
            ["e2"]
        );
        assert_eq!(a.head().await.expect("head"), Some(1));
        assert_eq!(b.head().await.expect("head"), Some(1));
    }

    /// A row whose stored objects will not decode is corruption: a read
    /// surfaces it as an error rather than silently dropping the row (which
    /// would let a resume re-fold a truncated log into the wrong state).
    ///
    /// The bad row is written from outside the store, which is the only place
    /// it can come from: the hatch's authorizer refuses a raw `INSERT` into
    /// `events`, and the schema's own trigger refuses an `UPDATE` to every
    /// connection there is.
    #[tokio::test]
    async fn read_errors_on_a_corrupt_row_instead_of_dropping_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");

        {
            let logs = Logs::new();
            let mut store = SqliteEventStore::open(&path, "s-1", &logs)
                .await
                .expect("open");
            store.append(ev(1)).await.expect("append");
            assert!(logs.shutdown().await.is_empty(), "the log closed cleanly");
        }

        let conn = rusqlite::Connection::open(&path).expect("open the file");
        conn.execute(
            "INSERT INTO events (stream, seq, epoch_ms, kind, schema_version, meta, data) \
             VALUES ('s-1', 2, 0, 'e2', 2, '{}', 'not json')",
            [],
        )
        .expect("write a bad row");
        drop(conn);

        let logs = Logs::new();
        let store = SqliteEventStore::open(&path, "s-1", &logs)
            .await
            .expect("reopen");
        let err = store
            .read(0, usize::MAX)
            .await
            .expect_err("a row that will not decode must surface");
        assert_eq!(err.kind(), KnlError::CORRUPTION, "{err}");
    }

    /// The backend's error language is translated in exactly one place, and
    /// the split is the one a caller can act on.
    #[test]
    fn every_store_error_has_a_kernel_class() {
        use eventsdb_core::Error as Failure;
        let cases = [
            (Failure::Validation("v".into()), KnlError::VALIDATION),
            (Failure::Busy("b".into()), KnlError::BUSY),
            (Failure::Timeout("t".into()), KnlError::TIMEOUT),
            (Failure::Storage("s".into()), KnlError::STORAGE),
            (Failure::Corruption("c".into()), KnlError::CORRUPTION),
            (Failure::Unsupported("u".into()), KnlError::UNSUPPORTED),
            // Not a call this kernel makes, so it is the store failing to do
            // the work — and it has to land somewhere, since the enum is
            // `#[non_exhaustive]`.
            (
                Failure::Truncated {
                    requested: 1,
                    removed_up_to: 2,
                },
                KnlError::STORAGE,
            ),
        ];
        for (failure, expected) in cases {
            let translated = KnlError::from(failure);
            assert_eq!(translated.kind(), expected, "{translated}");
        }
        assert!(
            !KnlError::from(Failure::Timeout("t".into())).is_retryable(),
            "a deadline is not contention"
        );
        assert!(KnlError::from(Failure::Busy("b".into())).is_retryable());
    }

    /// A contended lock is what a real contended write surfaces as: a second
    /// connection holds the write lock, so the retries are exhausted and the
    /// error the caller gets says "ask again".
    ///
    /// The log is opened by hand with a short busy timeout, because the point
    /// is the *class* of the failure and the default timeout would spend five
    /// seconds per attempt reaching it.
    #[tokio::test]
    async fn a_write_that_stays_contended_surfaces_as_busy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let log = SqliteEventLog::open_with(
            &path,
            eventsdb_sqlite::OpenOptions::default()
                .busy_timeout(std::time::Duration::from_millis(50))
                .upcasters(crate::knl::kernel_upcasters()),
        )
        .await
        .expect("open");
        let mut store = SqliteEventStore::on(Arc::new(log), "s-1");
        store.append(ev(1)).await.expect("the first append");

        // A second connection takes the write lock and keeps it.
        let blocker = rusqlite::Connection::open(&path).expect("open the file");
        blocker
            .execute_batch("BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS held (x)")
            .expect("hold the lock");

        let err = store
            .append(ev(2))
            .await
            .expect_err("a write that stays contended must surface");
        assert_eq!(err.kind(), KnlError::BUSY, "{err}");
        assert!(err.is_retryable(), "busy is the class that says ask again");

        blocker.execute_batch("ROLLBACK").expect("release");
        store.append(ev(3)).await.expect("the lock is free again");
    }

    /// Two handles on one stream both write: an append records a fact, so it
    /// is serialized and assigned the next seq rather than refused for the
    /// head one of them last saw.
    #[tokio::test]
    async fn two_handles_on_one_stream_both_append_in_arrival_order() {
        let logs = Logs::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        let mut a = SqliteEventStore::open(&path, "s-1", &logs)
            .await
            .expect("open a");
        let mut b = SqliteEventStore::open(&path, "s-1", &logs)
            .await
            .expect("open b");

        assert_eq!(a.append(ev(1)).await.expect("a").seq, 1);
        assert_eq!(b.append(ev(2)).await.expect("b").seq, 2);
        assert_eq!(a.append(ev(3)).await.expect("a").seq, 3);

        let stored = b.read(0, usize::MAX).await.expect("read");
        assert_eq!(
            stored.iter().map(kind_of).collect::<Vec<_>>(),
            ["e1", "e2", "e3"]
        );
    }

    // -- the read side ------------------------------------------------------

    /// Ask `store` for `sql` with everything default.
    async fn ask(store: &SqliteEventStore, sql: &str) -> KnlResult<QueryRows> {
        ask_with(store, sql, QueryParams::None, &QueryOpts::default()).await
    }

    /// Ask `store` for `sql`, saying how.
    async fn ask_with(
        store: &SqliteEventStore,
        sql: &str,
        params: QueryParams,
        opts: &QueryOpts,
    ) -> KnlResult<QueryRows> {
        let plan = query::plan(sql, params, opts, &store.stream)?;
        store.query(&plan).await
    }

    /// The `kind` column of every row, in order.
    fn kinds_of(rows: &QueryRows) -> Vec<&str> {
        rows.rows
            .iter()
            .filter_map(|row| row.get("kind").and_then(Value::as_str))
            .collect()
    }

    /// A query reads what the writer wrote — on the in-memory log as much as
    /// on a file.
    #[tokio::test]
    async fn a_query_reads_what_the_writer_wrote() {
        let (mut store, _logs) = mem_store().await;
        store.append(ev(1)).await.expect("append");
        store.append(ev(2)).await.expect("append");

        let rows = ask(
            &store,
            "SELECT kind, seq FROM events WHERE stream = $stream ORDER BY seq",
        )
        .await
        .expect("query");
        assert_eq!(kinds_of(&rows), ["e1", "e2"]);
        assert!(!rows.truncated);
        assert_eq!(rows.rows[0].get("seq"), Some(&json!(1)));
    }

    /// `$stream` is this store's own stream and nothing else: a second stream
    /// in the same database is not selected by it.
    #[tokio::test]
    async fn stream_binds_to_this_stores_own_stream() {
        let logs = Logs::new();
        let log = logs.memory().await.expect("the log");
        let mut mine = SqliteEventStore::on(Arc::clone(&log), "s-mine");
        let mut theirs = SqliteEventStore::on(Arc::clone(&log), "s-theirs");
        mine.append(ev(1)).await.expect("mine");
        theirs.append(ev(2)).await.expect("theirs");

        let rows = ask(&mine, "SELECT kind FROM events WHERE stream = $stream")
            .await
            .expect("query");
        assert_eq!(kinds_of(&rows), ["e1"]);
    }

    /// `$sessions` reads across a set: two streams in one database, one
    /// statement, and the ids are bound rather than pasted in.
    #[tokio::test]
    async fn sessions_reads_across_the_set_it_was_given() {
        let logs = Logs::new();
        let log = logs.memory().await.expect("the log");
        let mut one = SqliteEventStore::on(Arc::clone(&log), "s-one");
        let mut two = SqliteEventStore::on(Arc::clone(&log), "s-two");
        let mut three = SqliteEventStore::on(Arc::clone(&log), "s-three");
        one.append(ev(1)).await.expect("one");
        two.append(ev(2)).await.expect("two");
        three.append(ev(3)).await.expect("three");

        let opts = QueryOpts {
            sessions: Some(vec!["s-one".to_string(), "s-two".to_string()]),
            ..QueryOpts::default()
        };
        let rows = ask_with(
            &one,
            "SELECT kind FROM events WHERE stream IN $sessions ORDER BY position",
            QueryParams::None,
            &opts,
        )
        .await
        .expect("query");
        assert_eq!(kinds_of(&rows), ["e1", "e2"]);
    }

    /// A value is bound, never pasted: a quote inside it is a character in a
    /// string, not the end of one.
    #[tokio::test]
    async fn a_bound_value_with_a_quote_in_it_is_a_value() {
        let (mut store, _logs) = mem_store().await;
        store
            .append(obj(json!({ "kind": "it's fine" })))
            .await
            .expect("append");

        let rows = ask_with(
            &store,
            "SELECT kind FROM events WHERE stream = $stream AND kind = :kind",
            QueryParams::Named(obj(json!({ "kind": "it's fine" }))),
            &QueryOpts::default(),
        )
        .await
        .expect("query");
        assert_eq!(kinds_of(&rows), ["it's fine"]);
    }

    /// The cap is reported, not silently applied — and a result that happens
    /// to be exactly `limit` long is not called truncated.
    #[tokio::test]
    async fn the_row_cap_is_reported_when_it_cuts() {
        let (mut store, _logs) = mem_store().await;
        for i in 1..=5 {
            store.append(ev(i)).await.expect("append");
        }

        let capped = QueryOpts {
            limit: 2,
            ..QueryOpts::default()
        };
        let rows = ask_with(
            &store,
            "SELECT kind FROM events WHERE stream = $stream ORDER BY seq",
            QueryParams::None,
            &capped,
        )
        .await
        .expect("query");
        assert_eq!(kinds_of(&rows), ["e1", "e2"]);
        assert!(rows.truncated, "the answer was cut");

        let exact = QueryOpts {
            limit: 5,
            ..QueryOpts::default()
        };
        let rows = ask_with(
            &store,
            "SELECT kind FROM events WHERE stream = $stream ORDER BY seq",
            QueryParams::None,
            &exact,
        )
        .await
        .expect("query");
        assert_eq!(rows.rows.len(), 5);
        assert!(!rows.truncated, "nothing was cut off");
    }

    /// A query that will not finish is cut short, and says so in its own
    /// class: nothing was contended, so "ask again" would be the wrong advice.
    #[tokio::test]
    async fn a_query_that_runs_too_long_is_a_timeout() {
        let (store, _logs) = mem_store().await;
        let hurried = QueryOpts {
            timeout_ms: 50,
            ..QueryOpts::default()
        };
        let err = ask_with(
            &store,
            // Unbounded on purpose: it ends when the deadline ends it.
            "WITH RECURSIVE forever(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM forever) \
             SELECT COUNT(*) FROM forever",
            QueryParams::None,
            &hurried,
        )
        .await
        .expect_err("an endless query must be cut short");
        assert_eq!(err.kind(), KnlError::TIMEOUT, "{err}");
        assert!(!err.is_retryable(), "a slow query is not a retry: {err}");

        // The connection is usable afterwards: the interrupt ended a
        // statement, not the reader.
        assert!(ask(&store, "SELECT 1 AS one").await.is_ok());
    }

    /// A statement that is not a read never reaches the connection, and a
    /// second statement is refused whole.  (The rules are
    /// [`super::super::query`]'s; this is the path through the store.)
    #[tokio::test]
    async fn a_write_or_a_second_statement_is_refused_before_the_connection() {
        let (store, _logs) = mem_store().await;
        for sql in [
            "INSERT INTO events (stream) VALUES ('x')",
            "UPDATE events SET kind = 'x'",
            "PRAGMA table_info(events)",
            "ATTACH DATABASE '/tmp/other.db' AS other",
            "SELECT 1; DROP TABLE events",
        ] {
            let err = ask(&store, sql).await.expect_err("must be refused");
            assert_eq!(err.kind(), KnlError::VALIDATION, "{sql:?}: {err}");
        }
    }

    /// Every SQLite type comes back as itself, and a NULL comes back as an
    /// absent column rather than a present nothing.
    #[tokio::test]
    async fn the_sqlite_types_map_onto_values_and_null_is_absence() {
        let (store, _logs) = mem_store().await;
        let rows = ask(
            &store,
            // `absent`, not `nothing`: NOTHING is a SQLite keyword.
            "SELECT 1 AS whole, 1.5 AS fraction, 'text' AS words, NULL AS absent, \
             CAST('bytes' AS BLOB) AS raw",
        )
        .await
        .expect("query");
        let row = &rows.rows[0];
        assert_eq!(row["whole"], Value::from(1));
        assert_eq!(row["fraction"], Value::from(1.5));
        assert_eq!(row["words"], Value::from("text"));
        assert_eq!(
            row["raw"],
            Value::from("<blob>"),
            "a blob has no value on the other side of the bridge, and says so"
        );
        assert!(
            !row.contains_key("absent"),
            "a NULL column is absent, so it reads as nil: {row:?}"
        );
    }

    /// The published schema is the table: the constant `knl.api()` hands out,
    /// held against the columns SQLite actually reports.
    #[tokio::test]
    async fn the_published_schema_is_the_events_table() {
        let columns = events_schema().expect("schema");
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "position",
                "stream",
                "seq",
                "epoch_ms",
                "kind",
                "schema_version",
                "meta",
                "data"
            ]
        );

        let pk: Vec<&str> = columns
            .iter()
            .filter(|c| c.pk)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(pk, ["position"], "the log is keyed by its global order");

        // And a query may name every one of them.
        let (store, _logs) = mem_store().await;
        let sql = format!("SELECT {} FROM {EVENTS_TABLE}", names.join(", "));
        ask(&store, &sql)
            .await
            .expect("the published columns are the real ones");
    }

    /// The published schema is also the *live* one.
    ///
    /// This is what keeps [`events_schema`] a reading of the table rather than
    /// a claim about it: a real log is opened, closed, and asked what its
    /// `events` table has — with `PRAGMA table_info` on a plain connection,
    /// because a pragma is one of the things the store's own hatch refuses,
    /// and rightly (setting one is how the migration ladder's marker or the
    /// journal mode would be changed underneath it).
    #[tokio::test]
    async fn the_published_schema_is_the_live_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.db");
        {
            let logs = Logs::new();
            SqliteEventStore::open(&path, "s-1", &logs)
                .await
                .expect("open");
            assert!(logs.shutdown().await.is_empty(), "the log closed cleanly");
        }

        let conn = rusqlite::Connection::open(&path).expect("open the file");
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({EVENTS_TABLE})"))
            .expect("table_info");
        let live: Vec<SchemaColumn> = stmt
            .query_map([], |row| {
                Ok(SchemaColumn {
                    name: row.get::<_, String>("name")?,
                    declared_type: row.get::<_, String>("type")?,
                    pk: row.get::<_, i64>("pk")? > 0,
                })
            })
            .expect("read the columns")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read the columns");

        assert_eq!(live, events_schema().expect("published schema"));
    }
}
