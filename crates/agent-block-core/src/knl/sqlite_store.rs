//! The durable [`EventStore`]: one SQLite table, one stream per session.
//!
//! [`SqliteEventStore`] takes the same calls [`MemEventStore`] does, so a
//! session's log survives a process restart without any other code changing.
//! It is scoped to one `stream` (the session id); several sessions share one
//! DB file, and the `(stream, seq)` primary key keeps their logs apart.
//!
//! # Append-only, store-assigned coordinates
//!
//! There is no update or delete — the trait has neither, so a backend cannot
//! offer one.  `seq` and `epoch_ms` are the store's to assign: `append`
//! computes the next `seq` inside the transaction that inserts, runs the
//! same [`validate_event`] and [`stamp`] the in-memory store runs, and
//! returns the coordinates inline.
//!
//! # One backend, two kinds of database
//!
//! There is no second implementation of [`EventStore`] in the product: a
//! session's log is a SQLite table whether or not it outlives the process.
//! [`SqliteEventStore::open`] takes a file; [`SqliteEventStore::open_memory`]
//! takes a database that lives in memory under a name derived from the
//! stream (`file:knl-<stream>?mode=memory&cache=shared`), which is what an
//! ephemeral session gets.  The shared-cache URI is not decoration: a second
//! connection to the same name sees the same database, which is what lets the
//! read side below exist at all — and it is also why the writer connection
//! must outlive the session, since an in-memory database is reclaimed when
//! its last connection closes.
//!
//! The one thing the in-memory database cannot do is survive the process.
//! Within it, a stream is a stream: [`super::Session::resume`] reopens one by
//! name exactly as it reopens a file.
//!
//! # The stored shape is columns, and one of them is the kind's own
//!
//! The event's envelope is columns — `stream` / `seq` / `epoch_ms` / `kind` /
//! `schema_version` / `beat` — and the two objects it carries are one column
//! each: `meta`, a shallow table of scalars, and `data`, the kind's own
//! content at any depth ([`super::event`]).  A read rebuilds exactly the
//! object that was written, so a caller sees no difference between this and a
//! log kept in memory.
//!
//! The whole event used to go into a single `payload` column, which put an
//! envelope key and a kind's own field at the same level for anything reading
//! the log with SQL: a `json_extract` could not say which of the two it was
//! reaching into, and a kind changing shape broke a view with nothing to
//! point at.  Now the columns *are* the contract — a view over them is
//! unaffected by any kind — and the paths that need watching are all inside
//! `data`.
//!
//! # Reads are indexed by kind, and by beat
//!
//! The table carries a `(stream, kind, seq)` index beside its `(stream, seq)`
//! primary key, so a kind-filtered read ([`EventStore::read_kinds`], and the
//! decision input of [`EventStore::append_if`]) costs the size of the *fold*
//! rather than the size of the stream: folding the balance reads the
//! `budget_*` events, not every fact the session ever recorded.
//!
//! `beat` has a column and a `(stream, beat, seq)` index of its own, because
//! it is the one correlation the log itself is grouped by: the events of one
//! beat are a range of that index rather than a scan with a `json_extract`
//! in the predicate.
//!
//! # The read side is a second connection, and it cannot write
//!
//! [`EventStore::query`] answers a caller's own SQL ([`super::query`]) over a
//! **separate** connection to the same database, opened `READ_ONLY` and put
//! into `query_only` mode, lazily on the first query and reused after that.
//! Three independent things therefore have to fail before a query could
//! change the log: the statement is checked to be a single `SELECT` / `WITH`
//! before SQLite sees it, the prepared statement is asked whether it writes,
//! and the connection it runs on has no write capability to lend it.  Values
//! are bound, never interpolated — including the ids `$sessions` expands to.
//!
//! A query runs under a deadline: [`AsyncIsle::call_timeout`] interrupts the
//! statement if it has not finished in time, and that surfaces as
//! [`KnlError::Timeout`].
//!
//! # The connection lives on a thread of its own, and nobody waits on it
//!
//! Neither connection is held by this struct: each is owned by a
//! [`rusqlite_isle::AsyncIsle`], a thread that takes closures and runs them
//! one at a time.  A store method is therefore a closure sent to that thread
//! and a result **awaited** — the caller's task yields while SQLite works, so
//! the one thread that must never stop (the Lua VM's, which is the sole worker
//! of its own runtime) goes on driving every other coroutine, timer and cancel
//! it owns.  That is why the whole SPI below is `async`: an event store that
//! can only be waited for synchronously is an event store that stops the VM.
//!
//! The handle is cloneable and cheap; what is *not* cloneable is the
//! [`rusqlite_isle::AsyncIsleDriver`] that owns the thread's join handle.
//! Those go to an [`IsleDrivers`] the host holds, so a session's threads
//! outlive the session — a dropped handle can still hand its closing event to
//! the isle without waiting for it — and are drained once, at host shutdown.
//!
//! # Concurrency
//!
//! `append`, `append_many` and `append_if` read-then-write, so each runs in an
//! `IMMEDIATE` transaction: the `RESERVED` lock is taken at `BEGIN` rather
//! than promoted from `SHARED` on the first write, which is the point
//! `busy_timeout` actually covers — a `DEFERRED` transaction can still hit
//! `SQLITE_BUSY` on lock *promotion* even with a timeout set.  Contention with
//! another connection is waited out by the busy timeout the isle was opened
//! with, and `append` / `append_many` sit inside [`AsyncIsle::call_retry`],
//! which re-submits the whole job on `SQLITE_BUSY`, backing off with
//! `tokio::time::sleep` rather than parking a thread.  A write that is still
//! contended after that surfaces as [`KnlError::Busy`], which is the one class
//! that tells the caller another try is worth making.
//!
//! `append_if` gets the busy timeout and the retryable error, not the backoff
//! loop — its decision is a `FnOnce`, so an attempt consumes it.  What it no
//! longer needs is the channel round trip the borrowed-closure form required:
//! the decision is owned and `Send`, so it travels *with* the job and runs on
//! the isle's own thread, inside the transaction, with nothing on either side
//! waiting for the other.
//!
//! That is what makes the SPI's promise true here: appends to one stream are
//! *serialized* — two handles both write and the log interleaves in arrival
//! order — a batch is one transaction, so it lands whole or not at all, and a
//! decision taken by `append_if` runs against the stream inside the same
//! transaction that records its answer, so no concurrent writer can slip
//! between the two.
//!
//! [`MemEventStore`]: super::event_store::MemEventStore
//! [`AsyncIsle::call_retry`]: rusqlite_isle::AsyncIsle::call_retry
//! [`AsyncIsle::call_timeout`]: rusqlite_isle::AsyncIsle::call_timeout

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{params, params_from_iter, Connection, OpenFlags, TransactionBehavior};
use rusqlite_isle::{AsyncIsle, AsyncIsleDriver, IsleError, RetryPolicy};
use serde_json::{Map, Value};
use tokio::sync::OnceCell;

use super::event::{
    stamp, validate_event, FIELD_BEAT, FIELD_DATA, FIELD_EPOCH_MS, FIELD_KIND, FIELD_META,
    FIELD_SEQ,
};
use super::event_store::{
    stamp_schema_version, ChildScan, ChildrenDecision, Committed, Decision, EventStore, Split,
    SplitDecision, CURRENT_SCHEMA_VERSION, SCHEMA_VERSION_FIELD,
};
use super::query::{session_slot, QueryParams, QueryPlan, QueryRows, STREAM_PARAM};
use super::{now_ms, KnlError, KnlResult};

/// How long a contended write waits for the lock before erroring.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The lifecycle owners of the connection threads a session's log lives on.
///
/// [`rusqlite_isle::AsyncIsle`] hands back a cloneable handle and a driver
/// that is not clonable: the driver owns the thread's join handle and is the
/// only thing that can drain and stop it.  A session cannot hold its own —
/// the whole point of the drop backstop is that a handle nobody closed can
/// still hand its `session_closed` to the isle *after* the handle is gone, and
/// a thread its own store had already stopped could not take it.
///
/// So the drivers are parked here instead: one collection per host run, shut
/// down once at the end of it, exactly as the `std.ts` connection thread is.
/// Cheap to clone (an `Arc`), because every site that opens a store needs to
/// reach it.
///
/// The lock is a plain [`Mutex`] and is never held across an `.await`:
/// [`IsleDrivers::shutdown`] takes the whole list out under the lock and
/// releases it before it starts waiting on the first thread.
#[derive(Clone, Default)]
pub struct IsleDrivers {
    parked: Arc<Mutex<Vec<AsyncIsleDriver>>>,
}

impl std::fmt::Debug for IsleDrivers {
    /// The drivers themselves have nothing worth printing; the count is what
    /// a caller debugging a leak wants.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IsleDrivers")
            .field("parked", &self.len())
            .finish()
    }
}

impl IsleDrivers {
    /// A fresh, empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take ownership of `driver` for the rest of the run.
    ///
    /// A poisoned lock is stepped over rather than raised on: this runs while
    /// a store is being opened, the data behind the lock is a plain `Vec` that
    /// no half-finished write can corrupt, and refusing to keep the driver
    /// would leak the thread outright.
    fn park(&self, driver: AsyncIsleDriver) {
        self.parked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(driver);
    }

    /// How many connection threads are still owned here.
    pub fn len(&self) -> usize {
        self.parked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no connection thread has been opened (or all were drained).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drain every thread: queued jobs run to completion, then each thread
    /// stops and is joined.
    ///
    /// The queued jobs matter — the drop backstop submits its `session_closed`
    /// without waiting for it, so this is where those land.  Failures are
    /// collected rather than raised on the first one: a thread that panicked
    /// is no reason to leave the rest running.
    ///
    /// Idempotent: a second call finds nothing parked and returns an empty
    /// list.
    pub async fn shutdown(&self) -> Vec<IsleError> {
        // The guard is released here, before the first `.await` below.
        let drivers: Vec<AsyncIsleDriver> =
            std::mem::take(&mut *self.parked.lock().unwrap_or_else(PoisonError::into_inner));
        let mut failures = Vec::new();
        for driver in drivers {
            if let Err(e) = driver.shutdown().await {
                failures.push(e);
            }
        }
        failures
    }
}

/// The table the log lives in — published as the read contract
/// ([`events_schema`]).
pub const EVENTS_TABLE: &str = "events";

/// The DDL for [`EVENTS_TABLE`] and its two indexes.
///
/// `IF NOT EXISTS` throughout, so opening a fresh database and reopening one
/// an earlier build wrote take the same path.  The `(stream, kind, seq)`
/// index is what makes a kind-filtered read cost the size of the fold rather
/// than the size of the stream, and it keeps the rows in `seq` order within a
/// kind, so the read needs no sort; `(stream, beat, seq)` does the same for
/// the events of one beat.
///
/// `beat` is the one nullable column: it is the caller's to declare and most
/// events do not belong to a beat.  `meta` and `data` are `NOT NULL` because
/// they are filled in with `{}` on the way in ([`stamp`]), so a reader never
/// has to tell an empty object from a missing one.
const SCHEMA_DDL: &str = "CREATE TABLE IF NOT EXISTS events ( \
         stream         TEXT    NOT NULL, \
         seq            INTEGER NOT NULL, \
         epoch_ms       INTEGER NOT NULL, \
         kind           TEXT    NOT NULL, \
         schema_version INTEGER NOT NULL, \
         beat           TEXT    NULL, \
         meta           TEXT    NOT NULL, \
         data           TEXT    NOT NULL, \
         PRIMARY KEY (stream, seq) \
     ); \
     CREATE INDEX IF NOT EXISTS events_stream_kind_seq \
         ON events (stream, kind, seq); \
     CREATE INDEX IF NOT EXISTS events_stream_beat_seq \
         ON events (stream, beat, seq);";

/// One column of [`EVENTS_TABLE`], as SQLite itself reports it.
///
/// Published to the shell so a caller writing SQL against the log reads the
/// column names and types from the database rather than from a list somebody
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

/// Where a store's database lives.
///
/// The store keeps this so it can open a *second* connection to the same
/// database for reads.  For a file that is the same path; for an in-memory
/// database it is the shared-cache URI, which is the only way a second
/// connection can reach one.
#[derive(Debug, Clone)]
enum Db {
    /// A file on disk.
    File(PathBuf),
    /// An in-memory database, addressed by its shared-cache URI.
    Memory(String),
}

impl Db {
    /// The identity of this database, as [`EventStore::database`] reports it.
    ///
    /// The same string the connection is opened by — a path for a file, the
    /// shared-cache URI for an in-memory database — because that is exactly
    /// what "the same database" means here: two stores opened by the same
    /// target reach the same rows, and the `(stream, seq)` key keeps their
    /// streams apart inside it.
    fn id(&self) -> String {
        self.target().to_string_lossy().into_owned()
    }

