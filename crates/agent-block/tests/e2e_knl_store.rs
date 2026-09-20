//! `agent-block knl sessions` and `agent-block knl backup` — what a reader
//! outside the host can ask of a *store*, rather than of one session in it.
//!
//! Both exist because the alternative is not available to that reader:
//! `knl.views.sessions` is Lua running inside the host, and `cp` of a WAL
//! database with a writer open is not a consistent copy. So the test runs the
//! binary — the library only writes the events — and the assertions are on
//! stdout and the exit code, which is the surface a reader has.

mod common;

use std::path::Path;

use agent_block_core::knl::{Logs, Session, SqliteEventStore};
use serde_json::{json, Value};

/// Open a session on the store at `path`, write one fact, close it.
///
/// Closed rather than left to the drop backstop: `head_seq` is asserted
/// below, and a write still on the log's queue would make that a race.
async fn seed_session(logs: &Logs, path: &Path, stream: &str) {
    let store = SqliteEventStore::open(path, stream, logs)
        .await
        .expect("open the store");
    let mut session = Session::open_on("u".to_string(), None, None, Box::new(store))
        .await
        .expect("open the session");
    session
        .append(
            match json!({ "kind": "msg_user", "data": { "content": stream } }) {
                Value::Object(map) => map,
                other => panic!("an event is an object, got {other}"),
            },
        )
        .await
        .expect("append");
    session.close(None).await.expect("close the session");
}

/// A temp store holding the named sessions, and the directory that owns it.
fn seeded(streams: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("knl.sqlite");
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            let logs = Logs::new();
            for stream in streams {
                seed_session(&logs, &path, stream).await;
            }
            assert!(logs.shutdown().await.is_empty(), "the log closed cleanly");
        });
    (dir, path)
}

/// Every line of `stdout` as JSON, which is what "JSON Lines" has to mean for
/// a reader piping this into anything.
fn lines(stdout: &[u8]) -> Vec<Value> {
    std::str::from_utf8(stdout)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line:?}: {e}")))
        .collect()
}

/// `knl sessions` answers "which sessions are in this store", and the id it
/// prints is the one `knl export --session` takes.
#[test]
fn sessions_lists_the_store_and_its_ids_feed_export() {
    let (_dir, path) = seeded(&["s-one", "s-two"]);
    let store = path.to_str().expect("utf-8");

    let out = common::agent_block_cmd()
        .args(["knl", "sessions", "--store", store])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    let records = lines(&out);
    assert_eq!(
        records
            .iter()
            .map(|r| r["session"].clone())
            .collect::<Vec<_>>(),
        [json!("s-one"), json!("s-two")],
        "{records:?}"
    );
    for record in &records {
        // The opening, the one fact, the closing: where the stream got to.
        assert_eq!(record["head_seq"], json!(3), "{record}");
    }

    // The id a listing printed is an id an export takes — which is the whole
    // reason this subcommand exists.
    let id = records[0]["session"].as_str().expect("an id");
    let exported = common::agent_block_cmd()
        .args([
            "knl",
            "export",
            "--store",
            store,
            "--session",
            id,
            "--as",
            "messages",
        ])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let messages = lines(&exported);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["content"], json!("s-one"));
}

/// The listing pages: `--limit` cuts it and `--after` is exclusive, so the
/// last id printed is the next call's cursor.
#[test]
fn sessions_pages_by_the_last_id_it_printed() {
    let (_dir, path) = seeded(&["s-one", "s-three", "s-two"]);
    let store = path.to_str().expect("utf-8");

    let first = lines(
        &common::agent_block_cmd()
            .args(["knl", "sessions", "--store", store, "--limit", "2"])
            .assert()
            .code(0)
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(
        first
            .iter()
            .map(|r| r["session"].clone())
            .collect::<Vec<_>>(),
        [json!("s-one"), json!("s-three")],
        "{first:?}"
    );

    let cursor = first[1]["session"].as_str().expect("an id");
    let next = lines(
        &common::agent_block_cmd()
            .args(["knl", "sessions", "--store", store, "--after", cursor])
            .assert()
            .code(0)
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(
        next.iter()
            .map(|r| r["session"].clone())
            .collect::<Vec<_>>(),
        [json!("s-two")],
        "the cursor is exclusive: {next:?}"
    );
}

/// `knl backup` writes a copy that opens as a log — `knl export` reads the
/// copy — and refuses a destination that already holds something.
#[test]
fn backup_writes_a_copy_that_export_can_read() {
    let (dir, path) = seeded(&["s-one"]);
    let store = path.to_str().expect("utf-8");
    let copy = dir.path().join("copy.sqlite");
    let copy_arg = copy.to_str().expect("utf-8");

    common::agent_block_cmd()
        .args(["knl", "backup", "--store", store, "--to", copy_arg])
        .assert()
        .code(0)
        .stdout(predicates::prelude::predicate::str::contains(copy_arg));
    assert!(copy.exists(), "the copy is at the path that was asked for");

    // The copy is a kernel database, not a file that merely exists: the
    // session reads back out of it through the ordinary export.
    let exported = common::agent_block_cmd()
        .args([
            "knl",
            "export",
            "--store",
            copy_arg,
            "--session",
            "s-one",
            "--as",
            "events",
        ])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let kinds: Vec<Value> = lines(&exported)
        .iter()
        .map(|record| record["kind"].clone())
        .collect();
    assert_eq!(
        kinds,
        [
            json!("session_opened"),
            json!("msg_user"),
            json!("session_closed"),
        ],
        "{kinds:?}"
    );

    // A second backup to the same path is refused rather than overwriting
    // what is there, which is the shape that loses data on a typo.
    common::agent_block_cmd()
        .args(["knl", "backup", "--store", store, "--to", copy_arg])
        .assert()
        .code(1)
        .stderr(predicates::prelude::predicate::str::contains("error:"));
}

/// A store that is not there is the same one line every verb gives, and no
/// database is created on the spot.
#[test]
fn a_store_that_is_not_there_is_an_error_for_both_verbs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("absent.sqlite");
    let store = absent.to_str().expect("utf-8");

    common::agent_block_cmd()
        .args(["knl", "sessions", "--store", store])
        .assert()
        .code(1)
        .stderr(predicates::prelude::predicate::str::contains(
            "no kernel database at",
        ));

    common::agent_block_cmd()
        .args([
            "knl",
            "backup",
            "--store",
            store,
            "--to",
            dir.path().join("copy.sqlite").to_str().expect("utf-8"),
        ])
        .assert()
        .code(1)
        .stderr(predicates::prelude::predicate::str::contains(
            "no kernel database at",
        ));

    assert!(!absent.exists(), "the failed calls created nothing");
}
