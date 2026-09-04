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
//! A query runs under a deadline: a watchdog interrupts the connection if the
//! statement has not finished in time, and the interrupt surfaces as
//! [`KnlError::Timeout`].
//!
//! # Concurrency
//!
//! `append`, `append_many` and `append_if` read-then-write, so each runs in an
//! `IMMEDIATE` transaction: the `RESERVED` lock is taken at `BEGIN` rather than promoted
//! from `SHARED` on the first write, which is the point `busy_timeout`
//! actually covers — a `DEFERRED` transaction can still hit `SQLITE_BUSY` on
//! lock *promotion* even with a timeout set. On top of the timeout, a
//! contended `BEGIN`/insert/commit is retried a bounded number of times when
//! SQLite reports a retryable code (`SQLITE_BUSY` / `SQLITE_LOCKED`, matched
//! on the error code, not the message). If every attempt is still contended
//! the write surfaces as a busy/locked [`KnlError`] rather than looping
//! forever.
//!
//! That is what makes the SPI's promise true here: appends to one stream are
//! *serialized* — two handles both write and the log interleaves in arrival
//! order — a batch is one transaction, so it lands whole or not at all, and a
//! decision taken by `append_if` runs against the stream inside the same
//! transaction that records its answer, so no concurrent writer can slip
//! between the two.
//!
//! [`MemEventStore`]: super::event_store::MemEventStore

use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{params, params_from_iter, Connection, OpenFlags, TransactionBehavior};
use serde_json::{Map, Value};

use super::event::{
    stamp, validate_event, FIELD_BEAT, FIELD_DATA, FIELD_EPOCH_MS, FIELD_KIND, FIELD_META,
    FIELD_SEQ,
};
use super::event_store::{
    stamp_schema_version, Committed, Decision, EventStore, CURRENT_SCHEMA_VERSION,
    SCHEMA_VERSION_FIELD,
};
use super::query::{session_slot, QueryParams, QueryPlan, QueryRows, STREAM_PARAM};
use super::{now_ms, KnlError, KnlResult};

/// How long a contended write waits for the lock before erroring.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// The URI an in-memory database for `stream` is addressed by.
    ///
    /// Derived from the stream id, so reopening the same stream in the same
    /// process finds the same database — which is what makes an in-memory
    /// session resumable while it is still alive.
    fn memory_uri(stream: &str) -> String {
        format!("file:knl-{stream}?mode=memory&cache=shared")
    }

    /// Open a connection with `flags`.
    fn open(&self, flags: OpenFlags) -> rusqlite::Result<Connection> {
        // `SQLITE_OPEN_URI` is what makes the `file:` form a URI rather than a
        // relative path called "file:…"; it is in `OpenFlags::default()` and
        // added explicitly for the read-only flags built below.
        match self {
            Self::File(path) => Connection::open_with_flags(path, flags),
            Self::Memory(uri) => {
                Connection::open_with_flags(uri, flags | OpenFlags::SQLITE_OPEN_URI)
            }
        }
    }

    /// The flags a read-only connection is opened with.
    fn read_only_flags() -> OpenFlags {
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI
    }
}

/// How many times a write is retried when SQLite reports a retryable code
/// (`SQLITE_BUSY` / `SQLITE_LOCKED`) before the busy/locked error surfaces.
const MAX_TX_ATTEMPTS: u32 = 5;

/// A single transactional attempt's failure: a retryable SQLite fault, or a
/// terminal error (a rejected event, a corrupt row, an encode failure) that
/// no retry can fix.
enum TxError {
    /// A rusqlite fault; retried when its code is busy/locked.
    Sqlite(rusqlite::Error),
    /// A terminal kernel error — never retried.
    Terminal(KnlError),
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
    /// The write connection to the database.
    ///
    /// Held for the store's whole life, which for an in-memory database is
    /// not merely convenient: a shared-cache in-memory database exists only
    /// while a connection to it is open, so this handle *is* the database.
    conn: Connection,
    /// Where the database is, so a second connection can be opened to it.
    db: Db,
    /// The read-only connection, opened on the first query and reused.
    ///
    /// Lazy because most sessions never run one: a store that only appends
    /// and folds pays nothing for the read side existing.
    reader: OnceCell<Connection>,
    /// The stream this store is scoped to — the session id.
    stream: String,
}

impl SqliteEventStore {
    /// Open (creating if absent) the DB at `path`, scoped to `stream`.
    ///
    /// The `events` table is created if it does not exist, so opening a fresh
    /// file and reopening an existing one take the same path.
    pub fn open(path: &Path, stream: impl Into<String>) -> KnlResult<Self> {
        Self::init(Db::File(path.to_path_buf()), stream.into())
    }

    /// Open an in-memory database for `stream`.
    ///
    /// The database is named after the stream and opened in shared-cache
    /// mode, so the read connection reaches the same rows the writer wrote —
    /// and so reopening the same stream id in the same process finds the same
    /// log.  It exists only while this store does: an in-memory database is
    /// reclaimed when its last connection closes, which is exactly what
    /// "ephemeral" should mean.
    pub fn open_memory(stream: impl Into<String>) -> KnlResult<Self> {
        let stream = stream.into();
        Self::init(Db::Memory(Db::memory_uri(&stream)), stream)
    }