    /// The URI an in-memory database for `stream` is addressed by.
    ///
    /// Derived from the stream id, so reopening the same stream in the same
    /// process finds the same database — which is what makes an in-memory
    /// session resumable while it is still alive.
    fn memory_uri(stream: &str) -> String {
        format!("file:knl-{stream}?mode=memory&cache=shared")
    }

    /// What SQLite is asked to open: a path for a file, the shared-cache URI
    /// for an in-memory database.
    ///
    /// The URI goes through the same argument the path does, which is why
    /// `SQLITE_OPEN_URI` is in both flag sets below: it is what makes the
    /// `file:` form a URI rather than a relative path called "file:…".
    fn target(&self) -> PathBuf {
        match self {
            Self::File(path) => path.clone(),
            Self::Memory(uri) => PathBuf::from(uri),
        }
    }

    /// The flags the writing connection is opened with.
    fn write_flags() -> OpenFlags {
        OpenFlags::default() | OpenFlags::SQLITE_OPEN_URI
    }

    /// The flags a read-only connection is opened with.
    fn read_only_flags() -> OpenFlags {
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI
    }

    /// Start the writing isle: the thread that owns the connection every
    /// append goes through, with the `events` table ensured before it takes
    /// its first job.
    ///
    /// The thread is created by `std::thread::Builder` inside the isle and
    /// needs no runtime of its own; what this call awaits is the oneshot that
    /// says the connection opened and the DDL ran.  So the caller yields
    /// rather than blocking, which is what lets `knl.open` be reached from
    /// inside the Lua VM at all.
    async fn spawn_writer(&self, drivers: &IsleDrivers) -> KnlResult<AsyncIsle> {
        let (isle, driver) = AsyncIsle::builder()
            .thread_name("knl-events")
            .open_flags(Self::write_flags())
            .wal(BUSY_TIMEOUT)
            .spawn(self.target(), |conn| conn.execute_batch(SCHEMA_DDL))
            .await
            .map_err(KnlError::from)?;
        drivers.park(driver);
        Ok(isle)
    }

