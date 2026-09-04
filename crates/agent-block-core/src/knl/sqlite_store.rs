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
//! # Concurrency
//!
//! `append` and `append_if` read-then-write, so each runs in an `IMMEDIATE`
//! transaction: the `RESERVED` lock is taken at `BEGIN` rather than promoted
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
//! order — and a decision taken by `append_if` runs against the stream inside
//! the same transaction that records its answer, so no concurrent writer can
//! slip between the two.
//!
//! [`MemEventStore`]: super::event_store::MemEventStore

use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::{Map, Value};

use super::event::{seq_of, stamp, validate_event, FIELD_KIND};
use super::event_store::{
    stamp_schema_version, Committed, Decision, EventStore, CURRENT_SCHEMA_VERSION,
    SCHEMA_VERSION_FIELD,
};
use super::{now_ms, KnlError, KnlResult};

/// How long a contended write waits for the lock before erroring.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// The open connection to the DB file (or an in-memory database).
    conn: Connection,
    /// The stream this store is scoped to — the session id.
    stream: String,
}

impl SqliteEventStore {
    /// Open (creating if absent) the DB at `path`, scoped to `stream`.
    ///
    /// The `events` table is created if it does not exist, so opening a fresh
    /// file and reopening an existing one take the same path.
    pub fn open(path: &Path, stream: impl Into<String>) -> KnlResult<Self> {
        let conn = Connection::open(path).map_err(sqlite_err)?;
        Self::init(conn, stream.into())
    }

    /// An in-memory database scoped to `stream` (for tests and ephemera).
    ///
    /// Nothing persists past the returned value's lifetime — this is *not*
    /// the durable path, only a backend that behaves like the file one.
    pub fn open_in_memory(stream: impl Into<String>) -> KnlResult<Self> {
        let conn = Connection::open_in_memory().map_err(sqlite_err)?;
        Self::init(conn, stream.into())
    }

    /// Set the busy timeout and ensure the table, then wrap the connection.
    fn init(conn: Connection, stream: String) -> KnlResult<Self> {
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sqlite_err)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events ( \
                 stream         TEXT    NOT NULL, \
                 seq            INTEGER NOT NULL, \
                 epoch_ms       INTEGER NOT NULL, \
                 kind           TEXT    NOT NULL, \
                 schema_version INTEGER NOT NULL, \
                 payload        TEXT    NOT NULL, \
                 PRIMARY KEY (stream, seq) \
             );",
        )
        .map_err(sqlite_err)?;
        Ok(Self { conn, stream })
    }
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

    fn append_if(&mut self, decide: &mut Decision<'_>) -> KnlResult<Option<Committed>> {
        // The read, the decision and the insert share one IMMEDIATE
        // transaction, so the invariant `decide` checks holds at the instant
        // the event lands.  A contended attempt is retried whole — `decide` is
        // a pure fold over the events it is handed, so running it again on the
        // freshly read stream is the correct thing to do.
        run_with_retry(|| append_if_attempt(&mut self.conn, &self.stream, &mut *decide))
    }

    fn read(&self, from_seq: u64, limit: usize) -> KnlResult<Vec<Value>> {
        // `usize::MAX` (an unbounded read) caps at i64::MAX, which SQLite
        // treats as "no limit"; `0` reads nothing.
        let capped = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT payload FROM events \
                 WHERE stream = ?1 AND seq >= ?2 ORDER BY seq ASC LIMIT ?3",
            )
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map(params![self.stream, from_seq as i64, capped], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sqlite_err)?;
        // A read that cannot prepare/query is a fault, and a row whose payload
        // does not decode is corruption — both surface as an error rather than
        // being silently dropped, so a caller (resume) never re-folds a
        // truncated log into the wrong state.
        let mut events = Vec::new();
        for row in rows {
            let payload = row.map_err(sqlite_err)?;
            let value = serde_json::from_str::<Value>(&payload)
                .map_err(|e| KnlError::new(format!("sqlite: corrupt event payload: {e}")))?;
            events.push(value);
        }
        Ok(events)
    }

    fn head(&self) -> KnlResult<Option<u64>> {
        // A transient busy read must surface, not read as "empty": a caller
        // deciding open-vs-resume (or a CAS) on a swallowed error would
        // treat a populated stream as fresh.  Same discipline as read().
        head_in(&self.conn, &self.stream).map_err(sqlite_err)
    }

    fn len(&self) -> KnlResult<usize> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE stream = ?1",
                params![self.stream],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .map_err(sqlite_err)
    }
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
            Err(TxError::Sqlite(error)) => return Err(sqlite_err(error)),
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

