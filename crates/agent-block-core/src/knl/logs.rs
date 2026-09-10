//! The open logs a host holds, and the one-time work an open does.
//!
//! A session is a *stream*, and a stream lives in a *log*
//! ([`eventsdb_sqlite::SqliteEventLog`]): one database, one writer thread, a
//! pool of read-only connections beside it, and one upcaster chain.  [`Logs`]
//! is the host's collection of them — a file is opened once per process and
//! shared from then on, and one in-memory log serves every ephemeral session
//! of a run.
//!
//! # Why a log is opened once
//!
//! Opening the same file twice is safe but not free, and the two things it
//! costs are exactly the two this kernel depends on: subscribers are woken
//! per log, and **the upcaster chain is per log** — two logs opened with
//! different chains read the same bytes differently and nothing detects it.
//! So the key is the database rather than the session: the parent directory
//! canonicalised (the file itself may not exist yet — the host creates the
//! directory, not the file) beside the file name.
//!
//! It is also what makes a session *tree* work.  A child is opened on its
//! parent's database, and both halves of an allocation are one transaction,
//! which is a transaction on one connection: two logs on one file would have
//! two, and a hatch transaction on one of them would meet the other's write
//! lock rather than share it.
//!
//! # The in-memory log is a database, not a lesser session
//!
//! `store = "mem"` opens no file.  It is one log per [`Logs`], and a `"mem"`
//! session is a stream in it — so a `"mem"` parent can have children, and a
//! `"mem"` stream can be resumed by name, for as long as the host lives.  The
//! earlier backend gave each ephemeral session a shared-cache database of its
//! own, whose locks are per *table*, and had to refuse a tree on it; there is
//! nothing to refuse now, because there is one connection and one writer.
//!
//! # What an open does once
//!
//! Three things, in this order, and each of them is idempotent:
//!
//! 1. **the legacy migration** ([`migrate_legacy`]), for a `knl.sqlite` an
//!    earlier release wrote — before eventsdb sees the file at all;
//! 2. **`index_meta("beat")`**, so a read that filters on the beat label is
//!    an index range rather than a scan of every row's `meta`;
//! 3. **the parent index** ([`CHILD_INDEX_DDL`]), which is what makes the
//!    close-time child scan look openings up instead of walking the table.
//!
//! # Shutdown drains, and that is what the drop backstop rests on
//!
//! A handle nobody closed submits its `session_closed` from `Drop`, without
//! waiting for it ([`super::EventStore::detach_append`]).  That write is a job
//! on the log's own queue, so it lands when the queue drains — which is what
//! [`Logs::shutdown`] is for, and why the logs belong to the host rather than
//! to any session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eventsdb_core::position::Position;
use eventsdb_core::transfer::ExportedEvent;
use eventsdb_sqlite::{OpenOptions, SqliteEventLog};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use super::event::{FIELD_BEAT, FIELD_DATA, FIELD_EPOCH_MS, FIELD_KIND, FIELD_META, FIELD_SEQ};
use super::event_store::{kernel_upcasters, SCHEMA_VERSION_FIELD};
use super::sqlite_store::CHILD_INDEX_DDL;
use super::{KnlError, KnlResult};

/// The table a `knl.sqlite` written before this backend is moved aside into.
///
/// Its presence *is* the migration's state: the rename and the import are two
/// transactions, so a crash between them leaves the table there and the next
/// open picks up where this one stopped.
const LEGACY_TABLE: &str = "knl_legacy_events";

/// The indexes the earlier backend created, dropped before eventsdb opens the
/// file.
///
/// `events_stream_kind_seq` is the one that has to go: eventsdb's own ladder
/// creates an index of that name, and a leftover would make the first step
/// fail on a name that already exists.  The other two are dropped in the same
/// breath because nothing reads them any more.
const LEGACY_INDEXES: [&str; 3] = [
    "events_stream_kind_seq",
    "events_stream_beat_seq",
    "events_session_opened_parent",
];

/// The open logs of one host run.
///
/// Cheap to clone (an `Arc`), because every site that opens a session needs to
/// reach it: the bridge, the host's shutdown, and the tests.
#[derive(Clone, Default)]
pub struct Logs {
    inner: Arc<Open>,
}