    /// Open the writer, set the busy timeout, ensure the table and its index.
    fn init(db: Db, stream: String) -> KnlResult<Self> {
        let conn = db.open(OpenFlags::default())?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch(SCHEMA_DDL)?;
        Ok(Self {
            conn,
            db,
            reader: OnceCell::new(),
            stream,
        })
    }

    /// The read-only connection, opened on first use.
    ///
    /// A *second* connection to the same database, with no write capability:
    /// `SQLITE_OPEN_READ_ONLY` is what SQLite was asked for, and
    /// `query_only` is the same answer said again inside the connection, so a
    /// statement that slipped past the checks on the text still has nothing
    /// to write with.
    fn reader(&self) -> KnlResult<&Connection> {
        if let Some(reader) = self.reader.get() {
            return Ok(reader);
        }
        let reader = self.db.open(Db::read_only_flags())?;
        reader.busy_timeout(BUSY_TIMEOUT)?;
        reader.execute_batch("PRAGMA query_only = 1;")?;
        // `set` cannot fail here — nothing else can have filled the cell,
        // since `&self` is not shared across threads — and the value is
        // fetched back rather than moved out so the connection stays owned by
        // the cell for every later query.
        let _ = self.reader.set(reader);
        Ok(self
            .reader
            .get()
            .expect("the reader was just placed in the cell"))
    }

    /// The columns of the `events` table, as SQLite reports them.
    ///
    /// Read through the *reader*, because this is the read contract: what a
    /// caller's SQL may name. `PRAGMA table_info` rather than a list written
    /// out here, so the published schema cannot drift from the table.
    pub fn schema(&self) -> KnlResult<Vec<SchemaColumn>> {
        let reader = self.reader()?;
        let mut stmt = reader.prepare(&format!("PRAGMA table_info({EVENTS_TABLE})"))?;
        let rows = stmt.query_map([], |row| {
            Ok(SchemaColumn {
                name: row.get::<_, String>("name")?,
                declared_type: row.get::<_, String>("type")?,
                pk: row.get::<_, i64>("pk")? > 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(KnlError::from)
    }
}

/// The columns of the `events` table, without a session to ask.
///
/// The schema is a property of the kernel, not of any one log, so this opens
/// a throwaway in-memory store and reads the table back off it — the same
/// `PRAGMA table_info` a caller's own store would answer with.  It is what
/// `knl.api()` publishes.
pub fn events_schema() -> KnlResult<Vec<SchemaColumn>> {
    SqliteEventStore::open_memory(format!("schema-{}", uuid::Uuid::new_v4()))?.schema()
}

impl EventStore for SqliteEventStore {
    fn append(&mut self, mut event: Map<String, Value>) -> KnlResult<Committed> {
        // Reject before touching the stream: a rejected event burns no seq.
        validate_event(&event)?;
        // Stamp the schema version once, before the retry loop; the
        // kernel-owned seq / epoch_ms are stamped per attempt inside the
        // transaction, recomputed from the live head each time.
        stamp_schema_version(&mut event);
        run_with_retry(|| append_attempt(&mut self.conn, &self.stream, &event))
    }

    fn append_many(&mut self, events: Vec<Map<String, Value>>) -> KnlResult<Vec<Committed>> {
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
        run_with_retry(|| append_many_attempt(&mut self.conn, &self.stream, &events))
    }

    fn append_if(
        &mut self,
        kinds: Option<&[&str]>,
        decide: &mut Decision<'_>,
    ) -> KnlResult<Option<Committed>> {
        // The read, the decision and the insert share one IMMEDIATE
        // transaction, so the invariant `decide` checks holds at the instant
        // the event lands.  A contended attempt is retried whole — `decide` is
        // a pure fold over the events it is handed, so running it again on the
        // freshly read stream is the correct thing to do.
        run_with_retry(|| append_if_attempt(&mut self.conn, &self.stream, kinds, &mut *decide))
    }

    fn read_kinds(
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
        let (sql, args) = read_query(&self.stream, kinds, from_seq, capped);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), read_row)?;
        // A read that cannot prepare/query is a fault, and a row whose stored
        // objects do not decode is corruption — both surface as an error
        // rather than being silently dropped, so a caller (resume) never
        // re-folds a truncated log into the wrong state.
        let mut events = Vec::new();
        for row in rows {
            events.push(event_of(row?)?);
        }
        Ok(events)
    }

    fn head(&self) -> KnlResult<Option<u64>> {
        // A transient busy read must surface, not read as "empty": a caller
        // deciding open-vs-resume (or a CAS) on a swallowed error would
        // treat a populated stream as fresh.  Same discipline as read().
        head_in(&self.conn, &self.stream).map_err(KnlError::from)
    }

    fn len(&self) -> KnlResult<usize> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE stream = ?1",
                params![self.stream],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .map_err(KnlError::from)
    }

    fn query(&self, plan: &QueryPlan) -> KnlResult<QueryRows> {
        run_query(self.reader()?, plan)
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

/// The watchdog that ends a query that is taking too long.
///
/// SQLite has no per-statement timeout — `busy_timeout` bounds waiting for a
/// *lock*, which is a different thing from a statement that is simply
/// expensive — so a deadline has to come from outside: a thread that waits,
/// and interrupts the connection if the query has not said it is done.
/// (`Connection::progress_handler`, which would do this in-thread, is behind
/// rusqlite's `hooks` feature and this build does not enable it.)
///
/// The wait ends either way: the query signals completion by dropping the
/// sender, which wakes the thread immediately, so the interrupt only ever
/// fires while the statement it belongs to is still running.
struct Deadline {
    /// Dropped when the query finishes; that is the signal.
    done: Option<mpsc::Sender<()>>,
    /// The waiting thread, joined on drop so no watchdog outlives its query.
    watchdog: Option<JoinHandle<()>>,
}

impl Deadline {
    /// Start watching `conn` for `timeout`.
    fn arm(conn: &Connection, timeout: Duration) -> Self {
        let interrupt = conn.get_interrupt_handle();
        let (done, finished) = mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            // A disconnect means the query finished first: nothing to do.
            if finished.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
                interrupt.interrupt();
            }
        });
        Self {
            done: Some(done),
            watchdog: Some(watchdog),
        }
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        // Dropping the sender wakes the watchdog at once; joining it means the
        // interrupt cannot land on whatever this connection does next.
        drop(self.done.take());
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

/// Prepare, check, bind and run one query.
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

    let _deadline = Deadline::arm(conn, plan.timeout);
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

/// Run one transactional `attempt`, retrying up to [`MAX_TX_ATTEMPTS`] times
/// while SQLite reports a retryable lock code, then surfacing the busy/locked
/// error rather than looping forever. A terminal error is returned at once.
fn run_with_retry<T, F>(mut attempt: F) -> KnlResult<T>
where
    F: FnMut() -> Result<T, TxError>,
{
    let mut tries = 1;
    loop {
        match attempt() {
            Ok(committed) => return Ok(committed),
            Err(TxError::Sqlite(error)) if is_retryable(&error) && tries < MAX_TX_ATTEMPTS => {
                tries += 1;
            }
            Err(TxError::Sqlite(error)) => return Err(error.into()),
            Err(TxError::Terminal(error)) => return Err(error),
        }
    }
}

/// One `IMMEDIATE` append: take the reserved lock up front, compute the next
/// `seq` from the live head, stamp and insert, then commit.
fn append_attempt(
    conn: &mut Connection,
    stream: &str,
    event: &Map<String, Value>,
) -> Result<Committed, TxError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(TxError::Sqlite)?;
    let seq = next_seq(&tx, stream).map_err(TxError::Sqlite)?;
    let epoch_ms = now_ms();
    let mut row = event.clone();
    stamp(&mut row, seq, epoch_ms);
    insert_row(&tx, stream, seq, epoch_ms, &row)?;
    tx.commit().map_err(TxError::Sqlite)?;
    Ok(Committed { seq, epoch_ms })
}

/// One `IMMEDIATE` batch append: take the reserved lock up front, number the
/// events on from the live head, and insert them all before committing.
///
/// All or nothing: an event that will not encode, or a contended insert
/// part-way through, drops the transaction and leaves the stream exactly as
/// it was — which is what lets a caller write two facts that are one fact.
fn append_many_attempt(
    conn: &mut Connection,
    stream: &str,
    events: &[Map<String, Value>],
) -> Result<Vec<Committed>, TxError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(TxError::Sqlite)?;
    let mut seq = next_seq(&tx, stream).map_err(TxError::Sqlite)?;
    let mut committed = Vec::with_capacity(events.len());
    for event in events {
        let epoch_ms = now_ms();
        let mut row = event.clone();
        stamp_schema_version(&mut row);
        stamp(&mut row, seq, epoch_ms);
        insert_row(&tx, stream, seq, epoch_ms, &row)?;
        committed.push(Committed { seq, epoch_ms });
        seq = seq.saturating_add(1);
    }
    tx.commit().map_err(TxError::Sqlite)?;
    Ok(committed)
}