/// One `IMMEDIATE` decide-then-append: read the stream, ask `decide` what to
/// record, and insert its answer in the same transaction.
///
/// The whole stream is read to decide, which is acceptable while streams are
/// small; the future optimisation is a snapshot plus the events after it.
///
/// A `None` decision commits nothing — the transaction is dropped, so the
/// stream is exactly as it was — and reports `Ok(None)`.
fn append_if_attempt(
    conn: &mut Connection,
    stream: &str,
    decide: &mut Decision<'_>,
) -> Result<Option<Committed>, TxError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(TxError::Sqlite)?;
    let events = read_in(&tx, stream)?;
    let Some(event) = decide(&events) else {
        // Nothing to write: the transaction is rolled back on drop.
        return Ok(None);
    };
    // The decision's event is validated like any other: a malformed one is
    // refused and the transaction goes no further.
    validate_event(&event).map_err(TxError::Terminal)?;
    let head = events.last().map(seq_of);
    let seq = head.unwrap_or(0).saturating_add(1);
    let epoch_ms = now_ms();
    let mut row = event.clone();
    stamp_schema_version(&mut row);
    stamp(&mut row, seq, epoch_ms);
    insert_row(&tx, stream, seq, epoch_ms, &row)?;
    tx.commit().map_err(TxError::Sqlite)?;
    Ok(Some(Committed { seq, epoch_ms }))
}

/// Every event of `stream`, in `seq` order, read inside a transaction.
///
/// The in-transaction twin of [`EventStore::read`]: a decode failure is
/// corruption and terminal, a fault on the read itself is retryable.
fn read_in(conn: &Connection, stream: &str) -> Result<Vec<Value>, TxError> {
    let mut stmt = conn
        .prepare("SELECT payload FROM events WHERE stream = ?1 ORDER BY seq ASC")
        .map_err(TxError::Sqlite)?;
    let rows = stmt
        .query_map(params![stream], |row| row.get::<_, String>(0))
        .map_err(TxError::Sqlite)?;
    let mut events = Vec::new();
    for row in rows {
        let payload = row.map_err(TxError::Sqlite)?;
        let value = serde_json::from_str::<Value>(&payload).map_err(|e| {
            TxError::Terminal(KnlError::new(format!("sqlite: corrupt event payload: {e}")))
        })?;
        events.push(value);
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

/// Insert the fully-stamped event; `payload` is the whole event object, so a
/// read reconstructs the exact same `Value` the in-memory store returns. An
/// encode failure is terminal; a contended insert is retryable.
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
    let payload = serde_json::to_string(&Value::Object(event.clone()))
        .map_err(|e| TxError::Terminal(KnlError::new(format!("sqlite: encode event: {e}"))))?;
    conn.execute(
        "INSERT INTO events (stream, seq, epoch_ms, kind, schema_version, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            stream,
            seq as i64,
            epoch_ms as i64,
            kind,
            schema_version as i64,
            payload
        ],
    )
    .map_err(TxError::Sqlite)?;
    Ok(())
}