    /// Start the reading isle: a second thread, a second connection, and no
    /// write capability on it at all.
    async fn spawn_reader(&self, drivers: &IsleDrivers) -> KnlResult<AsyncIsle> {
        let (isle, driver) = AsyncIsle::builder()
            .thread_name("knl-events-read")
            .open_flags(Self::read_only_flags())
            .busy_timeout(BUSY_TIMEOUT)
            .spawn(self.target(), |conn| {
                conn.execute_batch("PRAGMA query_only = 1;")
            })
            .await
            .map_err(KnlError::from)?;
        drivers.park(driver);
        Ok(isle)
    }
}

/// How a busy write is retried: the isle re-submits the whole job on
/// `SQLITE_BUSY`, backing off between attempts.
///
/// The defaults (3 retries from 50 ms, doubling) are the isle's, and so is the
/// decision of what counts as busy — this store no longer classifies lock
/// contention for the purpose of retrying it.
fn retry_policy() -> RetryPolicy {
    RetryPolicy::default()
}

/// A job's failure, split by who should see it.
///
/// [`Sqlite`](Self::Sqlite) is handed back to the isle, which is what lets it
/// recognise a contended write and try again; a
/// [`Terminal`](Self::Terminal) kernel error (a rejected event, a corrupt row,
/// an encode failure) is carried out through the job's *value* instead, so no
/// retry is spent on something no retry can fix.
enum JobError {
    /// A rusqlite fault, returned to the isle.
    Sqlite(rusqlite::Error),
    /// A terminal kernel error — never retried.
    Terminal(KnlError),
}

/// Hand a job's outcome to the isle in the shape it expects: SQLite's errors
/// as errors (retryable), the kernel's as a value (terminal).
fn finish<T>(outcome: Result<T, JobError>) -> Result<KnlResult<T>, rusqlite::Error> {
    match outcome {
        Ok(value) => Ok(Ok(value)),
        Err(JobError::Sqlite(error)) => Err(error),
        Err(JobError::Terminal(error)) => Ok(Err(error)),
    }
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

/// A durable [`EventStore`] backed by SQLite, scoped to one `stream`.
///
/// The session *is* the stream: one instance serves one session's log.
/// Several instances may point at the same DB file with different streams.
pub struct SqliteEventStore {
    /// The handle on the thread that owns the writing connection.
    ///
    /// Held for the store's whole life, which for an in-memory database is
    /// not merely convenient: a shared-cache in-memory database exists only
    /// while a connection to it is open, so this isle *is* the database.
    writer: AsyncIsle,
    /// Where the database is, so a second connection can be opened to it.
    db: Db,
    /// The read-only isle, started on the first query and reused.
    ///
    /// Lazy because most sessions never run one: a store that only appends
    /// and folds pays nothing — not even a thread — for the read side
    /// existing.  A [`tokio::sync::OnceCell`] rather than the `std` one
    /// because opening it is now an `await`, and because the cell has to stay
    /// `Sync` for the store's `&self` reads to be `Send` futures.
    reader: OnceCell<AsyncIsle>,
    /// Where the drivers of both threads went, so the reader can park its own
    /// when it is opened.
    drivers: IsleDrivers,
    /// The stream this store is scoped to — the session id.
    stream: String,
    /// The identity of the database, computed once at open ([`Db::id`]) so
    /// [`EventStore::database`] can hand back a borrow of it.
    db_id: String,
}

impl SqliteEventStore {
    /// Open (creating if absent) the DB at `path`, scoped to `stream`.
    ///
    /// The `events` table is created if it does not exist, so opening a fresh
    /// file and reopening an existing one take the same path.
    ///
    /// `drivers` takes ownership of the connection thread this starts (and of
    /// the read thread, if a query ever opens one): see [`IsleDrivers`].
    pub async fn open(
        path: &Path,
        stream: impl Into<String>,
        drivers: &IsleDrivers,
    ) -> KnlResult<Self> {
        Self::init(Db::File(path.to_path_buf()), stream.into(), drivers).await
    }

    /// Open an in-memory database for `stream`.
    ///
    /// The database is named after the stream and opened in shared-cache
    /// mode, so the read connection reaches the same rows the writer wrote —
    /// and so reopening the same stream id in the same process finds the same
    /// log.  It lives as long as a connection to it is open, which is until
    /// `drivers` is shut down.
    pub async fn open_memory(stream: impl Into<String>, drivers: &IsleDrivers) -> KnlResult<Self> {
        let stream = stream.into();
        Self::init(Db::Memory(Db::memory_uri(&stream)), stream, drivers).await
    }

    /// Start the writing isle — which sets the busy timeout, applies the WAL
    /// preset and ensures the table and its indexes before it takes a job.
    async fn init(db: Db, stream: String, drivers: &IsleDrivers) -> KnlResult<Self> {
        let writer = db.spawn_writer(drivers).await?;
        let db_id = db.id();
        Ok(Self {
            writer,
            db,
            reader: OnceCell::new(),
            drivers: drivers.clone(),
            stream,
            db_id,
        })
    }

    /// The read-only isle, started on first use.
    ///
    /// A *second* connection to the same database, on a thread of its own and
    /// with no write capability: `SQLITE_OPEN_READ_ONLY` is what SQLite was
    /// asked for, and `query_only` is the same answer said again inside the
    /// connection, so a statement that slipped past the checks on the text
    /// still has nothing to write with.
    ///
    /// A failed open is not remembered: the cell stays empty, so the next
    /// query tries again rather than reporting the first failure forever.
    async fn reader(&self) -> KnlResult<&AsyncIsle> {
        self.reader
            .get_or_try_init(|| self.db.spawn_reader(&self.drivers))
            .await
    }

    /// The columns of the `events` table, as SQLite reports them.
    ///
    /// Read through the *reader*, because this is the read contract: what a
    /// caller's SQL may name. `PRAGMA table_info` rather than a list written
    /// out here, so the published schema cannot drift from the table.
    pub async fn schema(&self) -> KnlResult<Vec<SchemaColumn>> {
        self.reader()
            .await?
            .call(schema_of)
            .await
            .map_err(KnlError::from)
    }
}

/// `PRAGMA table_info(events)`, as [`SchemaColumn`]s.
///
/// One reader for both callers: a live store's [`SqliteEventStore::schema`],
/// which runs it on the reading isle, and [`events_schema`], which runs it on
/// a connection of its own.
fn schema_of(conn: &mut Connection) -> rusqlite::Result<Vec<SchemaColumn>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({EVENTS_TABLE})"))?;
    let rows = stmt.query_map([], |row| {
        Ok(SchemaColumn {
            name: row.get::<_, String>("name")?,
            declared_type: row.get::<_, String>("type")?,
            pk: row.get::<_, i64>("pk")? > 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
}

/// The columns of the `events` table, without a session to ask.
///
/// The schema is a property of the kernel, not of any one log, so this creates
/// the table in a private in-memory database and reads it straight back — the
/// same `PRAGMA table_info` a caller's own store would answer with.  It is
/// what `knl.api()` publishes.
///
/// Deliberately **not** async, and deliberately not an isle.  It opens a
/// nameless in-memory database, runs `CREATE TABLE IF NOT EXISTS` and one
/// pragma against it, and drops it: no file is touched, no lock can be
/// contended, and no thread is started, so there is nothing here for the
/// caller to wait on.  That is what keeps `knl.api()` a synchronous call —
/// a declaration of the surface should not have to be awaited — while the
/// rule that the VM thread never waits on the OS still holds, because this
/// never reaches the OS.
pub fn events_schema() -> KnlResult<Vec<SchemaColumn>> {
    let mut conn = Connection::open_in_memory().map_err(KnlError::from)?;
    conn.execute_batch(SCHEMA_DDL).map_err(KnlError::from)?;
    schema_of(&mut conn).map_err(KnlError::from)
}

/// The kinds a read was asked for, owned, so the selection can be sent to the
/// isle's thread along with the closure that uses it.
fn owned_kinds(kinds: Option<&[&str]>) -> Option<Vec<String>> {
    kinds.map(|kinds| kinds.iter().map(|kind| (*kind).to_string()).collect())
}

#[async_trait]
impl EventStore for SqliteEventStore {
    async fn append(&mut self, mut event: Map<String, Value>) -> KnlResult<Committed> {
        // Reject before touching the stream: a rejected event burns no seq.
        validate_event(&event)?;
        // Stamp the schema version once, before the job is submitted; the
        // kernel-owned seq / epoch_ms are stamped per attempt inside the
        // transaction, recomputed from the live head each time.
        stamp_schema_version(&mut event);
        let stream = self.stream.clone();
        self.writer
            .call_retry(retry_policy(), move |conn| {
                finish(append_in(conn, &stream, &event))
            })
            .await
            .map_err(KnlError::from)?
    }

    async fn append_many(&mut self, events: Vec<Map<String, Value>>) -> KnlResult<Vec<Committed>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        // Validate before the transaction is opened: a batch with a malformed
        // event in it never takes the write lock at all.
        for event in &events {
            validate_event(event)?;
        }
        // One IMMEDIATE transaction for the whole batch, so the facts that
        // belong together land together.  A contended attempt is retried
        // whole; nothing outside the transaction has been changed by a failed
        // one, so re-running it is the correct thing to do.
        let stream = self.stream.clone();
        self.writer
            .call_retry(retry_policy(), move |conn| {
                finish(append_many_in(conn, &stream, &events))
            })
            .await
            .map_err(KnlError::from)?
    }

    async fn append_if(
        &mut self,
        kinds: Option<&[&str]>,
        decide: Decision,
    ) -> KnlResult<Option<Committed>> {
        // The read, the decision and the insert share one IMMEDIATE
        // transaction, so the invariant `decide` checks holds at the instant
        // the event lands — and all three now run in one job, on the isle's
        // own thread, because the decision is an owned `Send` closure that
        // travels with it.  The channel round trip the borrowed form needed
        // (job asks, caller answers, both waiting on each other with the write
        // lock held) is gone with it.
        let stream = self.stream.clone();
        let kinds = owned_kinds(kinds);
        self.writer
            .call(move |conn| finish(append_if_in(conn, &stream, kinds.as_deref(), decide)))
            .await
            .map_err(KnlError::from)?
    }

    async fn append_if_many(
        &mut self,
        other: &str,
        kinds: Option<&[&str]>,
        decide: SplitDecision,
    ) -> KnlResult<Option<Split<Committed>>> {
        // One IMMEDIATE transaction over both streams, exactly as
        // `append_if` takes one over this stream: they are rows of the same
        // table on the same connection, so "two streams" costs the write
        // nothing beyond a second `MAX(seq)`.  Not retried, for the reason
        // `append_if` is not — the decision is a `FnOnce` and an attempt
        // consumes it.
        let stream = self.stream.clone();
        let other = other.to_string();
        let kinds = owned_kinds(kinds);
        self.writer
            .call(move |conn| {
                finish(append_if_many_in(
                    conn,
                    &stream,
                    &other,
                    kinds.as_deref(),
                    decide,
                ))
            })
            .await
            .map_err(KnlError::from)?
    }

    async fn append_with_open_children(
        &mut self,
        scan: &ChildScan,
        decide: ChildrenDecision,
    ) -> KnlResult<Committed> {
        // The scan reads other streams and the insert writes this one, so
        // they share the IMMEDIATE transaction: what the boundary records is
        // what was true at the instant it landed, not a moment before it.
        let stream = self.stream.clone();
        let scan = scan.clone();
        self.writer
            .call(move |conn| finish(append_with_open_children_in(conn, &stream, &scan, decide)))
            .await
            .map_err(KnlError::from)?
    }

    fn database(&self) -> Option<&str> {
        Some(&self.db_id)
    }

    async fn read_kinds(
        &self,
        kinds: Option<&[&str]>,
        from_seq: u64,
        limit: usize,
    ) -> KnlResult<Vec<Value>> {
        // An empty selection selects nothing — and `kind IN ()` is not SQL,
        // so it is answered here rather than built into a statement.
        if kinds.is_some_and(<[&str]>::is_empty) {
            return Ok(Vec::new());
        }
        // `usize::MAX` (an unbounded read) caps at i64::MAX, which SQLite
        // treats as "no limit"; `0` reads nothing.
        let capped = i64::try_from(limit).unwrap_or(i64::MAX);
        let kinds = owned_kinds(kinds);
        let stream = self.stream.clone();
        self.writer
            .call(move |conn| finish(read_in(conn, &stream, kinds.as_deref(), from_seq, capped)))
            .await
            .map_err(KnlError::from)?
    }

    async fn head(&self) -> KnlResult<Option<u64>> {
        // A transient busy read must surface, not read as "empty": a caller
        // deciding open-vs-resume (or a CAS) on a swallowed error would
        // treat a populated stream as fresh.  Same discipline as read().
        let stream = self.stream.clone();
        self.writer
            .call(move |conn| head_in(conn, &stream))
            .await
            .map_err(KnlError::from)
    }

    async fn len(&self) -> KnlResult<usize> {
        let stream = self.stream.clone();
        self.writer
            .call(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE stream = ?1",
                    params![stream],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .map(|n| n as usize)
            .map_err(KnlError::from)
    }

    async fn query(&self, plan: &QueryPlan) -> KnlResult<QueryRows> {
        // The deadline is the isle's: it interrupts the statement when the
        // time is up and reports `Timeout`, so there is no watchdog thread
        // here to outlive the query it was watching.
        let timeout = plan.timeout;
        let plan = plan.clone();
        self.reader()
            .await?
            .call_timeout(timeout, move |conn| Ok(run_query(conn, &plan)))
            .await
            .map_err(KnlError::from)?
    }

    fn detach_append(&self, mut event: Map<String, Value>) {
        // The drop backstop's path, and the one write nobody awaits.  A
        // handle that was collected has no caller left to raise to and no
        // task left to wait in, so the job is handed to the isle and let go
        // of: the thread runs it because its driver outlives every session
        // ([`IsleDrivers`]), and the boundary lands before the host drains
        // that thread at shutdown.
        if let Err(e) = validate_event(&event) {
            tracing::warn!(error = %e, "knl: a detached append was refused before it was submitted");
            return;
        }
        stamp_schema_version(&mut event);
        let stream = self.stream.clone();
        // `detach`, not a dropped task: dropping an `AsyncTask` cancels the
        // job it stands for, which would throw away the very event this
        // exists to record.
        self.writer
            .spawn_call(move |conn| finish(append_in(conn, &stream, &event)))
            .detach();
    }
}

/// A query's own translation of a rusqlite failure.
///
/// The write path's [`From<rusqlite::Error>`] answers a different question —
/// "can this write be retried" — and has no reason to know about deadlines.
/// Here there are two more outcomes a caller can act on: a statement the
/// watchdog cut short is [`KnlError::Timeout`] (the query was too slow, not
/// the store too busy), and a value that came back and would not read as what
/// it is declared to be is [`KnlError::Corruption`] — the IO worked, so what
/// is wrong is the data.  Matched on the error's shape, never on message text.
fn query_error(error: rusqlite::Error) -> KnlError {
    if let rusqlite::Error::SqliteFailure(inner, _) = &error {
        if inner.code == rusqlite::ErrorCode::OperationInterrupted {
            return KnlError::Timeout(format!("query interrupted: {error}"));
        }
    }
    match error {
        rusqlite::Error::Utf8Error(_)
        | rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::IntegralValueOutOfRange(..) => {
            KnlError::Corruption(format!("sqlite: query: {error}"))
        }
        other => KnlError::from(other),
    }
}

/// Prepare, check, bind and run one query.
///
/// Runs on the reading isle's thread, under the deadline that thread was given
/// ([`EventStore::query`]): SQLite has no per-statement timeout — `busy_timeout`
/// bounds waiting for a *lock*, which is a different thing from a statement
/// that is simply expensive — so the isle interrupts the connection when the
/// time is up.  The interrupt reaches this function as `SQLITE_INTERRUPT` on
/// whichever step was running, and [`query_error`] names it a timeout.
fn run_query(conn: &Connection, plan: &QueryPlan) -> KnlResult<QueryRows> {
    // A statement that will not compile is the caller's SQL, not the store
    // failing: report it as the refusal it is, unless the database was too
    // busy to answer at all.
    let mut stmt = conn.prepare(&plan.sql).map_err(|error| {
        if is_retryable(&error) {
            KnlError::from(error)
        } else {
            KnlError::Validation(format!("sql: {error}"))
        }
    })?;
    // The second of the three guards (the text was checked before this, the
    // connection has no write capability at all): SQLite's own answer to
    // "does this statement change the database".
    if !stmt.readonly() {
        return Err(KnlError::Validation(
            "a query may not write; only SELECT / WITH statements are run".to_string(),
        ));
    }
    bind(&mut stmt, plan)?;

    // Taken before the rows borrow the statement, and owned, so the columns
    // outlive the borrow.
    let columns: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();

    let mut rows = stmt.raw_query();
    let mut out = Vec::new();
    while out.len() < plan.limit {
        let Some(row) = rows.next().map_err(query_error)? else {
            // The whole result set fitted.
            return Ok(QueryRows {
                rows: out,
                truncated: false,
            });
        };
        let mut record = Map::new();
        for (index, column) in columns.iter().enumerate() {
            // A NULL is an absent key rather than a null value: the Lua side
            // reads it as `nil`, which is what a missing column means there.
            if let Some(value) = read_value(row.get_ref(index).map_err(query_error)?)? {
                record.insert(column.clone(), value);
            }
        }
        out.push(record);
    }
    // The cap was reached: whether anything was actually cut off is one more
    // step, so a result that happens to be exactly `limit` long is not
    // reported as truncated.
    let truncated = rows.next().map_err(query_error)?.is_some();
    Ok(QueryRows {
        rows: out,
        truncated,
    })
}

/// Bind every parameter the statement declares.
///
/// Driven by the *statement*, not by the caller's table: SQLite is asked what
/// parameters it compiled and each one is answered, so a value that matches
/// nothing and a parameter that nothing matches are both errors instead of a
/// silent NULL.  The reserved names ([`STREAM_PARAM`] and the
/// `:knl_sessions_*` slots [`super::query`] wrote) are the kernel's;
/// everything else is looked up in what the caller passed.
fn bind(stmt: &mut rusqlite::Statement<'_>, plan: &QueryPlan) -> KnlResult<()> {
    const NO_VALUES: &[Value] = &[];

    let slots: Vec<String> = (0..plan.sessions.len()).map(session_slot).collect();
    let given: &[Value] = match &plan.params {
        QueryParams::Positional(values) => values,
        _ => NO_VALUES,
    };
    let mut taken = 0;

    for index in 1..=stmt.parameter_count() {
        // Read out as an owned name first: the borrow of the statement ends
        // here, so the binding below can take it mutably.
        let name = stmt.parameter_name(index).map(str::to_string);
        let Some(name) = name else {
            // An anonymous `?`: the caller's, in the order they were given.
            // Every parameter the kernel wrote is named, so there is nothing
            // of ours here to confuse them with.
            let value = given.get(taken).ok_or_else(|| {
                KnlError::Validation(format!(
                    "the query has more `?` parameters than the {} value(s) given",
                    given.len()
                ))
            })?;
            taken += 1;
            stmt.raw_bind_parameter(index, SqlParam(value.clone()))
                .map_err(query_error)?;
            continue;
        };

        if name == STREAM_PARAM {
            stmt.raw_bind_parameter(index, plan.stream.clone())
                .map_err(query_error)?;
            continue;
        }
        if let Some(slot) = slots.iter().position(|slot| *slot == name) {
            stmt.raw_bind_parameter(index, plan.sessions[slot].clone())
                .map_err(query_error)?;
            continue;
        }

        let QueryParams::Named(named) = &plan.params else {
            return Err(KnlError::Validation(format!(
                "the query names the parameter {name:?}, so params must be a table of names \
                 to values"
            )));
        };
        // The prefix character is SQLite's, not the caller's: `:kind` is
        // answered by `kind`.  The full spelling is accepted too, for a
        // caller that writes what it sees.
        let value = named
            .get(&name[1..])
            .or_else(|| named.get(&name))
            .ok_or_else(|| {
                KnlError::Validation(format!("no value was given for the parameter {name:?}"))
            })?;
        stmt.raw_bind_parameter(index, SqlParam(value.clone()))
            .map_err(query_error)?;
    }

    if given.len() > taken {
        return Err(KnlError::Validation(format!(
            "{} value(s) were given for {taken} `?` parameter(s)",
            given.len()
        )));
    }
    Ok(())
}

/// A caller's JSON value on its way into a bound parameter.
///
/// The four SQLite types a JSON value maps onto without inventing anything:
/// null, integer, real, text.  A composite — an array or an object — is not a
/// SQLite value, and encoding one as its JSON text would be the kernel
/// guessing what the caller meant, so it is refused.
struct SqlParam(Value);

impl rusqlite::ToSql for SqlParam {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::ToSqlOutput;
        let value = match &self.0 {
            Value::Null => SqlValue::Null,
            Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    SqlValue::Integer(i)
                } else if let Some(f) = n.as_f64() {
                    SqlValue::Real(f)
                } else {
                    return Err(rusqlite::Error::ToSqlConversionFailure(
                        format!("{n} is not a SQLite number").into(),
                    ));
                }
            }
            Value::String(s) => SqlValue::Text(s.clone()),
            other => {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    format!("a {} is not a SQLite value", type_name_of(other)).into(),
                ));
            }
        };
        Ok(ToSqlOutput::Owned(value))
    }
}

