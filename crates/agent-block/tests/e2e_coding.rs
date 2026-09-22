//! `coding.run` end to end, against a scripted in-process model.
//!
//! The specs in `blocks/coding/spec/` cover the module's pure parts — what the
//! seed is made of, what the opts refuse, what `decide` decides. Nothing ran
//! the loop itself: the four tests here do, through the real binary, the real
//! bridges (`sh.exec` runs the verify, `std.fs` applies the edit) and a mock
//! that answers whatever the test scripted for each call.
//!
//! The repository is a temporary directory holding one file, and the verify is
//! a fixed-string grep over it — so a test says green or red for exactly one
//! reason, and no toolchain is involved.
//!
//! Run with `LSHAPE_CHECK=1` (as `just test` does) to check the module's
//! opts and result contracts against what the host actually produces.

mod common;

use predicates::prelude::*;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use tempfile::{tempdir, TempDir};

/// The target file before the run: `double` answers nil, so the verify is red.
const LIB_BEFORE: &str = "local M = {}\n\nfunction M.double(n)\n    return nil\nend\n\nreturn M\n";

/// What the verify greps for, and what the scripted edit writes.
const LIB_AFTER_LINE: &str = "return n * 2";

/// A temporary repository holding the one target file.
fn make_repo() -> (TempDir, String) {
    let dir = tempdir().expect("tempdir for the repo");
    let target = dir.path().join("lib.lua");
    std::fs::write(&target, LIB_BEFORE).expect("write the target file");
    let target = target.to_string_lossy().into_owned();
    (dir, target)
}

/// An assistant turn that asks for one tool call.
fn tool_call(id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": arguments.to_string() }
        }]
    })
}

/// The edit that makes the verify pass.
fn edit_turn(target: &str) -> Value {
    tool_call(
        "call_edit_1",
        "fs_search_replace",
        json!({
            "path": target,
            "edits": [{ "search": "    return nil", "replace": format!("    {LIB_AFTER_LINE}") }]
        }),
    )
}

/// An assistant turn that answers with no tool call — the model saying it is
/// done. What the loop makes of that is decided against the facts.
fn answer_turn(text: &str) -> Value {
    json!({ "role": "assistant", "content": text, "tool_calls": null })
}

/// Run the fixture against a scripted mock and return `(stdout, chat_calls)`.
///
/// `env` carries the case's own variables (`CODING_DONE_TEST` and the like);
/// the repository, the base url and a private `AGENT_BLOCK_HOME` are set here
/// because every case needs them and needs them the same.
async fn run_fixture(script: Vec<Value>, repo: &str, env: &[(&str, &str)]) -> (String, usize) {
    let (base_url, call_count, ct) =
        common::openai_mock::spawn_scripted_openai_mock(script, "mock").await;
    // Give the server a moment to start accepting connections.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let repo = repo.to_string();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();

    let stdout = tokio::task::spawn_blocking(move || {
        let home = tempdir().expect("tempdir for AGENT_BLOCK_HOME");
        let mut cmd = common::agent_block_cmd();
        cmd.args(["-s", &common::fixture("coding_openai_mock.lua")])
            .env("OPENAI_BASE_URL_TEST", &base_url)
            .env("CODING_REPO_TEST", &repo)
            .env("AGENT_BLOCK_HOME", home.path());
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.assert().success();
        String::from_utf8_lossy(&out.get_output().stdout).into_owned()
    })
    .await
    .expect("subprocess task should not panic");

    let calls = call_count.load(Ordering::SeqCst);
    ct.cancel();
    (stdout, calls)
}

/// Assert `stdout` carries the marker line `line`.
fn says(stdout: &str, line: &str) {
    assert!(
        predicate::str::contains(line).eval(stdout),
        "expected the fixture to print `{line}`; it printed:\n{stdout}"
    );
}

