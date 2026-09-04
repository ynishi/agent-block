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
//! `append_if_head` reads the head and inserts inside one transaction, so a
//! compare-and-swap is correct even when two writers race — SQLite's file
//! lock serialises them.  A `busy_timeout` is set on the connection so a
//! contended write waits rather than failing outright; a wait that times out
//! surfaces as a busy/locked [`KnlError`].
//!
//! [`MemEventStore`]: super::event_store::MemEventStore

use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection};
use serde_json::{Map, Value};

use super::event::{stamp, validate_event, FIELD_KIND};
use super::event_store::{
    stamp_schema_version, Committed, EventStore, CURRENT_SCHEMA_VERSION, SCHEMA_VERSION_FIELD,
};
use super::{now_ms, KnlError, KnlResult};

/// How long a contended write waits for the lock before erroring.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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
        let tx = self.conn.transaction().map_err(sqlite_err)?;
        let seq = next_seq(&tx, &self.stream)?;
        let epoch_ms = now_ms();
        // Same stamping the in-memory store does: schema version, then the
        // kernel-owned seq / epoch_ms overwriting any caller value.
        stamp_schema_version(&mut event);
        stamp(&mut event, seq, epoch_ms);
        insert_row(&tx, &self.stream, seq, epoch_ms, &event)?;
        tx.commit().map_err(sqlite_err)?;
        Ok(Committed { seq, epoch_ms })
    }

    fn append_if_head(
        &mut self,
        mut event: Map<String, Value>,
        expected_head: u64,
    ) -> KnlResult<Committed> {
        // Read the head and insert in one transaction, so the compare-and-swap
        // holds under concurrent writers (the file lock serialises them).
        let tx = self.conn.transaction().map_err(sqlite_err)?;
        let actual = head_in(&tx, &self.stream)?;
        let matches = match expected_head {
            0 => actual.is_none(),
            expected => actual == Some(expected),
        };
        if !matches {
            return Err(KnlError::new(format!(
                "head conflict: expected {expected_head}, actual {actual:?}"
            )));
        }
        // Head matched: now the append can still reject a malformed event,
        // exactly as the in-memory store's `append_if_head` does.
        validate_event(&event)?;
        let seq = actual.unwrap_or(0).saturating_add(1);
        let epoch_ms = now_ms();
        stamp_schema_version(&mut event);
        stamp(&mut event, seq, epoch_ms);
        insert_row(&tx, &self.stream, seq, epoch_ms, &event)?;
        tx.commit().map_err(sqlite_err)?;
        Ok(Committed { seq, epoch_ms })
    }

    fn read(&self, from_seq: u64, limit: usize) -> Vec<Value> {
        // `usize::MAX` (an unbounded read) caps at i64::MAX, which SQLite
        // treats as "no limit"; `0` reads nothing.
        let capped = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = match self.conn.prepare(
            "SELECT payload FROM events \
             WHERE stream = ?1 AND seq >= ?2 ORDER BY seq ASC LIMIT ?3",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let Ok(rows) = stmt.query_map(
            params![self.stream, from_seq as i64, capped],
            |row| row.get::<_, String>(0),
        ) else {
            return Vec::new();
        };
        rows.filter_map(Result::ok)
            .filter_map(|payload| serde_json::from_str::<Value>(&payload).ok())
            .collect()
    }

    fn head(&self) -> Option<u64> {
        head_in(&self.conn, &self.stream).unwrap_or(None)
    }

    fn len(&self) -> usize {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE stream = ?1",
                params![self.stream],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .unwrap_or(0)
    }
}

/// The next `seq` for `stream`: `MAX(seq) + 1`, or `1` for an empty stream.
fn next_seq(conn: &Connection, stream: &str) -> KnlResult<u64> {
    conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE stream = ?1",
        params![stream],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n as u64)
    .map_err(sqlite_err)
}