/// What kind of JSON value this is, for a refusal message.
fn type_name_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "table",
    }
}

/// One column of one row, as JSON — or `None` for `NULL`.
///
/// INTEGER and REAL come back as numbers, TEXT as a string.  A BLOB comes
/// back as a string too, lossily: the boundary above this one is Lua, whose
/// strings are byte strings, and refusing the row would make a column nobody
/// selected on purpose fatal.  A TEXT column that is not UTF-8 is a different
/// matter — it was declared to be text and it is not — so that is corruption.
/// A REAL that is NaN or infinite has no representation on the other side of
/// this boundary, and dropping it would hand back a row with a column
/// silently missing, so it is raised instead.
fn read_value(value: ValueRef<'_>) -> KnlResult<Option<Value>> {
    Ok(match value {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(Value::from(i)),
        ValueRef::Real(f) => {
            let number = serde_json::Number::from_f64(f).ok_or_else(|| {
                KnlError::Storage(format!(
                    "sqlite: a REAL column is {f}, which has no value on the other side of the \
                     bridge"
                ))
            })?;
            Some(Value::Number(number))
        }
        ValueRef::Text(bytes) => {
            let text = std::str::from_utf8(bytes).map_err(|e| {
                KnlError::Corruption(format!("sqlite: a TEXT column is not valid UTF-8: {e}"))
            })?;
            Some(Value::from(text))
        }
        ValueRef::Blob(bytes) => Some(Value::from(String::from_utf8_lossy(bytes).into_owned())),
    })
}

/// One `IMMEDIATE` append: take the reserved lock up front, compute the next
/// `seq` from the live head, stamp and insert, then commit.
///
/// Runs on the writing isle's thread, and may run more than once: a contended
/// `BEGIN` is a [`JobError::Sqlite`], which is what the isle's retry keys on,
/// and nothing outside the transaction was changed by an attempt that failed.
fn append_in(
    conn: &mut Connection,
    stream: &str,
    event: &Map<String, Value>,
) -> Result<Committed, JobError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(JobError::Sqlite)?;
    let seq = next_seq(&tx, stream).map_err(JobError::Sqlite)?;
    let epoch_ms = now_ms();
    let mut row = event.clone();
    stamp(&mut row, seq, epoch_ms);
    insert_row(&tx, stream, seq, epoch_ms, &row)?;
    tx.commit().map_err(JobError::Sqlite)?;
    Ok(Committed { seq, epoch_ms })
}

/// One `IMMEDIATE` batch append: take the reserved lock up front, number the
/// events on from the live head, and insert them all before committing.
///
/// All or nothing: an event that will not encode, or a contended insert
/// part-way through, drops the transaction and leaves the stream exactly as
/// it was — which is what lets a caller write two facts that are one fact.
fn append_many_in(
    conn: &mut Connection,
    stream: &str,
    events: &[Map<String, Value>],
) -> Result<Vec<Committed>, JobError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(JobError::Sqlite)?;
    let committed = insert_batch(&tx, stream, events)?;
    tx.commit().map_err(JobError::Sqlite)?;
    Ok(committed)
}

/// Number `events` on from `stream`'s live head, stamp them and insert them,
/// inside a transaction the caller opened and commits.
///
/// The one numbering rule for every batch a transaction writes — a plain
/// [`append_many_in`], and each side of an allocation
/// ([`append_if_many_in`]) — so the second stream of a two-stream write is
/// numbered exactly as the first is: from its own head, which is what makes
/// `seq` per-stream rather than per-transaction.
///
/// It validates, because an event that reaches here has not always been
/// checked: a decision's events are the decision's, and one it built wrong
/// must not be the first thing a stream carries.  A [`JobError::Terminal`]
/// drops the transaction, so a batch that fails part-way writes nothing.
fn insert_batch(
    tx: &Connection,
    stream: &str,
    events: &[Map<String, Value>],
) -> Result<Vec<Committed>, JobError> {
    let mut seq = next_seq(tx, stream).map_err(JobError::Sqlite)?;
    let mut committed = Vec::with_capacity(events.len());
    for event in events {
        validate_event(event).map_err(JobError::Terminal)?;
        let epoch_ms = now_ms();
        let mut row = event.clone();
        stamp_schema_version(&mut row);
        stamp(&mut row, seq, epoch_ms);
        insert_row(tx, stream, seq, epoch_ms, &row)?;
        committed.push(Committed { seq, epoch_ms });
        seq = seq.saturating_add(1);
    }
    Ok(committed)
}

/// One `IMMEDIATE` decide-then-append over two streams: read this stream,
/// ask the decision what to record where, and insert both sides before
/// committing.
///
/// The two-stream twin of [`append_if_in`], and the reason it exists is the
/// atomicity rather than the convenience: an allocation is a move between two
/// ledgers, and a reader that met one side without the other would be reading
/// units that had left one balance without arriving in the other.  Both
/// streams are rows of the same table on this one connection, so the same
/// transaction covers them.
///
/// A `None` decision commits nothing.  A [`Split`] with an empty `other`
/// writes only this stream, which is how a refusal is recorded: the fact that
/// the allocation was asked for and turned down, with no child opened.
fn append_if_many_in(
    conn: &mut Connection,
    stream: &str,
    other: &str,
    kinds: Option<&[String]>,
    decide: SplitDecision,
) -> Result<Option<Split<Committed>>, JobError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(JobError::Sqlite)?;
    let events = read_in(&tx, stream, kinds, 0, i64::MAX)?;
    let Some(split) = decide(events) else {
        // Nothing to write: the transaction is rolled back on drop.
        return Ok(None);
    };
    let own = insert_batch(&tx, stream, &split.own)?;
    let other = insert_batch(&tx, other, &split.other)?;
    tx.commit().map_err(JobError::Sqlite)?;
    Ok(Some(Split { own, other }))
}

/// One `IMMEDIATE` scan-then-append: find the streams this one is the parent
/// of that have not ended, hand them to the decision, and insert the event it
/// builds.
///
/// The scan is inside the transaction on purpose.  Asked before the write, it
/// would answer about a moment the boundary does not land in — a child could
/// end, or a new one open, in between — and a `session_closed` that named a
/// child which had already closed would be a record of something that never
/// happened.
fn append_with_open_children_in(
    conn: &mut Connection,
    stream: &str,
    scan: &ChildScan,
    decide: ChildrenDecision,
) -> Result<Committed, JobError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(JobError::Sqlite)?;
    let children = open_children_in(&tx, stream, scan).map_err(JobError::Sqlite)?;
    let committed = insert_batch(&tx, stream, &[decide(children)])?;
    tx.commit().map_err(JobError::Sqlite)?;
    // One event in, one out: `insert_batch` numbers what it is given, and it
    // was given exactly one.
    committed
        .into_iter()
        .next()
        .ok_or_else(|| JobError::Terminal(KnlError::Storage("the close wrote nothing".to_string())))
}

/// The streams that name `stream` as their parent and carry no ending.
///
/// The vocabulary is the caller's ([`ChildScan`]): which kind opens a stream,
/// which kind ends one, and where in the opening's `data` the parent is
/// named.  The JSON path is built from the field name here rather than being
/// bound as a value, because `json_extract`'s path argument is not one — the
/// name is the kernel's own constant, never a caller's text.
///
/// Ordered by when each child opened, so a close records its children in the
/// order they were started rather than in whatever order the rows came back.
fn open_children_in(
    conn: &Connection,
    stream: &str,
    scan: &ChildScan,
) -> rusqlite::Result<Vec<String>> {
    let path = format!("$.{}", scan.parent_field);
    let mut stmt = conn.prepare(
        "SELECT opened.stream \
           FROM events AS opened \
          WHERE opened.kind = ?1 \
            AND json_extract(opened.data, ?2) = ?3 \
            AND NOT EXISTS ( \
                SELECT 1 FROM events AS ending \
                 WHERE ending.stream = opened.stream AND ending.kind = ?4 \
            ) \
          ORDER BY opened.epoch_ms, opened.stream",
    )?;
    let rows = stmt.query_map(params![scan.opened, path, stream, scan.closed], |row| {
        row.get::<_, String>(0)
    })?;
    rows.collect()
}

/// One `IMMEDIATE` decide-then-append: read the stream, ask the caller's
/// closure what to record, and insert its answer in the same transaction.
///
/// The decision travels *with* the job — it is owned and `Send` — so it runs
/// here, on the isle's thread, between the read and the insert, with the write
/// lock held throughout.  Nothing waits on anything else: the caller's task is
/// suspended on the job's own oneshot and there is no second channel for the
/// two sides to deadlock across.
///
/// `kinds` narrows what the decision is shown, not where its answer lands:
/// the new event's `seq` comes from the stream's live head, so a filtered
/// decision numbers its write against everything, exactly as an ordinary
/// append does.
///
/// A `None` decision commits nothing — the transaction is dropped, so the
/// stream is exactly as it was — and reports `Ok(None)`.
fn append_if_in(
    conn: &mut Connection,
    stream: &str,
    kinds: Option<&[String]>,
    decide: Decision,
) -> Result<Option<Committed>, JobError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(JobError::Sqlite)?;
    let events = read_in(&tx, stream, kinds, 0, i64::MAX)?;
    let Some(event) = decide(events) else {
        // Nothing to write: the transaction is rolled back on drop.
        return Ok(None);
    };
    // The decision's event is validated like any other: a malformed one is
    // refused and the transaction goes no further.
    validate_event(&event).map_err(JobError::Terminal)?;
    // The head of the whole stream, not of the events the decision was shown:
    // a filtered read says nothing about where the next event goes.
    let seq = next_seq(&tx, stream).map_err(JobError::Sqlite)?;
    let epoch_ms = now_ms();
    let mut row = event.clone();
    stamp_schema_version(&mut row);
    stamp(&mut row, seq, epoch_ms);
    insert_row(&tx, stream, seq, epoch_ms, &row)?;
    tx.commit().map_err(JobError::Sqlite)?;
    Ok(Some(Committed { seq, epoch_ms }))
}

