//! `agent-block knl export` — a run's log, read by a process that did not
//! write it.
//!
//! The point of the subcommand is that no Lua is involved on the reading
//! side, so the test runs the binary: the library is used only to write the
//! events, and everything after that goes through the command line and its
//! stdout, which is the surface a reader actually has.

mod common;

use std::path::Path;

use agent_block_core::knl::{Logs, Session, SqliteEventStore};
use serde_json::{json, Map, Value};

/// The stream the seeded session writes to, and the id `--session` names.
const STREAM: &str = "s-export";

/// An event as a caller writes one.
fn event(kind: &str, beat: &str, data: Value) -> Map<String, Value> {
    match json!({ "kind": kind, "meta": { "beat": beat }, "data": data }) {
        Value::Object(map) => map,
        other => panic!("an event is an object, got {other}"),
    }
}

/// Write one turn's worth of facts into the store at `path`.
async fn seed(path: &Path) {
    let logs = Logs::new();
    let store = SqliteEventStore::open(path, STREAM, &logs)
        .await
        .expect("open the store");
    let mut session = Session::open_on("u".to_string(), None, None, Box::new(store))
        .await
        .expect("open the session");

    for written in [
        event("msg_user", "b1", json!({ "content": "list the files" })),
        event(
            "llm_response",
            "b1",
            json!({
                "content": [{ "type": "tool_use", "id": "c-1", "name": "sh", "input": {} }],
                "usage": { "input_tokens": 11, "output_tokens": 4 },
                "stop_reason": "tool_use",
            }),
        ),
        event(
            "tool_call",
            "b1",
            json!({ "call_id": "c-1", "name": "sh", "args": { "cmd": "ls" } }),
        ),
        event(
            "tool_result",
            "b1",
            json!({ "call_id": "c-1", "ok": true, "result": "a\nb" }),
        ),
    ] {
        session.append(written).await.expect("append");
    }

    assert!(logs.shutdown().await.is_empty(), "the log closed cleanly");
}

/// Seed a store in a temp directory and hand back the directory (which owns
/// the file) and the path.
fn seeded() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("knl.sqlite");
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(seed(&path));
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

/// `--as events` is the log itself: the opening the kernel wrote and every
/// fact after it, in `seq` order, each one whole.
#[test]
fn events_is_the_stored_log() {
    let (_dir, path) = seeded();

    let out = common::agent_block_cmd()
        .args([
            "knl",
            "export",
            "--store",
            path.to_str().expect("utf-8"),
            "--session",
            STREAM,
            "--as",
            "events",
        ])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    let records = lines(&out);
    let kinds: Vec<&str> = records.iter().filter_map(|r| r["kind"].as_str()).collect();
    assert_eq!(
        kinds.first(),
        Some(&"session_opened"),
        "the log opens with the boundary the kernel wrote: {kinds:?}"
    );
    assert_eq!(
        &kinds[kinds.len() - 4..],
        ["msg_user", "llm_response", "tool_call", "tool_result"],
        "{kinds:?}"
    );

    // Whole records, in order: the coordinates the store assigned are there,
    // and so is what the caller put under `meta` and `data`.
    let seqs: Vec<u64> = records.iter().filter_map(|r| r["seq"].as_u64()).collect();
    assert_eq!(seqs.len(), records.len(), "every record carries its seq");
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");

    let user = records
        .iter()
        .find(|r| r["kind"] == json!("msg_user"))
        .expect("the seeded msg_user");
    assert_eq!(user["data"]["content"], json!("list the files"));
    assert_eq!(user["meta"]["beat"], json!("b1"));
}

/// `--as messages` is the conversation the log holds: the four kinds a turn
/// is made of, one record each, carrying where in the log they came from.
#[test]
fn messages_is_the_conversation_the_log_holds() {
    let (_dir, path) = seeded();

    let out = common::agent_block_cmd()
        .args([
            "knl",
            "export",
            "--store",
            path.to_str().expect("utf-8"),
            "--session",
            STREAM,
            "--as",
            "messages",
        ])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    let records = lines(&out);
    assert_eq!(
        records.iter().map(|r| &r["kind"]).collect::<Vec<_>>(),
        [
            &json!("msg_user"),
            &json!("llm_response"),
            &json!("tool_call"),
            &json!("tool_result"),
        ],
        "the boundaries and the ledger are not part of the conversation: {records:?}"
    );

    assert_eq!(records[0]["role"], json!("user"));
    assert_eq!(records[0]["content"], json!("list the files"));

    assert_eq!(records[1]["role"], json!("assistant"));
    assert_eq!(records[1]["usage"]["input_tokens"], json!(11));
    assert_eq!(records[1]["stop_reason"], json!("tool_use"));

    assert_eq!(records[2]["role"], json!("assistant"));
    assert_eq!(
        records[2]["content"],
        json!({ "type": "tool_use", "id": "c-1", "name": "sh", "input": { "cmd": "ls" } })
    );

    assert_eq!(records[3]["role"], json!("user"));
    assert_eq!(
        records[3]["content"],
        json!({ "type": "tool_result", "tool_use_id": "c-1", "content": "a\nb" }),
        "a call that went well carries no is_error mark"
    );

    // Where each part came from, on every record: the beat the caller
    // declared, the coordinate the store assigned, and the kind it was.
    for record in &records {
        assert_eq!(record["beat"], json!("b1"), "{record}");
        assert!(record["seq"].as_u64().is_some(), "{record}");
        assert!(record["epoch_ms"].as_u64().is_some(), "{record}");
    }
}

/// A session that is not in the store is a failure, not an empty answer: a
/// reader asking for a run by id and silently getting nothing back cannot
/// tell a finished run from a typo.
#[test]
fn a_session_that_is_not_there_is_an_error() {
    let (_dir, path) = seeded();

    common::agent_block_cmd()
        .args([
            "knl",
            "export",
            "--store",
            path.to_str().expect("utf-8"),
            "--session",
            "s-nobody",
            "--as",
            "events",
        ])
        .assert()
        .code(1)
        .stdout(predicates::prelude::predicate::str::is_empty())
        .stderr(predicates::prelude::predicate::str::contains(
            "error: no session 's-nobody'",
        ));
}

/// And a store that is not there is the same one line rather than a database
/// created on the spot: opening creates, so the path is checked first.
#[test]
fn a_store_that_is_not_there_is_an_error_and_is_not_created() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("absent.sqlite");

    common::agent_block_cmd()
        .args([
            "knl",
            "export",
            "--store",
            path.to_str().expect("utf-8"),
            "--session",
            STREAM,
            "--as",
            "events",
        ])
        .assert()
        .code(1)
        .stderr(predicates::prelude::predicate::str::contains(
            "no kernel database at",
        ));

    assert!(
        !path.exists(),
        "the failed read left {} behind",
        path.display()
    );
}