/// What a [`Logs`] holds, behind the one `Arc` every clone shares.
#[derive(Default)]
struct Open {
    /// Files, keyed by the database rather than by the caller's spelling of
    /// the path.
    files: Mutex<HashMap<PathBuf, Arc<SqliteEventLog>>>,
    /// The one in-memory log, opened on first use.
    memory: Mutex<Option<Arc<SqliteEventLog>>>,
}

impl std::fmt::Debug for Logs {
    /// The logs themselves have nothing worth printing; that there are some
    /// is what a caller debugging a leak wants, and asking how many would
    /// mean taking a lock this cannot await on.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logs").finish_non_exhaustive()
    }
}

/// The options every log this kernel opens is opened with.
///
/// The chain is the kernel's ([`kernel_upcasters`]) and it is registered
/// *here*, on the log, rather than on the seam above it: eventsdb applies it
/// on every read it serves, so a second application in
/// [`super::CurrentStore`] would run each step twice.
fn options() -> OpenOptions {
    OpenOptions::default().upcasters(kernel_upcasters())
}

impl Logs {
    /// A fresh, empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// The log at `path`, opened if this is the first time it is asked for.
    ///
    /// The key is the canonicalised parent directory beside the file name:
    /// the file may not exist yet, so it cannot be canonicalised itself, but
    /// the directory does — the host creates it before it hands the path over
    /// — and that is enough to make `./a/knl.sqlite` and `a/knl.sqlite` one
    /// key rather than two logs on one file.
    pub async fn file(&self, path: &Path) -> KnlResult<Arc<SqliteEventLog>> {
        let key = key_of(path);
        let mut open = self.inner.files.lock().await;
        if let Some(log) = open.get(&key) {
            return Ok(Arc::clone(log));
        }
        let log = Arc::new(open_file(path).await?);
        open.insert(key, Arc::clone(&log));
        Ok(log)
    }

    /// The in-memory log, opened if this is the first time it is asked for.
    ///
    /// One per [`Logs`], so every `store = "mem"` session of a run is a
    /// stream in the same database: children work, and a resume by name finds
    /// the stream it names.
    pub async fn memory(&self) -> KnlResult<Arc<SqliteEventLog>> {
        let mut open = self.inner.memory.lock().await;
        if let Some(log) = open.as_ref() {
            return Ok(Arc::clone(log));
        }
        let log = SqliteEventLog::open_in_memory_with(options())
            .await
            .map_err(KnlError::from)?;
        ensure_indexes(&log).await?;
        let log = Arc::new(log);
        *open = Some(Arc::clone(&log));
        Ok(log)
    }

    /// The log a `database` identity names.
    ///
    /// [`super::EventStore::database`] hands back an identity to pass along
    /// and not to take apart, and this is the one thing that is done with one:
    /// opening a *second* stream on the same database, which is what a child
    /// session is.  The in-memory log answers to its own identity; anything
    /// else is a path.
    pub async fn database(&self, database: &str) -> KnlResult<Arc<SqliteEventLog>> {
        {
            let open = self.inner.memory.lock().await;
            if let Some(log) = open.as_ref() {
                if log.database() == database {
                    return Ok(Arc::clone(log));
                }
            }
        }
        self.file(Path::new(database)).await
    }

    /// How many logs are open.
    pub async fn len(&self) -> usize {
        let files = self.inner.files.lock().await.len();
        let memory = usize::from(self.inner.memory.lock().await.is_some());
        files + memory
    }

    /// Whether no log has been opened (or all were shut down).
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Drain every log: queued jobs run to completion, then each connection
    /// thread stops and is joined.
    ///
    /// The queued jobs matter — the drop backstop submits its `session_closed`
    /// without waiting for it, so this is where those land.  Failures are
    /// collected rather than raised on the first one: a thread that panicked
    /// is no reason to leave the rest running.
    ///
    /// Idempotent: a second call finds nothing open and returns an empty list.
    pub async fn shutdown(&self) -> Vec<eventsdb_core::Error> {
        let files: Vec<Arc<SqliteEventLog>> = {
            let mut open = self.inner.files.lock().await;
            open.drain().map(|(_, log)| log).collect()
        };
        let memory = self.inner.memory.lock().await.take();

        let mut failures = Vec::new();
        for log in files.into_iter().chain(memory) {
            if let Err(e) = log.shutdown().await {
                failures.push(e);
            }
        }
        failures
    }
}