/// The columns a read selects, in the order [`read_row`] takes them.
const READ_COLUMNS: &str = "seq, epoch_ms, kind, schema_version, beat, meta, data";

/// One stored row, as its columns come back from SQLite.
///
/// The raw values, before the two JSON columns are decoded: reading and
/// decoding are separate so a fault on the read (retryable) and a value that
/// will not decode (corruption) stay two different answers.
struct StoredRow {
    /// The store-assigned sequence number.
    seq: i64,
    /// The wall-clock append time the store stamped.
    epoch_ms: i64,
    /// The event's kind.
    kind: String,
    /// The shape the event was written under.
    schema_version: i64,
    /// The beat the caller declared, if it declared one.
    beat: Option<String>,
    /// The shallow `meta` object, as stored text.
    meta: String,
    /// The kind's own `data` object, as stored text.
    data: String,
}

/// Take one row's columns, in [`READ_COLUMNS`] order.
fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    Ok(StoredRow {
        seq: row.get(0)?,
        epoch_ms: row.get(1)?,
        kind: row.get(2)?,
        schema_version: row.get(3)?,
        beat: row.get(4)?,
        meta: row.get(5)?,
        data: row.get(6)?,
    })
}

/// Rebuild the event object a row was written from.
///
/// The inverse of [`insert_row`], and exactly that: the same keys in the same
/// envelope, so a caller reading a durable log sees what it wrote.  An absent
/// `beat` is an absent key rather than a null — the kernel's rule is that a
/// beat is a string when it is there at all.
fn event_of(row: StoredRow) -> KnlResult<Value> {
    let meta = decode_object(&row.meta, FIELD_META)?;
    let data = decode_object(&row.data, FIELD_DATA)?;

    let mut event = Map::new();
    event.insert(FIELD_KIND.to_string(), Value::from(row.kind));
    if let Some(beat) = row.beat {
        event.insert(FIELD_BEAT.to_string(), Value::from(beat));
    }
    event.insert(FIELD_META.to_string(), meta);
    event.insert(FIELD_DATA.to_string(), data);
    event.insert(FIELD_SEQ.to_string(), Value::from(row.seq as u64));
    event.insert(FIELD_EPOCH_MS.to_string(), Value::from(row.epoch_ms as u64));
    event.insert(
        SCHEMA_VERSION_FIELD.to_string(),
        Value::from(row.schema_version as u64),
    );
    Ok(Value::Object(event))
}

/// Decode a stored JSON column, which must be an object.
///
/// Corruption rather than storage: the IO worked and the bytes came back, so
/// what is wrong is the data, and no retry changes it.  A value that is not
/// an object is the same fault as one that will not parse — the store's own
/// writes are objects, so a scalar here came from the bytes.
fn decode_object(text: &str, column: &str) -> KnlResult<Value> {
    let value = serde_json::from_str::<Value>(text)
        .map_err(|e| KnlError::Corruption(format!("sqlite: corrupt event {column}: {e}")))?;
    if !value.is_object() {
        return Err(KnlError::Corruption(format!(
            "sqlite: corrupt event {column}: stored as {}, not a table",
            super::event::json_type_name(&value)
        )));
    }
    Ok(value)
}

/// The read statement for a stream, with its bound arguments.
///
/// One builder for both read paths — the plain one and the in-transaction
/// twin — so a filtered read and the input a decision is shown select the
/// same rows by the same rule.  The kinds are bound as parameters rather than
/// written into the SQL, so a kind is data here as it is everywhere else.
fn read_query(
    stream: &str,
    kinds: Option<&[String]>,
    from_seq: u64,
    limit: i64,
) -> (String, Vec<SqlValue>) {
    let mut sql = format!("SELECT {READ_COLUMNS} FROM events WHERE stream = ? AND seq >= ?");
    let mut args = vec![
        SqlValue::Text(stream.to_string()),
        SqlValue::Integer(from_seq as i64),
    ];
    if let Some(kinds) = kinds {
        let placeholders = vec!["?"; kinds.len()].join(", ");
        sql.push_str(&format!(" AND kind IN ({placeholders})"));
        args.extend(kinds.iter().map(|kind| SqlValue::Text(kind.clone())));
    }
    sql.push_str(" ORDER BY seq ASC LIMIT ?");
    args.push(SqlValue::Integer(limit));
    (sql, args)
}

/// The events of `stream`, in `seq` order, read on the isle's thread.
///
/// The one read both paths take — a plain [`EventStore::read_kinds`] and the
/// input a decision is shown from inside its transaction — so they select the
/// same rows by the same rule.  A fault on the read itself is SQLite's (and so
/// retryable); a row whose stored objects do not decode is corruption and
/// terminal, and it surfaces as an error rather than being silently dropped,
/// so a caller (resume) never re-folds a truncated log into the wrong state.
fn read_in(
    conn: &Connection,
    stream: &str,
    kinds: Option<&[String]>,
    from_seq: u64,
    limit: i64,
) -> Result<Vec<Value>, JobError> {
    // An empty selection selects nothing, and `kind IN ()` is not SQL.
    if kinds.is_some_and(<[String]>::is_empty) {
        return Ok(Vec::new());
    }
    let (sql, args) = read_query(stream, kinds, from_seq, limit);
    let mut stmt = conn.prepare(&sql).map_err(JobError::Sqlite)?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), read_row)
        .map_err(JobError::Sqlite)?;
    let mut events = Vec::new();
    for row in rows {
        let row = row.map_err(JobError::Sqlite)?;
        events.push(event_of(row).map_err(JobError::Terminal)?);
    }
    Ok(events)
}

/// The next `seq` for `stream`: `MAX(seq) + 1`, or `1` for an empty stream.
///
/// Returns the raw rusqlite error so the retry driver can key on its code.
fn next_seq(conn: &Connection, stream: &str) -> Result<u64, rusqlite::Error> {
    conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE stream = ?1",
        params![stream],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n as u64)
}

/// The current head of `stream`: `MAX(seq)`, or `None` when empty.
///
/// Returns the raw rusqlite error so the retry driver can key on its code.
fn head_in(conn: &Connection, stream: &str) -> Result<Option<u64>, rusqlite::Error> {
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(seq) FROM events WHERE stream = ?1",
        params![stream],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    Ok(max.map(|n| n as u64))
}

/// Insert the fully-stamped event, one column per envelope key and one each
/// for the two objects it carries, so a read rebuilds the exact same `Value`
/// the caller wrote ([`event_of`]).
///
/// An encode failure is terminal; a contended insert is retryable.
fn insert_row(
    conn: &Connection,
    stream: &str,
    seq: u64,
    epoch_ms: u64,
    event: &Map<String, Value>,
) -> Result<(), JobError> {
    let kind = event.get(FIELD_KIND).and_then(Value::as_str).unwrap_or("");
    let schema_version = event
        .get(SCHEMA_VERSION_FIELD)
        .and_then(Value::as_u64)
        .unwrap_or(CURRENT_SCHEMA_VERSION);
    // The beat is the caller's and most events have none: an undeclared one
    // is a NULL in its column, which is what the read gives back as an
    // absent key.
    let beat = event.get(FIELD_BEAT).and_then(Value::as_str);
    let meta = encode_object(event.get(FIELD_META), FIELD_META)?;
    let data = encode_object(event.get(FIELD_DATA), FIELD_DATA)?;
    conn.execute(
        "INSERT INTO events (stream, seq, epoch_ms, kind, schema_version, beat, meta, data) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            stream,
            seq as i64,
            epoch_ms as i64,
            kind,
            schema_version as i64,
            beat,
            meta,
            data
        ],
    )
    .map_err(JobError::Sqlite)?;
    Ok(())
}

/// Encode `meta` / `data` for its column: the object as text, `{}` when the
/// event carries none.
///
/// Both are filled in on the way through [`stamp`], so the default is a
/// belt-and-braces answer rather than the usual path — and it is the empty
/// object either way, which is what makes the column `NOT NULL`.
///
/// An event that will not encode never reaches the disk, so a failure here is
/// the store failing to do the work rather than data that came back wrong —
/// `Storage`, not `Corruption`.
fn encode_object(value: Option<&Value>, column: &str) -> Result<String, JobError> {
    let Some(value) = value else {
        return Ok("{}".to_string());
    };
    serde_json::to_string(value).map_err(|e| {
        JobError::Terminal(KnlError::Storage(format!(
            "sqlite: encode event {column}: {e}"
        )))
    })
}

/// Classify a rusqlite error into the kernel's vocabulary.
///
/// This is the one place the backend's error language is translated, and the
/// split is the one the caller can act on: a contended lock is
/// [`KnlError::Busy`] — the same call may succeed if it is made again — and
/// everything else is [`KnlError::Storage`], a fault the kernel cannot promise
/// anything about.  Matched on the SQLite error *code*, never the message
/// text, so the classification does not drift with a library's wording.
///
/// This is a wider net than the isle's own retry uses: the isle re-submits on
/// `SQLITE_BUSY` alone, because that is contention with another connection and
/// clears on its own, while `SQLITE_LOCKED` within one connection does not.
/// What the *caller* is told is the coarser question — "is another attempt
/// worth making at all" — and for that both are worth a try.
///
/// Corruption is not produced here: a row that comes back and will not decode
/// is a fault of the data rather than of the store, so it is raised where the
/// decode happens.
impl From<rusqlite::Error> for KnlError {
    fn from(error: rusqlite::Error) -> Self {
        if is_retryable(&error) {
            return KnlError::Busy(format!("sqlite: busy/locked: {error}"));
        }
        KnlError::Storage(format!("sqlite: {error}"))
    }
}

