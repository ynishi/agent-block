//! What the exit code says, held to it by running the processes.
//!
//! The contract a shell reads a run by: `0` finished, `75` never started,
//! anything else broke — and the one thing it must not read, that a returned
//! value saying the work failed is still a process that finished. It is
//! checked here rather than described in a handoff, because a caller writes
//! `case $?` against it and a description cannot go red.
//!
//! `--result` is checked in the same breath: the file is the only way an
//! answer leaves the process, and it is written only when there is one.

mod common;

use predicates::prelude::*;

/// A run that returned exits 0 — including when what it returned says the
/// work failed. The exit code is about the process; the answer is the value.
#[test]
fn a_return_is_exit_zero_even_when_the_value_says_it_failed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = dir.path().join("out.json");

    common::agent_block_cmd()
        .args([
            "-s",
            &common::fixture("exit_returns_failure.lua"),
            "--result",
            result.to_str().expect("utf-8"),
        ])
        .assert()
        .code(0);

    let written = std::fs::read_to_string(&result).expect("the value was written");
    let value: serde_json::Value = serde_json::from_str(&written).expect("json");
    assert_eq!(
        value["ok"], false,
        "the failure is in the value, not the exit code: {written}"
    );
}

/// A raise is exit 1, and nothing is written to `--result`: there was no
/// value. A reader has to handle the file's absence, so the absence is the
/// tested thing.
#[test]
fn a_raise_is_exit_one_and_writes_no_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = dir.path().join("out.json");

    common::agent_block_cmd()
        .args([
            "-s",
            &common::fixture("exit_raises.lua"),
            "--result",
            result.to_str().expect("utf-8"),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("the endpoint answered nonsense"));

    assert!(
        !result.exists(),
        "a run that raised wrote {} anyway",
        result.display()
    );
}

/// `job.defer` is exit 75 — `EX_TEMPFAIL` from sysexits(3) — and it is a
/// different answer from a failure on purpose: the block looked at what it
/// needed, did not find it, and never started. A shell that reads "non-zero
/// is failure" reads that as a broken run, which is the whole reason the
/// code is its own.
#[test]
fn a_defer_is_exit_seventy_five_and_writes_no_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = dir.path().join("out.json");

    common::agent_block_cmd()
        .args([
            "-s",
            &common::fixture("exit_defers.lua"),
            "--result",
            result.to_str().expect("utf-8"),
        ])
        .assert()
        .code(75)
        .stderr(predicate::str::contains("no pod"));

    assert!(
        !result.exists(),
        "a run that never started wrote {} anyway",
        result.display()
    );
}

/// A command line that does not parse is exit 2 — clap's, and not one of
/// ours, so a caller can tell "I called it wrong" from "it ran and broke".
#[test]
fn a_command_line_that_does_not_parse_is_exit_two() {
    common::agent_block_cmd()
        .args(["--nonesuch"])
        .assert()
        .code(2);
}

/// Naming no script at all is the run refusing to start, not a parse error:
/// clap cannot mark it required (a subcommand or `--block` stands in), so
/// the check is ours and answers 1 like any other refusal to run.
#[test]
fn naming_no_script_is_exit_one() {
    common::agent_block_cmd()
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no script given"));
}