/// The current head of `stream`: `MAX(seq)`, or `None` when empty.
fn head_in(conn: &Connection, stream: &str) -> KnlResult<Option<u64>> {
    let max: Option<i64> = conn
        .query_row(
            "SELECT MAX(seq) FROM events WHERE stream = ?1",
            params![stream],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(sqlite_err)?;
    Ok(max.map(|n| n as u64))
}

/// Insert the fully-stamped event; `payload` is the whole event object, so a
/// read reconstructs the exact same `Value` the in-memory store returns.
fn insert_row(
    conn: &Connection,
    stream: &str,
    seq: u64,
    epoch_ms: u64,
    event: &Map<String, Value>,
) -> KnlResult<()> {
    let kind = event.get(FIELD_KIND).and_then(Value::as_str).unwrap_or("");
    let schema_version = event
        .get(SCHEMA_VERSION_FIELD)
        .and_then(Value::as_u64)
        .unwrap_or(CURRENT_SCHEMA_VERSION);
    let payload = serde_json::to_string(&Value::Object(event.clone()))
        .map_err(|e| KnlError::new(format!("sqlite: encode event: {e}")))?;
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
    .map_err(sqlite_err)?;
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
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        let a = store.append(ev(1)).expect("append e1");
        let b = store.append(ev(2)).expect("append e2");
        let c = store.append(ev(3)).expect("append e3");

        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
        assert_eq!(store.len(), 3);
        assert!(!store.is_empty());

        // The stamped epoch is what is stored.
        let stored = store.read(0, usize::MAX);
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
        assert_eq!(store.len(), 0);
        assert_eq!(store.append(ev(1)).expect("append").seq, 1);
    }

    #[test]
    fn append_if_head_is_a_compare_and_swap() {
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");

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
        let err = store.append_if_head(ev(3), 5).expect_err("stale head");
        assert!(err.reason().contains("expected 5"), "{err}");
        assert!(err.reason().contains("actual Some(2)"), "{err}");
        assert_eq!(store.len(), 2, "no append on conflict");
    }

    #[test]
    fn read_pages_by_from_seq_and_limit() {
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
        for i in 1..=5 {
            store.append(ev(i)).expect("append");
        }

        assert_eq!(store.read(0, usize::MAX).len(), 5);
        assert_eq!(store.read(1, usize::MAX).len(), 5);
        assert_eq!(store.read(3, usize::MAX).len(), 3);
        assert_eq!(store.read(6, usize::MAX).len(), 0);

        let page = store.read(2, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(kind_of(&page[0]), "e2");
        assert_eq!(kind_of(&page[1]), "e3");

        // A zero limit returns nothing even when events match.
        assert!(store.read(0, 0).is_empty());
    }

    #[test]
    fn head_is_none_when_empty_then_tracks_the_max() {
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
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
    fn read_reconstructs_the_stored_envelope_including_the_schema_version() {
        let mut store = SqliteEventStore::open_in_memory("s").expect("open");
        store
            .append(obj(json!({ "kind": "note", "text": "hi" })))
            .expect("append");
        let stored = store.read(0, usize::MAX);
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
        let events = store.read(0, usize::MAX);
        assert_eq!(events.len(), 2);
        assert_eq!(kind_of(&events[0]), "note");
        assert_eq!(events[0]["text"], json!("durable"));
        assert_eq!(seq_of(&events[0]), 1);
        assert_eq!(
            events[0].get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
            Some(CURRENT_SCHEMA_VERSION),
            "the schema version survives the round-trip too"
        );
        assert_eq!(store.head(), Some(2));
    }

    #[test]
    fn two_streams_in_one_db_file_do_not_see_each_others_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.db");

        let mut a = SqliteEventStore::open(&path, "stream-a").expect("open a");
        let mut b = SqliteEventStore::open(&path, "stream-b").expect("open b");

        a.append(obj(json!({ "kind": "only_a" }))).expect("append a");
        b.append(obj(json!({ "kind": "only_b1" })))
            .expect("append b1");
        b.append(obj(json!({ "kind": "only_b2" })))
            .expect("append b2");

        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 2);
        // Each stream numbers its own seq from 1, independent of the other.
        assert_eq!(a.head(), Some(1));
        assert_eq!(b.head(), Some(2));

        assert_eq!(kind_of(&a.read(0, usize::MAX)[0]), "only_a");
        let b_events = b.read(0, usize::MAX);
        let b_kinds: Vec<&str> = b_events.iter().map(kind_of).collect();
        assert_eq!(b_kinds, ["only_b1", "only_b2"]);
    }
}