/// Translate an isle-level failure into the kernel's vocabulary.
///
/// The isle answers two kinds of question, and they map onto two kinds of
/// kernel error.  A SQL fault is passed straight through to the translation
/// above, so a contended write still reads as [`KnlError::Busy`] however it
/// arrived.  The isle's own conditions are about the *thread*, and they split
/// on whether waiting could help:
///
/// - `QueueFull` is backpressure — the connection thread is alive and behind,
///   so this is [`KnlError::Busy`], the one class that says "ask again";
/// - `Timeout` and `Cancelled` both mean a job was cut short rather than
///   answered, which is [`KnlError::Timeout`]: the deadline was the caller's,
///   and another identical attempt buys nothing;
/// - `Closed` (the thread is gone) and `Panicked` are [`KnlError::Storage`] —
///   the store could not do the work, and no retry changes that.
impl From<IsleError> for KnlError {
    fn from(error: IsleError) -> Self {
        match error {
            IsleError::Sqlite(error) => KnlError::from(error),
            IsleError::QueueFull => {
                KnlError::Busy("sqlite: the connection thread is at capacity".to_string())
            }
            IsleError::Timeout => KnlError::Timeout("sqlite: the deadline elapsed".to_string()),
            IsleError::Cancelled => KnlError::Timeout("sqlite: the job was cancelled".to_string()),
            // `IsleError` is `#[non_exhaustive]`: anything not named above is
            // the store failing to do the work, which is what `Storage` is.
            other => KnlError::Storage(format!("sqlite: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::{kind_of, seq_of};
    use crate::knl::query::QueryOpts;
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

    /// A store on an in-memory database of its very own, with the collection
    /// that owns its connection thread.
    ///
    /// The name matters: an in-memory database is shared by *name*, which is
    /// what lets the reader see the writer's rows — and would equally let two
    /// tests running in parallel see each other's.  A fresh id per store keeps
    /// each test's log to itself.
    ///
    /// The [`IsleDrivers`] comes back with the store because the caller has to
    /// hold it: it owns the connection thread, and a test that dropped it
    /// early would be pulling the database out from under its own assertions.
    async fn mem_store() -> (SqliteEventStore, IsleDrivers) {
        let drivers = IsleDrivers::new();
        let store = SqliteEventStore::open_memory(uuid::Uuid::new_v4().to_string(), &drivers)
            .await
            .expect("open");
        (store, drivers)
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
        let (mut store, _drivers) = mem_store().await;
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
        let (mut store, _drivers) = mem_store().await;
        store
            .append(obj(json!({ "text": "no kind" })))
            .await
            .expect_err("kind is required");
        assert_eq!(store.len().await.expect("len"), 0);
        assert_eq!(store.append(ev(1)).await.expect("append").seq, 1);
    }

    /// `append_if` decides on the stream inside its transaction: the events
    /// it is handed are the durable ones, a `Some` lands at the next seq, and
    /// a `None` commits nothing.
    #[tokio::test]
    async fn append_if_decides_inside_the_transaction_and_writes_only_a_some() {
        let (mut store, _drivers) = mem_store().await;
        store.append(ev(1)).await.expect("seed");

        // The decision runs on the connection's own thread now, so what it
        // saw comes back through a shared cell rather than a borrow.
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

    /// A malformed decision is refused and leaves the stream alone.
    #[tokio::test]
    async fn append_if_validates_the_event_the_decision_returns() {
        let (mut store, _drivers) = mem_store().await;
        store
            .append_if(None, decide(|_| Some(obj(json!({ "text": "no kind" })))))
            .await
            .expect_err("kind is required");
        assert_eq!(store.len().await.expect("len"), 0);
    }

    /// A batch is one transaction: the events land together, numbered on from
    /// the live head — and a batch that fails part-way leaves the stream
    /// exactly as it was, which is the whole reason it is one call.
    #[tokio::test]
    async fn append_many_is_one_transaction_that_lands_whole_or_not_at_all() {
        let (mut store, _drivers) = mem_store().await;
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
            "a failed batch wrote nothing"
        );
        assert_eq!(
            store.append(ev(5)).await.expect("append").seq,
            4,
            "no seq burnt"
        );

        // An empty batch is nothing to write, not an empty transaction.
        assert!(store
            .append_many(Vec::new())
            .await
            .expect("empty")
            .is_empty());
        assert_eq!(store.len().await.expect("len"), 4);
    }

    /// A two-stream write is one transaction: each side is numbered from its
    /// own head, both land together, and a `None` decision — or a malformed
    /// event on either side — leaves both streams exactly as they were.
    #[tokio::test]
    async fn append_if_many_writes_both_streams_or_neither() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        let mut parent = SqliteEventStore::open(&path, "p", &drivers)
            .await
            .expect("open the parent");
        let child = SqliteEventStore::open(&path, "c", &drivers)
            .await
            .expect("open the child");
        parent.append(ev(1)).await.expect("seed");

        let committed = parent
            .append_if_many(
                "c",
                None,
                Box::new(|events| {
                    assert_eq!(events.len(), 1, "the decision reads its own stream");
                    Some(Split {
                        own: vec![ev(2)],
                        other: vec![ev(3), ev(4)],
                    })
                }),
            )
            .await
            .expect("both sides")
            .expect("the decision wrote");
        assert_eq!(
            committed.own.iter().map(|c| c.seq).collect::<Vec<_>>(),
            [2],
            "this stream numbers on from its own head"
        );
        assert_eq!(
            committed.other.iter().map(|c| c.seq).collect::<Vec<_>>(),
            [1, 2],
            "and the other from its own, which was empty"
        );
        assert_eq!(child.len().await.expect("len"), 2, "the other side landed");

        // A `None` is a decision too: neither stream is touched.
        assert_eq!(
            parent
                .append_if_many("c", None, Box::new(|_| None))
                .await
                .expect("append_if_many"),
            None
        );
        assert_eq!(parent.len().await.expect("len"), 2);
        assert_eq!(child.len().await.expect("len"), 2);

        // One side may be empty — a refusal writes only this stream.
        parent
            .append_if_many("c", None, Box::new(|_| Some(Split::own(vec![ev(5)]))))
            .await
            .expect("append_if_many")
            .expect("the decision wrote");
        assert_eq!(parent.len().await.expect("len"), 3);
        assert_eq!(child.len().await.expect("len"), 2, "and nothing else");

        // A malformed event on the far side takes the whole transaction with
        // it, including the well-formed one on this side.
        parent
            .append_if_many(
                "c",
                None,
                Box::new(|_| {
                    Some(Split {
                        own: vec![ev(6)],
                        other: vec![obj(json!({ "text": "no kind" }))],
                    })
                }),
            )
            .await
            .expect_err("kind is required");
        assert_eq!(parent.len().await.expect("len"), 3, "nothing was written");
        assert_eq!(child.len().await.expect("len"), 2);
    }

    /// `database` names the database, not the stream: two stores on one file
    /// answer with the same string and a store on another file does not.
    /// That is the whole of what the identity is for — deciding whether one
    /// transaction can cover both.
    #[tokio::test]
    async fn database_is_the_same_for_two_streams_of_one_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let elsewhere = dir.path().join("other.db");
        let drivers = IsleDrivers::new();

        let a = SqliteEventStore::open(&path, "a", &drivers)
            .await
            .expect("open a");
        let b = SqliteEventStore::open(&path, "b", &drivers)
            .await
            .expect("open b");
        let far = SqliteEventStore::open(&elsewhere, "a", &drivers)
            .await
            .expect("open far");

        assert_eq!(a.database(), b.database(), "two streams, one database");
        assert_ne!(a.database(), far.database(), "two databases");
        assert_eq!(
            a.database(),
            Some(path.to_string_lossy().as_ref()),
            "the target it was opened by"
        );

        // An in-memory database has an identity too, and it is the URI a
        // second connection reaches it by.
        let (mem, _mem_drivers) = mem_store().await;
        let uri = mem.database().expect("a database").to_string();
        assert!(uri.contains("mode=memory"), "{uri}");
        let beside = SqliteEventStore::open(std::path::Path::new(&uri), "beside", &drivers)
            .await
            .expect("open beside");
        assert_eq!(
            beside.database(),
            Some(uri.as_str()),
            "opening that target reaches the same database"
        );
    }

    /// The child scan finds the streams that name this one as their parent
    /// and carry no ending — and nobody else's children, and not the ones
    /// that already closed.
    #[tokio::test]
    async fn open_children_are_the_unended_streams_that_name_this_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        /// A `session_opened` naming `parent`.
        fn opened(parent: &str) -> Map<String, Value> {
            obj(json!({
                "kind": "session_opened",
                "data": { "scope_id": "sc", "owner": "anon", "parent": parent }
            }))
        }
        let ended = obj(json!({ "kind": "session_closed", "data": { "reason": "done" } }));

        let mut parent = SqliteEventStore::open(&path, "p", &drivers)
            .await
            .expect("open p");
        // Still running.
        let mut running = SqliteEventStore::open(&path, "kid-a", &drivers)
            .await
            .expect("open kid-a");
        running.append(opened("p")).await.expect("opened");
        // Opened from p and already over.
        let mut over = SqliteEventStore::open(&path, "kid-b", &drivers)
            .await
            .expect("open kid-b");
        over.append(opened("p")).await.expect("opened");
        over.append(ended.clone()).await.expect("closed");
        // Somebody else's child, still running.
        let mut theirs = SqliteEventStore::open(&path, "kid-c", &drivers)
            .await
            .expect("open kid-c");
        theirs.append(opened("q")).await.expect("opened");
        // A stream with no parent at all.
        let mut root = SqliteEventStore::open(&path, "r", &drivers)
            .await
            .expect("open r");
        root.append(obj(
            json!({ "kind": "session_opened", "data": { "scope_id": "sc", "owner": "anon" } }),
        ))
        .await
        .expect("opened");

        let scan = ChildScan {
            opened: "session_opened".to_string(),
            closed: "session_closed".to_string(),
            parent_field: "parent".to_string(),
        };
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = Arc::clone(&seen);
        let committed = parent
            .append_with_open_children(
                &scan,
                Box::new(move |children| {
                    *recorded.lock().expect("not poisoned") = children;
                    ended.clone()
                }),
            )
            .await
            .expect("the close lands");

        assert_eq!(
            *seen.lock().expect("not poisoned"),
            ["kid-a"],
            "only the unended streams that named this one"
        );
        assert_eq!(committed.seq, 1, "and the event it built was appended");
        assert_eq!(parent.len().await.expect("len"), 1);
    }

    /// A kind-filtered read is answered off the index: only the kinds asked
    /// for come back, in `seq` order, still carrying the `seq` the stream gave
    /// them.  `None` is the whole stream, an empty selection is nothing.
    #[tokio::test]
    async fn read_kinds_selects_by_kind_and_keeps_the_streams_order() {
        let (mut store, _drivers) = mem_store().await;
        store
            .append(budget("budget_granted", 100))
            .await
            .expect("grant");
        store.append(ev(1)).await.expect("noise");
        store
            .append(budget("budget_spent", 10))
            .await
            .expect("spend");
        store.append(ev(2)).await.expect("more noise");

        let ledger = store
            .read_kinds(Some(&["budget_granted", "budget_spent"]), 0, usize::MAX)
            .await
            .expect("read_kinds");
        let kinds: Vec<&str> = ledger.iter().map(kind_of).collect();
        assert_eq!(kinds, ["budget_granted", "budget_spent"]);
        assert_eq!(seq_of(&ledger[0]), 1);
        assert_eq!(seq_of(&ledger[1]), 3, "the seq is the stream's");

        // from_seq and limit still apply to the filtered set.
        assert_eq!(
            store
                .read_kinds(Some(&["budget_granted"]), 2, usize::MAX)
                .await
                .expect("read_kinds")
                .len(),
            0
        );
        assert_eq!(
            store
                .read_kinds(Some(&["budget_granted", "budget_spent"]), 0, 1)
                .await
                .expect("read_kinds")
                .len(),
            1
        );

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
            4
        );
    }

    /// A decision that names its kinds is shown those and nothing else, and
    /// its write is still numbered against the whole stream — the filter is
    /// what the decision *reads*, not where its answer goes.
    #[tokio::test]
    async fn append_if_filters_the_decisions_input_and_numbers_against_the_stream() {
        let (mut store, _drivers) = mem_store().await;
        store
            .append(budget("budget_granted", 100))
            .await
            .expect("grant");
        store.append(ev(1)).await.expect("noise");
        store.append(ev(2)).await.expect("more noise");

        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = Arc::clone(&seen);
        let committed = store
            .append_if(
                Some(&["budget_granted"]),
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
            Some(4),
            "the write lands after everything, not after the filtered read"
        );
        assert_eq!(store.len().await.expect("len"), 4);
    }

    /// Two handles on one stream, one invariant: each decides inside its own
    /// transaction, so the second sees what the first wrote and exactly one
    /// of them may write.  This is the property a compare-and-swap against a
    /// cached head could only detect after the fact.
    #[tokio::test]
    async fn append_if_across_two_handles_decides_on_the_other_handles_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        let mut a = SqliteEventStore::open(&path, "s", &drivers)
            .await
            .expect("open a");
        let mut b = SqliteEventStore::open(&path, "s", &drivers)
            .await
            .expect("open b");