/// Render a rusqlite error as a [`KnlError`], noting the busy/locked codes.
///
/// A full retryable-error taxonomy is out of scope here; the message flags a
/// contended lock (the connection's `busy_timeout` already makes writes wait
/// rather than fail) so a caller can tell it apart from a real fault.
fn sqlite_err(error: rusqlite::Error) -> KnlError {
    if let rusqlite::Error::SqliteFailure(inner, _) = &error {
        if matches!(
            inner.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ) {
            return KnlError::new(format!("sqlite: busy/locked (retryable): {error}"));
        }
    }
    KnlError::new(format!("sqlite: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::{kind_of, seq_of};
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
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
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
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
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
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
        store.append(ev(1)).expect("seed");

        let mut seen_kinds: Vec<String> = Vec::new();
        let committed = store
            .append_if(&mut |events| {
                seen_kinds = events.iter().map(|e| kind_of(e).to_string()).collect();
                Some(ev(2))
            })
            .expect("append_if");
        assert_eq!(seen_kinds, ["e1"], "decide saw the durable stream");
        assert_eq!(committed.map(|c| c.seq), Some(2));

        let nothing = store.append_if(&mut |_| None).expect("append_if");
        assert_eq!(nothing, None);
        assert_eq!(store.len().expect("len"), 2, "a None commits nothing");
        assert_eq!(store.append(ev(3)).expect("append").seq, 3);
    }

    /// A malformed decision is refused and leaves the stream alone.
    #[test]
    fn append_if_validates_the_event_the_decision_returns() {
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
        store
            .append_if(&mut |_| Some(obj(json!({ "text": "no kind" }))))
            .expect_err("kind is required");
        assert_eq!(store.len().expect("len"), 0);
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

        let first = a.append_if(&mut only_once_a).expect("a decides");
        assert_eq!(first.map(|c| c.seq), Some(1), "a wrote the marker");

        let second = b.append_if(&mut only_once_b).expect("b decides");
        assert_eq!(second, None, "b saw a's marker and wrote nothing");
        assert_eq!(b.len().expect("len"), 1, "exactly one marker");
    }

    #[test]
    fn read_pages_by_from_seq_and_limit() {
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
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
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
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
    fn read_reconstructs_the_stored_envelope_including_the_schema_version() {
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
        store
            .append(obj(json!({ "kind": "note", "text": "hi" })))
            .expect("append");
        let stored = store.read(0, usize::MAX).expect("read");
        assert_eq!(kind_of(&stored[0]), "note");
        assert_eq!(stored[0]["text"], json!("hi"));
        assert_eq!(seq_of(&stored[0]), 1);
        assert_eq!(
            stored[0].get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION)
        );
    }

    #[test]
    fn events_persist_across_a_reopen_of_the_same_path_and_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        {
            let mut store = SqliteEventStore::open(&path, "s").expect("open");
            store
                .append(obj(json!({ "kind": "note", "text": "durable" })))
                .expect("append note");
            store.append(ev(2)).expect("append e2");
        } // the store — and its connection — is dropped here.

        // Reopening the same file and stream reads the same events back: the
        // durability payoff.
        let store = SqliteEventStore::open(&path, "s").expect("reopen");
        let events = store.read(0, usize::MAX).expect("read");
        assert_eq!(events.len(), 2);
        assert_eq!(kind_of(&events[0]), "note");
        assert_eq!(events[0]["text"], json!("durable"));
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

    /// (Fix 2) A row whose payload is not valid JSON is corruption: `read`
    /// surfaces it as an error rather than silently dropping the row (which
    /// would let a resume re-fold a truncated log into the wrong state).
    #[test]
    fn read_errors_on_a_corrupt_payload_row_instead_of_dropping_it() {
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
        store.append(ev(1)).expect("append");

        // Sneak in a row whose payload cannot decode — a corrupt log entry.
        store
            .conn
            .execute(
                "INSERT INTO events (stream, seq, epoch_ms, kind, schema_version, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!["s", 2_i64, 0_i64, "note", 1_i64, "{not valid json"],
            )
            .expect("insert corrupt row");

        let err = store
            .read(0, usize::MAX)
            .expect_err("a corrupt payload must surface, not be dropped");
        assert!(
            err.reason().contains("corrupt event payload"),
            "{}",
            err.reason()
        );
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
}