/// One `IMMEDIATE` decide-then-append: read the stream, ask `decide` what to
/// record, and insert its answer in the same transaction.
///
/// `kinds` narrows what the decision is shown, not where its answer lands:
/// the new event's `seq` comes from the stream's live head, so a filtered
/// decision numbers its write against everything, exactly as an ordinary
/// append does.
///
/// A `None` decision commits nothing — the transaction is dropped, so the
/// stream is exactly as it was — and reports `Ok(None)`.
fn append_if_attempt(
    conn: &mut Connection,
    stream: &str,
    kinds: Option<&[&str]>,
    decide: &mut Decision<'_>,
) -> Result<Option<Committed>, TxError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(TxError::Sqlite)?;
    let events = read_in(&tx, stream, kinds)?;
    let Some(event) = decide(&events) else {
        // Nothing to write: the transaction is rolled back on drop.
        return Ok(None);
    };
    // The decision's event is validated like any other: a malformed one is
    // refused and the transaction goes no further.
    validate_event(&event).map_err(TxError::Terminal)?;
    // The head of the whole stream, not of the events the decision was shown:
    // a filtered read says nothing about where the next event goes.
    let seq = next_seq(&tx, stream).map_err(TxError::Sqlite)?;
    let epoch_ms = now_ms();
    let mut row = event.clone();
    stamp_schema_version(&mut row);
    stamp(&mut row, seq, epoch_ms);
    insert_row(&tx, stream, seq, epoch_ms, &row)?;
    tx.commit().map_err(TxError::Sqlite)?;
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
    kinds: Option<&[&str]>,
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
        args.extend(kinds.iter().map(|kind| SqlValue::Text((*kind).to_string())));
    }
    sql.push_str(" ORDER BY seq ASC LIMIT ?");
    args.push(SqlValue::Integer(limit));
    (sql, args)
}