        // "Write the marker, but only if nobody has written one yet."
        let only_once = || {
            decide(|events: Vec<Value>| {
                (!events.iter().any(|e| kind_of(e) == "marker"))
                    .then(|| obj(json!({ "kind": "marker" })))
            })
        };

        let first = a.append_if(None, only_once()).await.expect("a decides");
        assert_eq!(first.map(|c| c.seq), Some(1), "a wrote the marker");

        let second = b.append_if(None, only_once()).await.expect("b decides");
        assert_eq!(second, None, "b saw a's marker and wrote nothing");
        assert_eq!(b.len().await.expect("len"), 1, "exactly one marker");
    }

    #[tokio::test]
    async fn read_pages_by_from_seq_and_limit() {
        let (mut store, _drivers) = mem_store().await;
        for i in 1..=5 {
            store.append(ev(i)).await.expect("append");
        }

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
        let (mut store, _drivers) = mem_store().await;
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

    /// A read rebuilds the object that was written: the envelope out of its
    /// columns, `meta` and `data` out of theirs, and the beat back as an
    /// absent key when there was none.
    #[tokio::test]
    async fn read_reconstructs_the_written_event_out_of_its_columns() {
        let (mut store, _drivers) = mem_store().await;
        store
            .append(obj(json!({
                "kind": "note",
                "beat": "b1",
                "meta": { "label": "a", "attempt": 2, "retried": true },
                "data": { "text": "hi", "nested": { "deep": [1, 2] } }
            })))
            .await
            .expect("append");
        store
            .append(obj(json!({ "kind": "note" })))
            .await
            .expect("append a bare one");

        let stored = store.read(0, usize::MAX).await.expect("read");
        assert_eq!(kind_of(&stored[0]), "note");
        assert_eq!(stored[0]["beat"], json!("b1"));
        assert_eq!(
            stored[0]["meta"],
            json!({ "label": "a", "attempt": 2, "retried": true })
        );
        assert_eq!(
            stored[0]["data"],
            json!({ "text": "hi", "nested": { "deep": [1, 2] } }),
            "data comes back at any depth"
        );
        assert_eq!(seq_of(&stored[0]), 1);
        assert!(stored[0].get("epoch_ms").is_some(), "{}", stored[0]);
        assert_eq!(
            stored[0].get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION)
        );

        // An event with nothing declared: no beat key at all, and the two
        // objects empty rather than missing.
        assert_eq!(stored[1].get("beat"), None, "{}", stored[1]);
        assert_eq!(stored[1]["meta"], json!({}));
        assert_eq!(stored[1]["data"], json!({}));
    }

    /// The beat lands in its own column — a plain `SELECT beat` sees it —
    /// and the index that makes a by-beat read a range is on the table.
    #[tokio::test]
    async fn the_beat_is_a_column_of_its_own_with_an_index() {
        let (mut store, _drivers) = mem_store().await;
        store
            .append(obj(json!({ "kind": "e1", "beat": "b1" })))
            .await
            .expect("append");
        store.append(ev(2)).await.expect("append with no beat");

        let rows = ask(
            &store,
            "SELECT seq, beat FROM events WHERE stream = $stream ORDER BY seq",
        )
        .await
        .expect("query");
        assert_eq!(rows.rows[0]["beat"], Value::from("b1"));
        assert!(
            !rows.rows[1].contains_key("beat"),
            "an undeclared beat is NULL: {:?}",
            rows.rows[1]
        );

        // Grouping a run by beat is a range of an index, not a scan.
        let indexes = store
            .writer
            .call(|conn| {
                let mut stmt = conn.prepare("PRAGMA index_list(events)")?;
                let names = stmt
                    .query_map([], |row| row.get::<_, String>("name"))?
                    .collect::<rusqlite::Result<Vec<_>>>();
                names
            })
            .await
            .expect("index_list");
        assert!(
            indexes.iter().any(|name| name == "events_stream_beat_seq"),
            "the (stream, beat, seq) index must exist: {indexes:?}"
        );
        assert!(
            indexes.iter().any(|name| name == "events_stream_kind_seq"),
            "…beside the by-kind one: {indexes:?}"
        );
    }

    #[tokio::test]
    async fn events_persist_across_a_reopen_of_the_same_path_and_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        {
            // Its own collection, shut down at the end of the block, so the
            // first connection is drained and joined before the reopen below
            // — the same "the store is gone" the sync version got from Drop.
            let drivers = IsleDrivers::new();
            let mut store = SqliteEventStore::open(&path, "s", &drivers)
                .await
                .expect("open");
            store
                .append(obj(
                    json!({ "kind": "note", "data": { "text": "durable" } }),
                ))
                .await
                .expect("append note");
            store.append(ev(2)).await.expect("append e2");
            drop(store);
            assert!(drivers.shutdown().await.is_empty(), "the writer joined");
        }

        // Reopening the same file and stream reads the same events back: the
        // durability payoff.
        let drivers = IsleDrivers::new();
        let store = SqliteEventStore::open(&path, "s", &drivers)
            .await
            .expect("reopen");
        let events = store.read(0, usize::MAX).await.expect("read");
        assert_eq!(events.len(), 2);
        assert_eq!(kind_of(&events[0]), "note");
        assert_eq!(events[0]["data"], json!({ "text": "durable" }));
        assert_eq!(seq_of(&events[0]), 1);
        assert_eq!(
            events[0].get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION),
            "the schema version survives the round-trip too"
        );
        assert_eq!(store.head().await.expect("head"), Some(2));
    }

    #[tokio::test]
    async fn two_streams_in_one_db_file_do_not_see_each_others_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        let mut a = SqliteEventStore::open(&path, "stream-a", &drivers)
            .await
            .expect("open a");
        let mut b = SqliteEventStore::open(&path, "stream-b", &drivers)
            .await
            .expect("open b");

        a.append(obj(json!({ "kind": "only_a" })))
            .await
            .expect("append a");
        b.append(obj(json!({ "kind": "only_b1" })))
            .await
            .expect("append b1");
        b.append(obj(json!({ "kind": "only_b2" })))
            .await
            .expect("append b2");

        assert_eq!(a.len().await.expect("len"), 1);
        assert_eq!(b.len().await.expect("len"), 2);
        // Each stream numbers its own seq from 1, independent of the other.
        assert_eq!(a.head().await.expect("head"), Some(1));
        assert_eq!(b.head().await.expect("head"), Some(2));

        assert_eq!(
            kind_of(&a.read(0, usize::MAX).await.expect("read")[0]),
            "only_a"
        );
        let b_events = b.read(0, usize::MAX).await.expect("read");
        let b_kinds: Vec<&str> = b_events.iter().map(kind_of).collect();
        assert_eq!(b_kinds, ["only_b1", "only_b2"]);
    }

    /// (Fix 2) A row whose stored objects will not decode is corruption:
    /// `read` surfaces it as an error rather than silently dropping the row
    /// (which would let a resume re-fold a truncated log into the wrong
    /// state).  Both JSON columns are checked, and a scalar where an object
    /// was written is the same fault as text that will not parse.
    #[tokio::test]
    async fn read_errors_on_a_corrupt_row_instead_of_dropping_it() {
        for (seq, meta, data, column) in [
            (2_i64, "{}", "{not valid json", "data"),
            (3_i64, "not valid json either", "{}", "meta"),
            (4_i64, "{}", "7", "data"),
        ] {
            let (mut store, _drivers) = mem_store().await;
            store.append(ev(1)).await.expect("append");

            // Sneak in a row the store itself could not have written.
            let stream = store.stream.clone();
            let (meta, data) = (meta.to_string(), data.to_string());
            store
                .writer
                .call(move |conn| {
                    conn.execute(
                        "INSERT INTO events \
                         (stream, seq, epoch_ms, kind, schema_version, beat, meta, data) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            stream,
                            seq,
                            0_i64,
                            "note",
                            1_i64,
                            None::<String>,
                            meta,
                            data
                        ],
                    )
                })
                .await
                .expect("insert corrupt row");

            let err = store
                .read(0, usize::MAX)
                .await
                .expect_err("a corrupt row must surface, not be dropped");
            assert!(
                err.reason().contains(&format!("corrupt event {column}")),
                "{}",
                err.reason()
            );
            // Corruption, not storage: the IO worked and the bytes came back,
            // so what is wrong is the data — no retry and no reconnect
            // changes it.
            assert_eq!(err.kind(), KnlError::CORRUPTION);
            assert!(!err.is_retryable());
        }
    }

    /// The backend's error language is translated in exactly one place, and
    /// the split is the one a caller can act on: a contended lock says "ask
    /// again", every other fault says nothing of the kind.
    #[test]
    fn a_contended_lock_is_busy_and_every_other_fault_is_storage() {
        /// A `rusqlite` failure carrying `code`.
        fn failure(code: rusqlite::ErrorCode) -> rusqlite::Error {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code,
                    extended_code: 0,
                },
                Some("under test".to_string()),
            )
        }

        for code in [
            rusqlite::ErrorCode::DatabaseBusy,
            rusqlite::ErrorCode::DatabaseLocked,
        ] {
            let error = KnlError::from(failure(code));
            assert_eq!(error.kind(), KnlError::BUSY, "{code:?}: {error}");
            assert!(error.is_retryable(), "{code:?}: {error}");
        }

        for code in [
            rusqlite::ErrorCode::DatabaseCorrupt,
            rusqlite::ErrorCode::ReadOnly,
            rusqlite::ErrorCode::DiskFull,
        ] {
            let error = KnlError::from(failure(code));
            assert_eq!(error.kind(), KnlError::STORAGE, "{code:?}: {error}");
            assert!(
                !error.is_retryable(),
                "the kernel does not promise a retry it cannot back: {error}"
            );
        }

        // A non-SQLite rusqlite fault is storage too — it is the store
        // failing to do the work, whatever the shape of the failure.
        let error = KnlError::from(rusqlite::Error::QueryReturnedNoRows);
        assert_eq!(error.kind(), KnlError::STORAGE, "{error}");
    }

    /// The busy classification is what a real contended write surfaces as,
    /// not only what the translation function returns in isolation: a second
    /// connection holds the write lock, so the retries are exhausted and the
    /// error the caller gets says "ask again".
    #[tokio::test]
    async fn a_write_that_stays_contended_surfaces_as_busy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        let mut store = SqliteEventStore::open(&path, "s", &drivers)
            .await
            .expect("open");
        store.append(ev(1)).await.expect("seed");

        // A blocker holding an EXCLUSIVE transaction: every attempt this
        // store makes finds the database locked, and the busy_timeout is cut
        // to nothing so the test does not wait it out five times over.
        let blocker = Connection::open(&path).expect("open blocker");
        blocker
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("take the write lock");
        store
            .writer
            .call(|conn| conn.busy_timeout(Duration::from_millis(0)))
            .await
            .expect("no waiting");

        let err = store
            .append(ev(2))
            .await
            .expect_err("a write against a held lock must not succeed");
        assert_eq!(err.kind(), KnlError::BUSY, "{err}");
        assert!(err.is_retryable(), "{err}");
    }

    /// Two handles on one stream both write: an append records a fact, so it
    /// is serialized and assigned the next seq rather than refused for the
    /// head one of them last saw.
    #[tokio::test]
    async fn two_handles_on_one_stream_both_append_in_arrival_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        let mut a = SqliteEventStore::open(&path, "s", &drivers)
            .await
            .expect("open a");
        let mut b = SqliteEventStore::open(&path, "s", &drivers)
            .await
            .expect("open b");

        a.append(ev(1)).await.expect("seed"); // both handles now see head 1

        // A writes, then B writes — neither is refused, and the log holds
        // them in the order they arrived.
        assert_eq!(a.append(ev(2)).await.expect("a appends").seq, 2);
        assert_eq!(b.append(ev(3)).await.expect("b appends").seq, 3);

        let events = b.read(0, usize::MAX).await.expect("read");
        let kinds: Vec<&str> = events.iter().map(kind_of).collect();
        assert_eq!(kinds, ["e1", "e2", "e3"]);
        assert_eq!(b.head().await.expect("head"), Some(3));
    }

    /// (Fix 3) Under the IMMEDIATE transaction + `busy_timeout`, interleaved
    /// single-threaded appends across two handles on one stream serialize and
    /// round-trip cleanly.
    #[tokio::test]
    async fn immediate_tx_appends_round_trip_across_two_handles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        let mut a = SqliteEventStore::open(&path, "s", &drivers)
            .await
            .expect("open a");
        let mut b = SqliteEventStore::open(&path, "s", &drivers)
            .await
            .expect("open b");

        assert_eq!(a.append(ev(1)).await.expect("a1").seq, 1);
        assert_eq!(b.append(ev(2)).await.expect("b2").seq, 2);
        assert_eq!(a.append(ev(3)).await.expect("a3").seq, 3);

        assert_eq!(b.head().await.expect("head"), Some(3));
        assert_eq!(b.read(0, usize::MAX).await.expect("read").len(), 3);
    }

    // -- the read side -----------------------------------------------------

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
        let plan = crate::knl::query::plan(sql, params, opts, &store.stream)?;
        store.query(&plan).await
    }

    /// The `kind` column of every row, in order.
    fn kinds_of(rows: &QueryRows) -> Vec<&str> {
        rows.rows
            .iter()
            .map(|row| row["kind"].as_str().expect("kind is a string"))
            .collect()
    }

    /// The reader sees what the writer wrote — on an in-memory database as
    /// much as on a file, which is the whole reason the memory one is opened
    /// under a shared-cache URI rather than as a private `:memory:`.
    #[tokio::test]
    async fn the_reader_sees_the_writers_rows_in_memory() {
        let (mut store, _drivers) = mem_store().await;
        store.append(ev(1)).await.expect("append e1");
        store.append(ev(2)).await.expect("append e2");

        let rows = ask(
            &store,
            "SELECT seq, kind FROM events WHERE stream = $stream ORDER BY seq",
        )
        .await
        .expect("query");
        assert_eq!(kinds_of(&rows), ["e1", "e2"]);
        assert_eq!(rows.rows[0]["seq"], Value::from(1));
        assert!(!rows.truncated);

        // A write after the first query is visible to the next one: the
        // reader is a live connection, not a snapshot taken when it opened.
        store.append(ev(3)).await.expect("append e3");
        let again = ask(&store, "SELECT kind FROM events ORDER BY seq")
            .await
            .expect("query");
        assert_eq!(kinds_of(&again), ["e1", "e2", "e3"]);
    }

    /// `$stream` is this store's own stream and nothing else: a second stream
    /// in the same database is not selected by it.
    #[tokio::test]
    async fn stream_binds_to_this_stores_own_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        let mut a = SqliteEventStore::open(&path, "stream-a", &drivers)
            .await
            .expect("open a");
        let mut b = SqliteEventStore::open(&path, "stream-b", &drivers)
            .await
            .expect("open b");
        a.append(obj(json!({ "kind": "only_a" }))).await.expect("a");
        b.append(obj(json!({ "kind": "only_b" }))).await.expect("b");

        let rows = ask(&a, "SELECT kind FROM events WHERE stream = $stream")
            .await
            .expect("query");
        assert_eq!(kinds_of(&rows), ["only_a"]);
        let rows = ask(&b, "SELECT kind FROM events WHERE stream = $stream")
            .await
            .expect("query");
        assert_eq!(kinds_of(&rows), ["only_b"]);
    }

    /// `$sessions` reads across a set: two streams in one database, one
    /// statement, and the ids are bound rather than pasted in.
    #[tokio::test]
    async fn sessions_reads_across_the_set_it_was_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");
        let drivers = IsleDrivers::new();

        let mut a = SqliteEventStore::open(&path, "stream-a", &drivers)
            .await
            .expect("open a");
        let mut b = SqliteEventStore::open(&path, "stream-b", &drivers)
            .await
            .expect("open b");
        a.append(obj(json!({ "kind": "from_a" }))).await.expect("a");
        b.append(obj(json!({ "kind": "from_b1" })))
            .await
            .expect("b1");
        b.append(obj(json!({ "kind": "from_b2" })))
            .await
            .expect("b2");

        let opts = QueryOpts {
            sessions: Some(vec!["stream-a".to_string(), "stream-b".to_string()]),
            ..QueryOpts::default()
        };
        let rows = ask_with(
            &a,
            "SELECT stream, kind FROM events WHERE stream IN $sessions ORDER BY stream, seq",
            QueryParams::None,
            &opts,
        )
        .await
        .expect("query");
        assert_eq!(kinds_of(&rows), ["from_a", "from_b1", "from_b2"]);

        // Left out, the set is the asking store's own stream.
        let rows = ask(&a, "SELECT kind FROM events WHERE stream IN $sessions")
            .await
            .expect("query");
        assert_eq!(kinds_of(&rows), ["from_a"]);
    }

    /// A value is bound, never pasted: a quote inside it is a character in a
    /// string, not the end of one.
    #[tokio::test]
    async fn a_bound_value_with_a_quote_in_it_is_a_value() {
        let (mut store, _drivers) = mem_store().await;
        store
            .append(obj(json!({ "kind": "it's a kind" })))
            .await
            .expect("append");
        store.append(ev(1)).await.expect("append e1");

        let rows = ask_with(
            &store,
            "SELECT kind FROM events WHERE kind = ?",
            QueryParams::Positional(vec![json!("it's a kind")]),
            &QueryOpts::default(),
        )
        .await
        .expect("query");
        assert_eq!(kinds_of(&rows), ["it's a kind"]);

        // The same by name, and a value that would be SQL if it were pasted
        // in matches nothing rather than doing anything.
        let named = QueryParams::Named(
            json!({ "kind": "x' OR 1=1 --" })
                .as_object()
                .expect("an object")
                .clone(),
        );
        let rows = ask_with(
            &store,
            "SELECT kind FROM events WHERE kind = :kind",
            named,
            &QueryOpts::default(),
        )
        .await
        .expect("query");
        assert!(rows.rows.is_empty(), "{:?}", rows.rows);
    }

    /// The cap is reported, not silently applied — and a result that happens
    /// to be exactly `limit` long is not called truncated.
    #[tokio::test]
    async fn the_row_cap_is_reported_when_it_cuts() {
        let (mut store, _drivers) = mem_store().await;
        for i in 1..=5 {
            store.append(ev(i)).await.expect("append");
        }

        let capped = QueryOpts {
            limit: 2,
            ..QueryOpts::default()
        };
        let rows = ask_with(
            &store,
            "SELECT kind FROM events ORDER BY seq",
            QueryParams::None,
            &capped,
        )
        .await
        .expect("query");
        assert_eq!(kinds_of(&rows), ["e1", "e2"]);
        assert!(rows.truncated, "the cap cut three rows off");

        let exact = QueryOpts {
            limit: 5,
            ..QueryOpts::default()
        };
        let rows = ask_with(
            &store,
            "SELECT kind FROM events ORDER BY seq",
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
        let (store, _drivers) = mem_store().await;
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

    /// The reader cannot write.  The statement checks run on the text, but
    /// they are not the only thing standing between a caller and the log:
    /// the connection a query runs on has no write capability at all.
    #[tokio::test]
    async fn the_reader_connection_refuses_a_write() {
        let (mut store, _drivers) = mem_store().await;
        store.append(ev(1)).await.expect("append");
        let reader = store.reader().await.expect("open the reader");

        let err = reader
            .call(|conn| {
                conn.execute(
                    "INSERT INTO events \
                     (stream, seq, epoch_ms, kind, schema_version, beat, meta, data) \
                     VALUES ('x', 1, 0, 'note', 1, NULL, '{}', '{}')",
                    [],
                )
            })
            .await
            .expect_err("the reader must not be able to write");
        assert!(
            matches!(KnlError::from(err), KnlError::Storage(_)),
            "a write through the reader is refused by SQLite itself"
        );

        // …and the log is as it was.
        assert_eq!(store.len().await.expect("len"), 1);
    }

    /// A statement that is not a read never reaches the connection, and a
    /// second statement is refused whole.  (The rules are
    /// [`super::super::query`]'s; this is the path through the store.)
    #[tokio::test]
    async fn a_write_or_a_second_statement_is_refused_before_the_connection() {
        let (store, _drivers) = mem_store().await;
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

    /// A parameter nobody answered, and a value nobody asked for, are both
    /// errors: a silent NULL is how a query quietly stops meaning what it
    /// says.
    #[tokio::test]
    async fn every_parameter_is_answered_and_every_value_is_used() {
        let (store, _drivers) = mem_store().await;

        let err = ask(&store, "SELECT * FROM events WHERE kind = :kind")
            .await
            .expect_err("an unanswered parameter must be refused");
        assert_eq!(err.kind(), KnlError::VALIDATION, "{err}");
        assert!(err.reason().contains(":kind"), "{}", err.reason());

        let err = ask_with(
            &store,
            "SELECT * FROM events WHERE kind = ?",
            QueryParams::Positional(vec![json!("a"), json!("b")]),
            &QueryOpts::default(),
        )
        .await
        .expect_err("a value with no parameter must be refused");
        assert_eq!(err.kind(), KnlError::VALIDATION, "{err}");
    }

    /// Every SQLite type comes back as itself, and a NULL comes back as an
    /// absent column rather than a present nothing.
    #[tokio::test]
    async fn the_sqlite_types_map_onto_values_and_null_is_absence() {
        let (store, _drivers) = mem_store().await;
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
        assert_eq!(row["raw"], Value::from("bytes"));
        assert!(
            !row.contains_key("absent"),
            "a NULL column is absent, so it reads as nil: {row:?}"
        );
    }

    /// The published schema is the table: read back off SQLite rather than
    /// written out, with the two columns that make a stream a stream as its
    /// primary key.
    #[tokio::test]
    async fn the_published_schema_is_the_events_table() {
        let columns = events_schema().expect("schema");
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "stream",
                "seq",
                "epoch_ms",
                "kind",
                "schema_version",
                "beat",
                "meta",
                "data"
            ]
        );

        let pk: Vec<&str> = columns
            .iter()
            .filter(|c| c.pk)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(pk, ["stream", "seq"], "the log is keyed by (stream, seq)");

        let declared: Vec<&str> = columns.iter().map(|c| c.declared_type.as_str()).collect();
        assert_eq!(
            declared,
            ["TEXT", "INTEGER", "INTEGER", "TEXT", "INTEGER", "TEXT", "TEXT", "TEXT"]
        );

        // And a query may name every one of them.
        let (store, _drivers) = mem_store().await;
        let sql = format!("SELECT {} FROM {EVENTS_TABLE}", names.join(", "));
        ask(&store, &sql)
            .await
            .expect("the published columns are the real ones");
    }

    /// The published schema is also the *live* one: a store's own reader
    /// reports the same columns the schema-only path does, which is what
    /// makes reading it off a throwaway connection sound.
    #[tokio::test]
    async fn a_live_store_reports_the_published_schema() {
        let (store, _drivers) = mem_store().await;
        assert_eq!(
            store.schema().await.expect("schema"),
            events_schema().expect("published schema")
        );
    }
}