/// The key a file is held under: the canonicalised parent, then the name.
///
/// A path that cannot be canonicalised (a parent that is not there yet) is
/// used as it was given — the open below will report the real problem, and a
/// key that is merely less canonical still cannot collide with another file.
fn key_of(path: &Path) -> PathBuf {
    let name = path.file_name();
    match (path.parent(), name) {
        (Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
            Ok(parent) => parent.join(name),
            Err(_) => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// Open the file at `path`: migrate a legacy log if there is one, then open,
/// then create the indexes this kernel's reads depend on.
async fn open_file(path: &Path) -> KnlResult<SqliteEventLog> {
    // Before eventsdb sees the file: the old table is moved aside on a plain
    // connection, so the ladder below creates its own schema rather than
    // meeting one that half matches.
    let legacy = {
        let path = path.to_path_buf();
        // A blocking open on the caller's thread would be the VM's thread,
        // which is the one thread that must never wait on the OS.
        tokio::task::spawn_blocking(move || prepare_legacy(&path))
            .await
            .map_err(|e| KnlError::Storage(format!("knl: the legacy check panicked: {e}")))??
    };

    let log = SqliteEventLog::open_with(path, options())
        .await
        .map_err(KnlError::from)?;
    ensure_indexes(&log).await?;
    if legacy {
        migrate_legacy(&log).await?;
    }
    Ok(log)
}

/// The indexes a freshly opened log gets, whether it is new or not.
///
/// Both are `IF NOT EXISTS`, so this is the same call on the hundredth open as
/// on the first, and neither is a change to what an event *is*: adding an
/// index bumps no schema version (see [`super::event_store`]).
async fn ensure_indexes(log: &SqliteEventLog) -> KnlResult<()> {
    // The beat is the one `meta` label the log itself is grouped by, so it is
    // the one key worth an index of its own.
    log.index_meta(FIELD_BEAT).await.map_err(KnlError::from)?;
    // Through the hatch, because it is an index on `events`: the authorizer
    // refuses a *write* to the log's tables and allows an index on one, which
    // is exactly this.
    log.with_transaction(|tx| {
        tx.execute_batch(CHILD_INDEX_DDL)
            .map_err(|e| eventsdb_core::Error::storage(e.to_string()))
    })
    .await
    .map_err(KnlError::from)
}

/// Whether `path` holds a log this backend has to bring forward, moving it
/// aside if so.
///
/// Two questions in one, because the answer to the second is what the first
/// leaves behind.  A file that already carries [`LEGACY_TABLE`] is one whose
/// rename committed and whose import did not — a crash between the two — and
/// it needs the import and nothing else.  A file with an `events` table at
/// `user_version` 0 and no `position` column is one an earlier release wrote:
/// the table is renamed and its indexes dropped, here, on a plain connection,
/// before eventsdb opens the file and runs its ladder.
///
/// Anything else — a fresh path, a file eventsdb already owns — is `false`.  A
/// file from a *newer* build is not detected here at all: eventsdb refuses it
/// when it migrates, and that refusal is the honest one to surface.
fn prepare_legacy(path: &Path) -> KnlResult<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut conn = rusqlite::Connection::open(path).map_err(KnlError::from)?;
    if table_exists(&conn, LEGACY_TABLE)? {
        return Ok(true);
    }
    if !table_exists(&conn, "events")? {
        return Ok(false);
    }
    let user_version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(KnlError::from)?;
    if user_version != 0 {
        return Ok(false);
    }
    if has_column(&conn, "events", "position")? {
        return Ok(false);
    }

    let tx = conn.transaction().map_err(KnlError::from)?;
    tx.execute_batch(&format!("ALTER TABLE events RENAME TO {LEGACY_TABLE}"))
        .map_err(KnlError::from)?;
    for index in LEGACY_INDEXES {
        tx.execute_batch(&format!("DROP INDEX IF EXISTS {index}"))
            .map_err(KnlError::from)?;
    }
    tx.commit().map_err(KnlError::from)?;
    tracing::info!(
        path = %path.display(),
        "knl: an earlier release's log was moved aside; its events are imported on this open"
    );
    Ok(true)
}

/// Whether `table` is in the schema.
fn table_exists(conn: &rusqlite::Connection, table: &str) -> KnlResult<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        rusqlite::params![table],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .map_err(KnlError::from)
}

/// Whether `table` has a column called `column`.
fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> KnlResult<bool> {
    let mut stmt = conn
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .map_err(KnlError::from)?;
    let mut rows = stmt.query([]).map_err(KnlError::from)?;
    while let Some(row) = rows.next().map_err(KnlError::from)? {
        if row.get::<_, String>(0).map_err(KnlError::from)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Bring the rows of [`LEGACY_TABLE`] into the log, and drop the table.
///
/// One transaction: the import and the drop land together, so the table is
/// there exactly while the migration has not committed.
///
/// **The events keep everything but their coordinates.**  `epoch_ms` and the
/// version they were written under survive ([`eventsdb_core::event::restamp`]),
/// because re-stamping an old event as current would put it out of reach of
/// the upcaster written for it.  `beat` was a column then and is a `meta` key
/// now, so it moves inside; `seq` is reassigned by the receiving store, and
/// the check below is that it was reassigned *the same*.
///
/// **The order is the reassignment.**  `seq` comes back as the position of the
/// row within its stream in the order they are imported, so the import is
/// ordered the way the rows were written: by wall clock, with `(stream, seq)`
/// breaking ties.  A clock that went backwards inside one stream would
/// reorder it, and that is caught rather than committed — the transaction
/// rolls back and the import is made again ordered by `(stream, seq)`, which
/// reproduces the old numbering by construction.
async fn migrate_legacy(log: &SqliteEventLog) -> KnlResult<()> {
    const BY_CLOCK: &str = "ORDER BY epoch_ms, stream, seq";
    const BY_STREAM: &str = "ORDER BY stream, seq";

    let reordered = Arc::new(AtomicBool::new(false));
    let imported = match import_legacy(log, BY_CLOCK, Arc::clone(&reordered)).await {
        Ok(imported) => imported,
        // Only the reordering is answered by trying again: any other failure
        // is the store's, and a second identical attempt would meet it again.
        Err(_) if reordered.load(Ordering::SeqCst) => {
            import_legacy(log, BY_STREAM, Arc::new(AtomicBool::new(false))).await?
        }
        Err(e) => return Err(e),
    };
    tracing::info!(
        events = imported,
        "knl: the events of an earlier release's log were imported"
    );
    Ok(())
}

/// One attempt at the import, in the given order.
///
/// `reordered` is raised when a row's reassigned `seq` is not the one it had,
/// which is the one failure the caller answers by trying a different order.
async fn import_legacy(
    log: &SqliteEventLog,
    order: &'static str,
    reordered: Arc<AtomicBool>,
) -> KnlResult<usize> {
    log.with_transaction(move |tx| {
        // Read whole, then import: the statement is finished with before the
        // first write, so the read and the insert are not interleaved on one
        // connection and the `DROP` below meets no open cursor.
        let legacy: Vec<(u64, ExportedEvent)> = {
            let mut stmt = tx
                .prepare(&format!(
                    "SELECT stream, seq, epoch_ms, kind, schema_version, beat, meta, data \
                     FROM {LEGACY_TABLE} {order}"
                ))
                .map_err(storage)?;
            let rows = stmt.query_map([], legacy_row).map_err(storage)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(storage)?);
            }
            out
        };

        let imported = legacy.len();
        for (was, exported) in &legacy {
            let committed = tx.import(exported)?;
            if committed.seq != *was {
                reordered.store(true, Ordering::SeqCst);
                return Err(eventsdb_core::Error::storage(format!(
                    "knl: importing an earlier release's log renumbered stream {:?} \
                     ({was} became {}); the import is made again in stream order",
                    exported.stream, committed.seq
                )));
            }
        }

        tx.execute_batch(&format!("DROP TABLE {LEGACY_TABLE}"))
            .map_err(storage)?;
        Ok(imported)
    })
    .await
    .map_err(KnlError::from)
}

/// A rusqlite failure inside the hatch, as the store failing to do the work.
///
/// The reads and the `DROP` here are the migration's own SQL rather than the
/// log's, so they come back in rusqlite's language and are given eventsdb's.
fn storage(error: rusqlite::Error) -> eventsdb_core::Error {
    eventsdb_core::Error::storage(error.to_string())
}

/// One legacy row, as the `seq` it had and the record it travels as.
///
/// The inverse of the earlier backend's insert: the envelope out of its
/// columns, `meta` and `data` out of theirs, and the beat column back inside
/// the `meta` where a beat lives now.
fn legacy_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(u64, ExportedEvent)> {
    let stream: String = row.get(0)?;
    let seq: i64 = row.get(1)?;
    let epoch_ms: i64 = row.get(2)?;
    let kind: String = row.get(3)?;
    let schema_version: i64 = row.get(4)?;
    let beat: Option<String> = row.get(5)?;
    let meta: String = row.get(6)?;
    let data: String = row.get(7)?;

    let mut meta = decode_object(&meta)?;
    if let Some(beat) = beat {
        // An event written under the shape that had a `beat` column cannot
        // already carry the label, so there is nothing here to overwrite —
        // and a row that does carry it was written under the shape that keeps
        // it there, which is the value to leave alone.
        meta.entry(FIELD_BEAT.to_string())
            .or_insert_with(|| Value::from(beat));
    }

    let mut event = Map::new();
    event.insert(FIELD_KIND.to_string(), Value::from(kind));
    event.insert(FIELD_META.to_string(), Value::Object(meta));
    event.insert(FIELD_DATA.to_string(), Value::Object(decode_object(&data)?));
    event.insert(FIELD_SEQ.to_string(), Value::from(seq as u64));
    event.insert(FIELD_EPOCH_MS.to_string(), Value::from(epoch_ms as u64));
    event.insert(
        SCHEMA_VERSION_FIELD.to_string(),
        Value::from(schema_version as u64),
    );

    Ok((
        seq as u64,
        ExportedEvent {
            stream,
            // A witness, not an instruction: the earlier backend had no
            // global position to record, so there is none to carry.
            position: Position::BEGINNING,
            event,
        },
    ))
}

/// A stored JSON column, which must be an object.
fn decode_object(text: &str) -> rusqlite::Result<Map<String, Value>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) | Err(_) => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("a stored column of an earlier release's log is not an object: {text}").into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::event::kind_of;
    use crate::knl::{EventStore, SqliteEventStore, CURRENT_SCHEMA_VERSION};
    use serde_json::json;

    /// The schema an earlier release wrote, copied here rather than referred
    /// to.
    ///
    /// It is not the store's DDL any more — nothing in the product creates
    /// this table — so it lives with the test that is *about* it, which is
    /// also what keeps the fixture honest: a change to today's schema cannot
    /// quietly change what "a log an earlier release wrote" means.
    const LEGACY_DDL: &str = "CREATE TABLE IF NOT EXISTS events ( \
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
             ON events (stream, beat, seq); \
         CREATE INDEX IF NOT EXISTS events_session_opened_parent \
             ON events (json_extract(data, '$.parent')) \
          WHERE kind = 'session_opened';";

    /// One row of the old table: `(stream, seq, epoch_ms, kind, beat)`.
    type LegacyRow = (&'static str, i64, i64, &'static str, Option<&'static str>);

    /// Write a file exactly as an earlier release would have left it.
    fn write_legacy(path: &Path, rows: &[LegacyRow]) {
        let conn = rusqlite::Connection::open(path).expect("create the legacy file");
        conn.execute_batch(LEGACY_DDL).expect("the legacy schema");
        for (stream, seq, epoch_ms, kind, beat) in rows {
            conn.execute(
                "INSERT INTO events \
                 (stream, seq, epoch_ms, kind, schema_version, beat, meta, data) \
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7)",
                rusqlite::params![
                    stream,
                    seq,
                    epoch_ms,
                    kind,
                    beat,
                    // The shape that had a `beat` column kept the label out of
                    // `meta`, so this is what such a row really looked like.
                    json!({ "label": "kept" }).to_string(),
                    json!({ "text": kind }).to_string(),
                ],
            )
            .expect("a legacy row");
        }
        // `user_version` is 0, which is what says nobody's ladder has run here.
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version");
        assert_eq!(version, 0, "the fixture is a pre-ladder database");
    }

    /// The events of `stream`, read back through the kernel's own store.
    async fn read_back(logs: &Logs, path: &Path, stream: &str) -> Vec<Value> {
        SqliteEventStore::open(path, stream, logs)
            .await
            .expect("open")
            .read(0, usize::MAX)
            .await
            .expect("read")
    }

    /// A log an earlier release wrote is brought forward on the first open:
    /// the streams read back in `seq` order, the beat is where a beat lives
    /// now, and the version each row was written under is carried far enough
    /// for the chain to finish the job.
    #[tokio::test]
    async fn a_legacy_log_is_migrated_when_it_is_opened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.sqlite");
        // Two streams, interleaved in wall-clock order, one of them carrying a
        // beat: the order the import takes is `(epoch_ms, stream, seq)`, so
        // this is a fixture where that order is not the per-stream one.
        write_legacy(
            &path,
            &[
                ("s-a", 1, 100, "session_opened", None),
                ("s-b", 1, 150, "session_opened", None),
                ("s-a", 2, 200, "msg_user", Some("b-1")),
                ("s-b", 2, 250, "llm_response", Some("b-2")),
                ("s-a", 3, 300, "note", None),
            ],
        );

        let logs = Logs::new();
        let a = read_back(&logs, &path, "s-a").await;
        assert_eq!(
            a.iter().map(kind_of).collect::<Vec<_>>(),
            ["session_opened", "msg_user", "note"],
            "the stream came back in its own order"
        );
        assert_eq!(
            a.iter()
                .map(|e| e.get(FIELD_SEQ).and_then(Value::as_u64))
                .collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3)],
            "with the seq each row had"
        );
        assert_eq!(
            a[1].get(FIELD_META),
            Some(&json!({ "label": "kept", "beat": "b-1" })),
            "the beat column moved inside `meta`, beside the labels that were \
             already there"
        );
        assert_eq!(
            a[0].get(FIELD_META),
            Some(&json!({ "label": "kept" })),
            "a row with no beat gains no key"
        );
        for event in &a {
            assert_eq!(
                event.get(SCHEMA_VERSION_FIELD).and_then(Value::as_u64),
                Some(CURRENT_SCHEMA_VERSION),
                "the chain brought the stored version forward: {event}"
            );
        }
        assert_eq!(
            a[0].get(FIELD_EPOCH_MS).and_then(Value::as_u64),
            Some(100),
            "the time it happened is the row's, not the migration's"
        );

        let b = read_back(&logs, &path, "s-b").await;
        assert_eq!(
            b.iter().map(kind_of).collect::<Vec<_>>(),
            ["session_opened", "llm_response"]
        );

        // The counter came with the rows, so the next append carries on rather
        // than colliding with what is already there.
        let mut store = SqliteEventStore::open(&path, "s-a", &logs)
            .await
            .expect("open");
        assert_eq!(
            store
                .append(
                    json!({ "kind": "note" })
                        .as_object()
                        .expect("an object")
                        .clone()
                )
                .await
                .expect("append")
                .seq,
            4
        );
    }

    /// The migration is idempotent, and it is idempotent *by construction*:
    /// the old table exists exactly while the import has not committed, so a
    /// second open finds nothing to do and a crash between the two resumes.
    #[tokio::test]
    async fn a_second_open_of_a_migrated_log_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.sqlite");
        write_legacy(
            &path,
            &[
                ("s-a", 1, 100, "session_opened", None),
                ("s-a", 2, 200, "note", None),
            ],
        );

        {
            let logs = Logs::new();
            assert_eq!(read_back(&logs, &path, "s-a").await.len(), 2);
            assert!(logs.shutdown().await.is_empty(), "the log closed cleanly");
        }

        // A second process, a second open: the same two events, not four.
        let logs = Logs::new();
        assert_eq!(read_back(&logs, &path, "s-a").await.len(), 2);
        assert!(logs.shutdown().await.is_empty(), "the log closed cleanly");

        let conn = rusqlite::Connection::open(&path).expect("open the file");
        assert!(
            !table_exists(&conn, LEGACY_TABLE).expect("look for the old table"),
            "the old table is dropped in the transaction that imported it"
        );
    }

    /// A stream whose wall clock went backwards is imported in *stream* order
    /// instead.
    ///
    /// The reassigned `seq` is the row's position within its stream in the
    /// order the import walks, so an import ordered by the clock would have
    /// renumbered this one — which is caught by comparing every row's new
    /// number with its old one, rolled back, and made again in an order that
    /// reproduces the numbering by construction.
    #[tokio::test]
    async fn a_backwards_clock_is_imported_in_stream_order_instead() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.sqlite");
        write_legacy(
            &path,
            &[
                ("s-a", 1, 300, "session_opened", None),
                // Later in the stream, earlier on the clock: ordering by the
                // clock would put this first and number it 1.
                ("s-a", 2, 100, "note", None),
                ("s-a", 3, 400, "msg_user", None),
            ],
        );

        let logs = Logs::new();
        let a = read_back(&logs, &path, "s-a").await;
        assert_eq!(
            a.iter().map(kind_of).collect::<Vec<_>>(),
            ["session_opened", "note", "msg_user"],
            "the stream keeps the order it was written in"
        );
        assert_eq!(
            a.iter()
                .map(|e| e.get(FIELD_SEQ).and_then(Value::as_u64))
                .collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3)],
            "and every row keeps the number it had"
        );
        assert_eq!(
            a[1].get(FIELD_EPOCH_MS).and_then(Value::as_u64),
            Some(100),
            "the clock reading is kept as it was, backwards and all"
        );
    }

    /// A file this backend already owns is opened, not migrated: the detection
    /// is about a *legacy* table, and a database with a `position` column and
    /// a ladder marker is neither.
    #[tokio::test]
    async fn a_current_log_is_not_mistaken_for_a_legacy_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.sqlite");
        {
            let logs = Logs::new();
            let mut store = SqliteEventStore::open(&path, "s-1", &logs)
                .await
                .expect("open");
            store
                .append(
                    json!({ "kind": "note" })
                        .as_object()
                        .expect("an object")
                        .clone(),
                )
                .await
                .expect("append");
            assert!(logs.shutdown().await.is_empty(), "the log closed cleanly");
        }

        assert!(
            !prepare_legacy(&path).expect("look at the file"),
            "a current log has nothing to bring forward"
        );
        let logs = Logs::new();
        assert_eq!(read_back(&logs, &path, "s-1").await.len(), 1);
    }

    /// A path that is not there yet is not a legacy log either — it is a log
    /// that has not been created, and creating it is the open's business.
    #[test]
    fn a_path_with_no_file_is_not_a_legacy_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!prepare_legacy(&dir.path().join("absent.sqlite")).expect("look at the path"));
    }

    /// A file is opened once and shared: two stores on one path are two
    /// handles on one log, and the in-memory one is a database of its own.
    #[tokio::test]
    async fn a_file_is_opened_once_and_the_identity_finds_it_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("knl.sqlite");
        let logs = Logs::new();

        let first = logs.file(&path).await.expect("open");
        // The same file by a longer name: the key is the canonicalised parent
        // beside the file name, so this is not a second log.
        let indirect = dir.path().join(".").join("knl.sqlite");
        let second = logs.file(&indirect).await.expect("open again");
        assert!(Arc::ptr_eq(&first, &second), "one file, one log");
        assert_eq!(logs.len().await, 1);

        // And the identity a store reports finds that same log back.
        let by_identity = logs.database(first.database()).await.expect("by identity");
        assert!(Arc::ptr_eq(&first, &by_identity));

        // The in-memory log is a database of its own, and answers to its own
        // identity.
        let memory = logs.memory().await.expect("the in-memory log");
        assert_ne!(memory.database(), first.database());
        let same = logs
            .database(memory.database())
            .await
            .expect("the in-memory log by identity");
        assert!(Arc::ptr_eq(&memory, &same));
        assert_eq!(logs.len().await, 2);

        assert!(logs.shutdown().await.is_empty(), "both closed cleanly");
        assert!(logs.is_empty().await, "and a second shutdown finds nothing");
        assert!(logs.shutdown().await.is_empty());
    }
}