/// The events of `stream` a decision is shown, in `seq` order, read inside a
/// transaction.
///
/// The in-transaction twin of [`EventStore::read_kinds`]: a decode failure is
/// corruption and terminal, a fault on the read itself is retryable.
fn read_in(conn: &Connection, stream: &str, kinds: Option<&[&str]>) -> Result<Vec<Value>, TxError> {
    // An empty selection selects nothing, and `kind IN ()` is not SQL.
    if kinds.is_some_and(<[&str]>::is_empty) {
        return Ok(Vec::new());
    }
    let (sql, args) = read_query(stream, kinds, 0, i64::MAX);
    let mut stmt = conn.prepare(&sql).map_err(TxError::Sqlite)?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), read_row)
        .map_err(TxError::Sqlite)?;
    let mut events = Vec::new();
    for row in rows {
        let row = row.map_err(TxError::Sqlite)?;
        events.push(event_of(row).map_err(TxError::Terminal)?);
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
) -> Result<(), TxError> {
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
    .map_err(TxError::Sqlite)?;
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
fn encode_object(value: Option<&Value>, column: &str) -> Result<String, TxError> {
    let Some(value) = value else {
        return Ok("{}".to_string());
    };
    serde_json::to_string(value).map_err(|e| {
        TxError::Terminal(KnlError::Storage(format!(
            "sqlite: encode event {column}: {e}"
        )))
    })
}

/// Classify a rusqlite error into the kernel's vocabulary.
///
/// This is the one place the backend's error language is translated, and the
/// split is the one the caller can act on: a contended lock is
/// [`KnlError::Busy`] — the same call may succeed if it is made again, which
/// is exactly what [`run_with_retry`] does with it — and everything else is
/// [`KnlError::Storage`], a fault the kernel cannot promise anything about.
/// Matched on the SQLite error *code*, never the message text, so the
/// classification does not drift with a library's wording.
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

    /// A store on an in-memory database of its very own.
    ///
    /// The name matters: an in-memory database is shared by *name*, which is
    /// what lets the reader see the writer's rows — and would equally let two
    /// tests running in parallel see each other's.  A fresh id per store keeps
    /// each test's log to itself.
    fn mem_store() -> SqliteEventStore {
        SqliteEventStore::open_memory(uuid::Uuid::new_v4().to_string()).expect("open")
    }

    #[test]
    fn append_assigns_gap_free_monotonic_seq_from_one() {
        let mut store = mem_store();
        assert!(store.is_empty().expect("is_empty"));
        assert_eq!(store.len().expect("len"), 0);

        let a = store.append(ev(1)).expect("append e1");
        let b = store.append(ev(2)).expect("append e2");
        let c = store.append(ev(3)).expect("append e3");

        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
        assert_eq!(store.len().expect("len"), 3);
        assert!(!store.is_empty().expect("is_empty"));

        // The stamped epoch is what is stored.
        let stored = store.read(0, usize::MAX).expect("read");
        let stored_epoch = stored[0]
            .get("epoch_ms")
            .and_then(Value::as_u64)
            .expect("epoch is on the stored event");
        assert_eq!(stored_epoch, a.epoch_ms);
    }

    #[test]
    fn a_rejected_append_records_nothing_and_burns_no_seq() {
        let mut store = mem_store();
        store
            .append(obj(json!({ "text": "no kind" })))
            .expect_err("kind is required");
        assert_eq!(store.len().expect("len"), 0);
        assert_eq!(store.append(ev(1)).expect("append").seq, 1);
    }

    /// `append_if` decides on the stream inside its transaction: the events
    /// it is handed are the durable ones, a `Some` lands at the next seq, and
    /// a `None` commits nothing.
    #[test]
    fn append_if_decides_inside_the_transaction_and_writes_only_a_some() {
        let mut store = mem_store();
        store.append(ev(1)).expect("seed");

        let mut seen_kinds: Vec<String> = Vec::new();
        let committed = store
            .append_if(None, &mut |events| {
                seen_kinds = events.iter().map(|e| kind_of(e).to_string()).collect();
                Some(ev(2))
            })
            .expect("append_if");
        assert_eq!(seen_kinds, ["e1"], "decide saw the durable stream");
        assert_eq!(committed.map(|c| c.seq), Some(2));

        let nothing = store.append_if(None, &mut |_| None).expect("append_if");
        assert_eq!(nothing, None);
        assert_eq!(store.len().expect("len"), 2, "a None commits nothing");
        assert_eq!(store.append(ev(3)).expect("append").seq, 3);
    }

    /// A malformed decision is refused and leaves the stream alone.
    #[test]
    fn append_if_validates_the_event_the_decision_returns() {
        let mut store = mem_store();
        store
            .append_if(None, &mut |_| Some(obj(json!({ "text": "no kind" }))))
            .expect_err("kind is required");
        assert_eq!(store.len().expect("len"), 0);
    }

    /// A batch is one transaction: the events land together, numbered on from
    /// the live head — and a batch that fails part-way leaves the stream
    /// exactly as it was, which is the whole reason it is one call.
    #[test]
    fn append_many_is_one_transaction_that_lands_whole_or_not_at_all() {
        let mut store = mem_store();
        store.append(ev(1)).expect("seed");

        let committed = store.append_many(vec![ev(2), ev(3)]).expect("the batch");
        assert_eq!(
            committed.iter().map(|c| c.seq).collect::<Vec<_>>(),
            [2, 3],
            "numbered on from the head that was there"
        );
        let stored = store.read(0, usize::MAX).expect("read");
        let kinds: Vec<&str> = stored.iter().map(kind_of).collect();
        assert_eq!(kinds, ["e1", "e2", "e3"]);

        // A malformed event refuses the whole batch, and the one before it in
        // the same call is not in the log either.
        store
            .append_many(vec![ev(4), obj(json!({ "text": "no kind" }))])
            .expect_err("kind is required");
        assert_eq!(store.len().expect("len"), 3, "a failed batch wrote nothing");
        assert_eq!(store.append(ev(5)).expect("append").seq, 4, "no seq burnt");

        // An empty batch is nothing to write, not an empty transaction.
        assert!(store.append_many(Vec::new()).expect("empty").is_empty());
        assert_eq!(store.len().expect("len"), 4);
    }

    /// A kind-filtered read is answered off the index: only the kinds asked
    /// for come back, in `seq` order, still carrying the `seq` the stream gave
    /// them.  `None` is the whole stream, an empty selection is nothing.
    #[test]
    fn read_kinds_selects_by_kind_and_keeps_the_streams_order() {
        let mut store = mem_store();
        store.append(budget("budget_granted", 100)).expect("grant");
        store.append(ev(1)).expect("noise");
        store.append(budget("budget_spent", 10)).expect("spend");
        store.append(ev(2)).expect("more noise");

        let ledger = store
            .read_kinds(Some(&["budget_granted", "budget_spent"]), 0, usize::MAX)
            .expect("read_kinds");
        let kinds: Vec<&str> = ledger.iter().map(kind_of).collect();
        assert_eq!(kinds, ["budget_granted", "budget_spent"]);
        assert_eq!(seq_of(&ledger[0]), 1);
        assert_eq!(seq_of(&ledger[1]), 3, "the seq is the stream's");

        // from_seq and limit still apply to the filtered set.
        assert_eq!(
            store
                .read_kinds(Some(&["budget_granted"]), 2, usize::MAX)
                .expect("read_kinds")
                .len(),
            0
        );
        assert_eq!(
            store
                .read_kinds(Some(&["budget_granted", "budget_spent"]), 0, 1)
                .expect("read_kinds")
                .len(),
            1
        );

        assert!(store
            .read_kinds(Some(&[]), 0, usize::MAX)
            .expect("read_kinds")
            .is_empty());
        assert_eq!(
            store
                .read_kinds(None, 0, usize::MAX)
                .expect("read_kinds")
                .len(),
            4
        );
    }

    /// A decision that names its kinds is shown those and nothing else, and
    /// its write is still numbered against the whole stream — the filter is
    /// what the decision *reads*, not where its answer goes.
    #[test]
    fn append_if_filters_the_decisions_input_and_numbers_against_the_stream() {
        let mut store = mem_store();
        store.append(budget("budget_granted", 100)).expect("grant");
        store.append(ev(1)).expect("noise");
        store.append(ev(2)).expect("more noise");

        let mut seen: Vec<String> = Vec::new();
        let committed = store
            .append_if(Some(&["budget_granted"]), &mut |events| {
                seen = events.iter().map(|e| kind_of(e).to_string()).collect();
                Some(budget("budget_spent", 10))
            })
            .expect("append_if");
        assert_eq!(seen, ["budget_granted"], "only the kinds asked for");
        assert_eq!(
            committed.map(|c| c.seq),
            Some(4),
            "the write lands after everything, not after the filtered read"
        );
        assert_eq!(store.len().expect("len"), 4);
    }

    /// Two handles on one stream, one invariant: each decides inside its own
    /// transaction, so the second sees what the first wrote and exactly one
    /// of them may write.  This is the property a compare-and-swap against a
    /// cached head could only detect after the fact.
    #[test]
    fn append_if_across_two_handles_decides_on_the_other_handles_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        let mut a = SqliteEventStore::open(&path, "s").expect("open a");
        let mut b = SqliteEventStore::open(&path, "s").expect("open b");

        // "Write the marker, but only if nobody has written one yet."
        let mut only_once_a = |events: &[Value]| {
            (!events.iter().any(|e| kind_of(e) == "marker"))
                .then(|| obj(json!({ "kind": "marker" })))
        };
        let mut only_once_b = |events: &[Value]| {
            (!events.iter().any(|e| kind_of(e) == "marker"))
                .then(|| obj(json!({ "kind": "marker" })))
        };

        let first = a.append_if(None, &mut only_once_a).expect("a decides");
        assert_eq!(first.map(|c| c.seq), Some(1), "a wrote the marker");

        let second = b.append_if(None, &mut only_once_b).expect("b decides");
        assert_eq!(second, None, "b saw a's marker and wrote nothing");
        assert_eq!(b.len().expect("len"), 1, "exactly one marker");
    }

    #[test]
    fn read_pages_by_from_seq_and_limit() {
        let mut store = mem_store();
        for i in 1..=5 {
            store.append(ev(i)).expect("append");
        }

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
        let mut store = mem_store();
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

    /// A read rebuilds the object that was written: the envelope out of its
    /// columns, `meta` and `data` out of theirs, and the beat back as an
    /// absent key when there was none.
    #[test]
    fn read_reconstructs_the_written_event_out_of_its_columns() {
        let mut store = mem_store();
        store
            .append(obj(json!({
                "kind": "note",
                "beat": "b1",
                "meta": { "label": "a", "attempt": 2, "retried": true },
                "data": { "text": "hi", "nested": { "deep": [1, 2] } }
            })))
            .expect("append");
        store
            .append(obj(json!({ "kind": "note" })))
            .expect("append a bare one");

        let stored = store.read(0, usize::MAX).expect("read");
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
    #[test]
    fn the_beat_is_a_column_of_its_own_with_an_index() {
        let mut store = mem_store();
        store
            .append(obj(json!({ "kind": "e1", "beat": "b1" })))
            .expect("append");
        store.append(ev(2)).expect("append with no beat");

        let rows = ask(
            &store,
            "SELECT seq, beat FROM events WHERE stream = $stream ORDER BY seq",
        )
        .expect("query");
        assert_eq!(rows.rows[0]["beat"], Value::from("b1"));
        assert!(
            !rows.rows[1].contains_key("beat"),
            "an undeclared beat is NULL: {:?}",
            rows.rows[1]
        );

        // Grouping a run by beat is a range of an index, not a scan.
        let indexes = store
            .conn
            .prepare("PRAGMA index_list(events)")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get::<_, String>("name"))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
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

    #[test]
    fn events_persist_across_a_reopen_of_the_same_path_and_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        {
            let mut store = SqliteEventStore::open(&path, "s").expect("open");
            store
                .append(obj(
                    json!({ "kind": "note", "data": { "text": "durable" } }),
                ))
                .expect("append note");
            store.append(ev(2)).expect("append e2");
        } // the store — and its connection — is dropped here.

        // Reopening the same file and stream reads the same events back: the
        // durability payoff.
        let store = SqliteEventStore::open(&path, "s").expect("reopen");
        let events = store.read(0, usize::MAX).expect("read");
        assert_eq!(events.len(), 2);
        assert_eq!(kind_of(&events[0]), "note");
        assert_eq!(events[0]["data"], json!({ "text": "durable" }));
        assert_eq!(seq_of(&events[0]), 1);
        assert_eq!(
            events[0].get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION),
            "the schema version survives the round-trip too"
        );
        assert_eq!(store.head().expect("head"), Some(2));
    }

    #[test]
    fn two_streams_in_one_db_file_do_not_see_each_others_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        let mut a = SqliteEventStore::open(&path, "stream-a").expect("open a");
        let mut b = SqliteEventStore::open(&path, "stream-b").expect("open b");

        a.append(obj(json!({ "kind": "only_a" })))
            .expect("append a");
        b.append(obj(json!({ "kind": "only_b1" })))
            .expect("append b1");
        b.append(obj(json!({ "kind": "only_b2" })))
            .expect("append b2");

        assert_eq!(a.len().expect("len"), 1);
        assert_eq!(b.len().expect("len"), 2);
        // Each stream numbers its own seq from 1, independent of the other.
        assert_eq!(a.head().expect("head"), Some(1));
        assert_eq!(b.head().expect("head"), Some(2));

        assert_eq!(kind_of(&a.read(0, usize::MAX).expect("read")[0]), "only_a");
        let b_events = b.read(0, usize::MAX).expect("read");
        let b_kinds: Vec<&str> = b_events.iter().map(kind_of).collect();
        assert_eq!(b_kinds, ["only_b1", "only_b2"]);
    }

    /// (Fix 2) A row whose stored objects will not decode is corruption:
    /// `read` surfaces it as an error rather than silently dropping the row
    /// (which would let a resume re-fold a truncated log into the wrong
    /// state).  Both JSON columns are checked, and a scalar where an object
    /// was written is the same fault as text that will not parse.
    #[test]
    fn read_errors_on_a_corrupt_row_instead_of_dropping_it() {
        for (seq, meta, data, column) in [
            (2_i64, "{}", "{not valid json", "data"),
            (3_i64, "not valid json either", "{}", "meta"),
            (4_i64, "{}", "7", "data"),
        ] {
            let mut store = mem_store();
            store.append(ev(1)).expect("append");

            // Sneak in a row the store itself could not have written.
            let stream = store.stream.clone();
            store
                .conn
                .execute(
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
                .expect("insert corrupt row");

            let err = store
                .read(0, usize::MAX)
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
    #[test]
    fn a_write_that_stays_contended_surfaces_as_busy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        let mut store = SqliteEventStore::open(&path, "s").expect("open");
        store.append(ev(1)).expect("seed");

        // A blocker holding an EXCLUSIVE transaction: every attempt this
        // store makes finds the database locked, and the busy_timeout is cut
        // to nothing so the test does not wait it out five times over.
        let blocker = Connection::open(&path).expect("open blocker");
        blocker
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("take the write lock");
        store
            .conn
            .busy_timeout(Duration::from_millis(0))
            .expect("no waiting");

        let err = store
            .append(ev(2))
            .expect_err("a write against a held lock must not succeed");
        assert_eq!(err.kind(), KnlError::BUSY, "{err}");
        assert!(err.is_retryable(), "{err}");
    }

    /// Two handles on one stream both write: an append records a fact, so it
    /// is serialized and assigned the next seq rather than refused for the
    /// head one of them last saw.
    #[test]
    fn two_handles_on_one_stream_both_append_in_arrival_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        let mut a = SqliteEventStore::open(&path, "s").expect("open a");
        let mut b = SqliteEventStore::open(&path, "s").expect("open b");

        a.append(ev(1)).expect("seed"); // both handles now see head 1

        // A writes, then B writes — neither is refused, and the log holds
        // them in the order they arrived.
        assert_eq!(a.append(ev(2)).expect("a appends").seq, 2);
        assert_eq!(b.append(ev(3)).expect("b appends").seq, 3);

        let events = b.read(0, usize::MAX).expect("read");
        let kinds: Vec<&str> = events.iter().map(kind_of).collect();
        assert_eq!(kinds, ["e1", "e2", "e3"]);
        assert_eq!(b.head().expect("head"), Some(3));
    }

    /// (Fix 3) Under the IMMEDIATE transaction + `busy_timeout`, interleaved
    /// single-threaded appends across two handles on one stream serialize and
    /// round-trip cleanly.
    #[test]
    fn immediate_tx_appends_round_trip_across_two_handles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        let mut a = SqliteEventStore::open(&path, "s").expect("open a");
        let mut b = SqliteEventStore::open(&path, "s").expect("open b");

        assert_eq!(a.append(ev(1)).expect("a1").seq, 1);
        assert_eq!(b.append(ev(2)).expect("b2").seq, 2);
        assert_eq!(a.append(ev(3)).expect("a3").seq, 3);

        assert_eq!(b.head().expect("head"), Some(3));
        assert_eq!(b.read(0, usize::MAX).expect("read").len(), 3);
    }

    // -- the read side -----------------------------------------------------

    /// Ask `store` for `sql` with everything default.
    fn ask(store: &SqliteEventStore, sql: &str) -> KnlResult<QueryRows> {
        ask_with(store, sql, QueryParams::None, &QueryOpts::default())
    }

    /// Ask `store` for `sql`, saying how.
    fn ask_with(
        store: &SqliteEventStore,
        sql: &str,
        params: QueryParams,
        opts: &QueryOpts,
    ) -> KnlResult<QueryRows> {
        let plan = crate::knl::query::plan(sql, params, opts, &store.stream)?;
        store.query(&plan)
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
    #[test]
    fn the_reader_sees_the_writers_rows_in_memory() {
        let mut store = mem_store();
        store.append(ev(1)).expect("append e1");
        store.append(ev(2)).expect("append e2");

        let rows = ask(
            &store,
            "SELECT seq, kind FROM events WHERE stream = $stream ORDER BY seq",
        )
        .expect("query");
        assert_eq!(kinds_of(&rows), ["e1", "e2"]);
        assert_eq!(rows.rows[0]["seq"], Value::from(1));
        assert!(!rows.truncated);

        // A write after the first query is visible to the next one: the
        // reader is a live connection, not a snapshot taken when it opened.
        store.append(ev(3)).expect("append e3");
        assert_eq!(
            kinds_of(&ask(&store, "SELECT kind FROM events ORDER BY seq").expect("query")),
            ["e1", "e2", "e3"]
        );
    }

    /// `$stream` is this store's own stream and nothing else: a second stream
    /// in the same database is not selected by it.
    #[test]
    fn stream_binds_to_this_stores_own_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        let mut a = SqliteEventStore::open(&path, "stream-a").expect("open a");
        let mut b = SqliteEventStore::open(&path, "stream-b").expect("open b");
        a.append(obj(json!({ "kind": "only_a" }))).expect("a");
        b.append(obj(json!({ "kind": "only_b" }))).expect("b");

        let rows = ask(&a, "SELECT kind FROM events WHERE stream = $stream").expect("query");
        assert_eq!(kinds_of(&rows), ["only_a"]);
        let rows = ask(&b, "SELECT kind FROM events WHERE stream = $stream").expect("query");
        assert_eq!(kinds_of(&rows), ["only_b"]);
    }

    /// `$sessions` reads across a set: two streams in one database, one
    /// statement, and the ids are bound rather than pasted in.
    #[test]
    fn sessions_reads_across_the_set_it_was_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        let mut a = SqliteEventStore::open(&path, "stream-a").expect("open a");
        let mut b = SqliteEventStore::open(&path, "stream-b").expect("open b");
        a.append(obj(json!({ "kind": "from_a" }))).expect("a");
        b.append(obj(json!({ "kind": "from_b1" }))).expect("b1");
        b.append(obj(json!({ "kind": "from_b2" }))).expect("b2");

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
        .expect("query");
        assert_eq!(kinds_of(&rows), ["from_a", "from_b1", "from_b2"]);

        // Left out, the set is the asking store's own stream.
        let rows = ask(&a, "SELECT kind FROM events WHERE stream IN $sessions").expect("query");
        assert_eq!(kinds_of(&rows), ["from_a"]);
    }

    /// A value is bound, never pasted: a quote inside it is a character in a
    /// string, not the end of one.
    #[test]
    fn a_bound_value_with_a_quote_in_it_is_a_value() {
        let mut store = mem_store();
        store
            .append(obj(json!({ "kind": "it's a kind" })))
            .expect("append");
        store.append(ev(1)).expect("append e1");

        let rows = ask_with(
            &store,
            "SELECT kind FROM events WHERE kind = ?",
            QueryParams::Positional(vec![json!("it's a kind")]),
            &QueryOpts::default(),
        )
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
        .expect("query");
        assert!(rows.rows.is_empty(), "{:?}", rows.rows);
    }

    /// The cap is reported, not silently applied — and a result that happens
    /// to be exactly `limit` long is not called truncated.
    #[test]
    fn the_row_cap_is_reported_when_it_cuts() {
        let mut store = mem_store();
        for i in 1..=5 {
            store.append(ev(i)).expect("append");
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
        .expect("query");
        assert_eq!(rows.rows.len(), 5);
        assert!(!rows.truncated, "nothing was cut off");
    }

    /// A query that will not finish is cut short, and says so in its own
    /// class: nothing was contended, so "ask again" would be the wrong advice.
    #[test]
    fn a_query_that_runs_too_long_is_a_timeout() {
        let store = mem_store();
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
        .expect_err("an endless query must be cut short");
        assert_eq!(err.kind(), KnlError::TIMEOUT, "{err}");
        assert!(!err.is_retryable(), "a slow query is not a retry: {err}");

        // The connection is usable afterwards: the interrupt ended a
        // statement, not the reader.
        assert!(ask(&store, "SELECT 1 AS one").is_ok());
    }

    /// The reader cannot write.  The statement checks run on the text, but
    /// they are not the only thing standing between a caller and the log:
    /// the connection a query runs on has no write capability at all.
    #[test]
    fn the_reader_connection_refuses_a_write() {
        let mut store = mem_store();
        store.append(ev(1)).expect("append");
        let reader = store.reader().expect("open the reader");

        let err = reader
            .execute(
                "INSERT INTO events \
                 (stream, seq, epoch_ms, kind, schema_version, beat, meta, data) \
                 VALUES ('x', 1, 0, 'note', 1, NULL, '{}', '{}')",
                [],
            )
            .expect_err("the reader must not be able to write");
        assert!(
            matches!(KnlError::from(err), KnlError::Storage(_)),
            "a write through the reader is refused by SQLite itself"
        );

        // …and the log is as it was.
        assert_eq!(store.len().expect("len"), 1);
    }

    /// A statement that is not a read never reaches the connection, and a
    /// second statement is refused whole.  (The rules are
    /// [`super::super::query`]'s; this is the path through the store.)
    #[test]
    fn a_write_or_a_second_statement_is_refused_before_the_connection() {
        let store = mem_store();
        for sql in [
            "INSERT INTO events (stream) VALUES ('x')",
            "UPDATE events SET kind = 'x'",
            "PRAGMA table_info(events)",
            "ATTACH DATABASE '/tmp/other.db' AS other",
            "SELECT 1; DROP TABLE events",
        ] {
            let err = ask(&store, sql).expect_err("must be refused");
            assert_eq!(err.kind(), KnlError::VALIDATION, "{sql:?}: {err}");
        }
    }

    /// A parameter nobody answered, and a value nobody asked for, are both
    /// errors: a silent NULL is how a query quietly stops meaning what it
    /// says.
    #[test]
    fn every_parameter_is_answered_and_every_value_is_used() {
        let store = mem_store();

        let err = ask(&store, "SELECT * FROM events WHERE kind = :kind")
            .expect_err("an unanswered parameter must be refused");
        assert_eq!(err.kind(), KnlError::VALIDATION, "{err}");
        assert!(err.reason().contains(":kind"), "{}", err.reason());

        let err = ask_with(
            &store,
            "SELECT * FROM events WHERE kind = ?",
            QueryParams::Positional(vec![json!("a"), json!("b")]),
            &QueryOpts::default(),
        )
        .expect_err("a value with no parameter must be refused");
        assert_eq!(err.kind(), KnlError::VALIDATION, "{err}");
    }

    /// Every SQLite type comes back as itself, and a NULL comes back as an
    /// absent column rather than a present nothing.
    #[test]
    fn the_sqlite_types_map_onto_values_and_null_is_absence() {
        let store = mem_store();
        let rows = ask(
            &store,
            // `absent`, not `nothing`: NOTHING is a SQLite keyword.
            "SELECT 1 AS whole, 1.5 AS fraction, 'text' AS words, NULL AS absent, \
             CAST('bytes' AS BLOB) AS raw",
        )
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
    #[test]
    fn the_published_schema_is_the_events_table() {
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
        let store = mem_store();
        let sql = format!("SELECT {} FROM {EVENTS_TABLE}", names.join(", "));
        ask(&store, &sql).expect("the published columns are the real ones");
    }
}