/// The edit, then the model saying it is done.
///
/// `iters = 2`, not 1, and that is what the loop does rather than a rounding
/// of it: an iteration's turn loop breaks the moment an edit lands, so the
/// beat that landed the edit ends iteration 1 and the verify runs. The run
/// does not end there — a green verify is never enough by itself — so the
/// declaring answer necessarily arrives in iteration 2, and that is the
/// iteration the run converges on. Two beats reach the model, one per turn.
#[tokio::test]
async fn coding_run_converges_when_the_model_declares_on_a_green_verify() {
    let (dir, target) = make_repo();
    let script = vec![
        edit_turn(&target),
        answer_turn("Done: double returns n * 2."),
    ];

    let (stdout, calls) = run_fixture(script, &dir.path().to_string_lossy(), &[]).await;

    says(&stdout, "CODING_MOCK_DONE");
    says(&stdout, "ok=true");
    says(&stdout, "iters=2");
    says(&stdout, "failure_reason=nil");
    says(&stdout, "done=declare");
    // The repository was red before the first beat, which is the fact the
    // baseline verify exists to record.
    says(&stdout, "baseline_ok=false");
    says(&stdout, "config.values.iters.from=caller");
    says(&stdout, "config.values.context_window.from=caller");
    assert_eq!(calls, 2, "one beat per turn: the edit, then the answer");

    // The edit landed on disk, which is the only place it could have.
    let after = std::fs::read_to_string(&target).expect("read the target back");
    assert!(
        after.contains(LIB_AFTER_LINE),
        "the scripted edit should be in the file; it holds:\n{after}"
    );
}

/// A green verify does not end the run while the model is still calling tools.
///
/// Same script as above with one more tool call wedged in: the edit lands and
/// the verify goes green in iteration 1, the model then reads the file (a tool
/// call, so the run cannot end on it), and only its third turn declares. The
/// run still converges — with one beat more than the declare case, which is
/// what says the green did not end it early.
#[tokio::test]
async fn coding_run_does_not_end_on_a_green_verify_while_tools_are_still_called() {
    let (dir, target) = make_repo();
    let script = vec![
        edit_turn(&target),
        tool_call("call_read_1", "fs_read", json!({ "path": target })),
        answer_turn("Checked the file; done."),
    ];

    let (stdout, calls) = run_fixture(script, &dir.path().to_string_lossy(), &[]).await;

    says(&stdout, "CODING_MOCK_DONE");
    says(&stdout, "ok=true");
    // Iteration 1 ends on the edit; iteration 2 spends a turn on the read and
    // declares on the next one, so the extra beat costs no extra iteration.
    says(&stdout, "iters=2");
    says(&stdout, "done=declare");
    assert_eq!(
        calls, 3,
        "the read is a beat of its own: the run did not end on the green verify that preceded it"
    );
}

/// `done = "plan"`: the checks the model filed have to pass as well.
///
/// The model files two steps through the `plan` tool, then edits, then
/// declares. The harness runs both checks after each iteration; they pass once
/// the edit has landed, and the run ends with `plan.passed == plan.total`.
#[tokio::test]
async fn coding_run_in_plan_mode_ends_when_every_filed_check_passes() {
    let (dir, target) = make_repo();
    let plan = tool_call(
        "call_plan_1",
        "plan",
        json!({
            "steps": [
                { "step": "double returns n * 2", "check": "grep -qF 'return n * 2' lib.lua" },
                { "step": "the module still exports double", "check": "grep -qF 'function M.double' lib.lua" }
            ]
        }),
    );
    let script = vec![plan, edit_turn(&target), answer_turn("Plan done.")];

    let (stdout, calls) = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[("CODING_DONE_TEST", "plan")],
    )
    .await;

    says(&stdout, "CODING_MOCK_DONE");
    says(&stdout, "ok=true");
    says(&stdout, "done=plan");
    says(&stdout, "plan.total=2 plan.passed=2 plan.filed=true");
    assert_eq!(calls, 3, "the plan, the edit, the answer");
}

/// The tripwire: neither `llm.conf.max_tokens` nor `reserve` names the room
/// the reply needs, so the run refuses before it calls anything.
///
/// No model is reached, so the script here is never answered — the mock is
/// spawned only because the fixture wants a base url to put in the conf.
#[tokio::test]
async fn coding_run_refuses_when_the_replys_room_is_not_named() {
    let (dir, _target) = make_repo();
    let script = vec![answer_turn("never reached")];

    let (stdout, calls) = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[("CODING_OMIT_RESERVE", "1")],
    )
    .await;

    says(&stdout, "CODING_MOCK_REFUSED");
    says(&stdout, "the reply's room is not named");
    assert_eq!(calls, 0, "the refusal comes before any model call");
}
