//! `coding.run` end to end, against a scripted in-process model.
//!
//! The specs in `blocks/coding/spec/` cover the module's pure parts — what the
//! seed is made of, what the opts refuse, what `decide` decides. Nothing ran
//! the loop itself: the tests here do, through the real binary, the real
//! bridges (`sh.exec` runs the verify, `std.fs` applies the edit) and a mock
//! that answers whatever the test scripted for each call and records every
//! request it was sent — what the loop SAYS BACK about a beat is on the wire
//! and nowhere else.
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

/// What one run left behind: what the fixture printed, how many beats reached
/// the model, and every chat request body in the order it was sent.
struct Ran {
    stdout: String,
    calls: usize,
    bodies: Vec<Value>,
}

impl Ran {
    /// Whether any request body after the first carries `text`.
    ///
    /// After the first, because the first is the seed alone: everything the
    /// loop says back about a beat is by definition in a later one.
    fn a_later_request_says(&self, text: &str) -> bool {
        self.bodies
            .iter()
            .skip(1)
            .any(|body| body.to_string().contains(text))
    }
}

/// Run the fixture against a scripted mock and return what the run left.
///
/// `env` carries the case's own variables (`CODING_DONE_TEST` and the like);
/// the repository, the base url and a private `AGENT_BLOCK_HOME` are set here
/// because every case needs them and needs them the same.
async fn run_fixture(script: Vec<Value>, repo: &str, env: &[(&str, &str)]) -> Ran {
    let (base_url, call_count, bodies, ct) =
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
    let bodies = bodies
        .lock()
        .expect("the recorded bodies are not poisoned")
        .clone();
    ct.cancel();
    Ran {
        stdout,
        calls,
        bodies,
    }
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

    let ran = run_fixture(script, &dir.path().to_string_lossy(), &[]).await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");
    says(&ran.stdout, "iters=2");
    says(&ran.stdout, "failure_reason=nil");
    says(&ran.stdout, "done=declare");
    // The repository was red before the first beat, which is the fact the
    // baseline verify exists to record.
    says(&ran.stdout, "baseline_ok=false");
    says(&ran.stdout, "config.values.iters.from=caller");
    says(&ran.stdout, "config.values.context_window.from=caller");
    assert_eq!(ran.calls, 2, "one beat per turn: the edit, then the answer");

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

    let ran = run_fixture(script, &dir.path().to_string_lossy(), &[]).await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");
    // Iteration 1 ends on the edit; iteration 2 spends a turn on the read and
    // declares on the next one, so the extra beat costs no extra iteration.
    says(&ran.stdout, "iters=2");
    says(&ran.stdout, "done=declare");
    assert_eq!(
        ran.calls, 3,
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

    let ran = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[("CODING_DONE_TEST", "plan")],
    )
    .await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");
    says(&ran.stdout, "done=plan");
    says(&ran.stdout, "plan.total=2 plan.passed=2 plan.filed=true");
    assert_eq!(ran.calls, 3, "the plan, the edit, the answer");
}

/// A call that arrived without one of its required arguments is refused by
/// name, and the run goes on.
///
/// The first turn asks for an edit with a `path` and no `edits` — the shape a
/// reply cut at the output limit leaves behind, and the one the provider
/// reports as a whole call. `policy.require_args` answers it
/// `argument_missing` before the tool runs, which is what the next request
/// carries; the model then sends the edit whole and declares.
#[tokio::test]
async fn coding_run_refuses_a_tool_call_whose_required_argument_never_arrived() {
    let (dir, target) = make_repo();
    let cut_edit = tool_call(
        "call_edit_cut",
        "fs_search_replace",
        json!({ "path": target }),
    );
    let script = vec![cut_edit, edit_turn(&target), answer_turn("Sent it whole.")];

    let ran = run_fixture(script, &dir.path().to_string_lossy(), &[]).await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");
    assert!(
        ran.a_later_request_says("argument_missing"),
        "the refusal should reach the model as the tool's answer; the requests were:\n{:#?}",
        ran.bodies
    );
    assert!(
        ran.a_later_request_says("never arrived"),
        "the refusal should say why a whole-looking call can arrive without its arguments"
    );

    // The run carried on to the edit it was after.
    let after = std::fs::read_to_string(&target).expect("read the target back");
    assert!(
        after.contains(LIB_AFTER_LINE),
        "the edit that followed the refused call should be in the file; it holds:\n{after}"
    );
}

/// A reply the server cut at the output limit does not end the run, and the
/// next request says so.
///
/// The first turn answers with no tool call and `finish_reason = "length"`:
/// that is not the model saying it is done, so `declare` does not fire on it.
/// The loop states the fact and goes on to the edit and the real declaration.
#[tokio::test]
async fn coding_run_does_not_take_a_cut_reply_as_the_model_declaring() {
    let (dir, target) = make_repo();
    let mut cut = answer_turn("I will now edit the file by");
    cut["finish_reason"] = json!("length");
    let script = vec![cut, edit_turn(&target), answer_turn("Done.")];

    let ran = run_fixture(script, &dir.path().to_string_lossy(), &[]).await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");
    // The cut reply cost an iteration of its own: it neither edited nor ended
    // the run, so the edit and the declaration are the two that follow.
    says(&ran.stdout, "iters=3");
    assert_eq!(ran.calls, 3, "the cut reply, the edit, the answer");
    assert!(
        ran.a_later_request_says("the reply stopped at the output limit"),
        "the next request should carry the fact; the requests were:\n{:#?}",
        ran.bodies
    );
}

/// `ops`: a file written in two calls — `write` for the first part, `append`
/// for the rest.
///
/// The verify greps for `return n * 2`, and the first call deliberately stops
/// one character short of it: nothing is green until the append has landed. So
/// this says that both ops were handed over, that both are path-locked to the
/// target, and that both count as edits — an append that did not count would
/// leave the run reading its second iteration as no edit at all.
#[tokio::test]
async fn coding_run_writes_a_file_in_two_calls_when_ops_names_write_and_append() {
    let (dir, target) = make_repo();
    // Ends mid-expression: `return n * ` does not match the verify.
    let first_half = "local M = {}\n\nfunction M.double(n)\n    return n * ";
    let script = vec![
        tool_call(
            "call_write_1",
            "fs_write",
            json!({ "path": target, "content": first_half }),
        ),
        tool_call(
            "call_append_1",
            "fs_append",
            json!({ "path": target, "content": "2\nend\n\nreturn M\n" }),
        ),
        answer_turn("Written in two parts."),
    ];

    let ran = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[("CODING_EDIT_OPS_TEST", "write,append")],
    )
    .await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");
    // One iteration per edit — each ends the turn loop the moment it lands —
    // and the declaration in the third.
    says(&ran.stdout, "iters=3");
    assert_eq!(ran.calls, 3, "the write, the append, the answer");

    let after = std::fs::read_to_string(&target).expect("read the target back");
    assert_eq!(
        after, "local M = {}\n\nfunction M.double(n)\n    return n * 2\nend\n\nreturn M\n",
        "the two calls should have written the whole file between them"
    );
}

/// `seed = "names"`: the targets go in as paths, and the model reads what it
/// needs.
///
/// The first request is the whole of the evidence for the seed — it is the
/// seed and nothing else — so it is asserted directly: the target's path is
/// there and the target's content is not. The run then reads the file, edits
/// it and declares, which is the point of the shape: nothing is lost, it is
/// fetched a range at a time.
#[tokio::test]
async fn coding_run_seeds_the_targets_by_name_and_lets_the_model_read_them() {
    let (dir, target) = make_repo();
    let script = vec![
        tool_call("call_read_1", "fs_read", json!({ "path": target })),
        edit_turn(&target),
        answer_turn("Read it, edited it."),
    ];

    let ran = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[("CODING_SEED_TEST", "names")],
    )
    .await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");

    let first = ran.bodies.first().expect("a first request").to_string();
    assert!(
        first.contains("Target files"),
        "the seed should list the targets; the first request was:\n{first}"
    );
    assert!(
        first.contains(target.trim_start_matches('/')),
        "the seed should name the target's path; the first request was:\n{first}"
    );
    assert!(
        !first.contains("function M.double"),
        "the seed should carry no line of the target; the first request was:\n{first}"
    );

    // The model read the file and the edit landed all the same.
    let after = std::fs::read_to_string(&target).expect("read the target back");
    assert!(
        after.contains(LIB_AFTER_LINE),
        "the edit should be in the file; it holds:\n{after}"
    );
}

/// `call_reserve`: every request carries the point its reasoning has to stop
/// at for the tool call after it to fit.
///
/// The mock's `/tokenize` answers a fixed count and its model card a fixed
/// window, so the number is arithmetic rather than a guess: the window, less
/// what the request costs, less the fold's reserve, less what is kept for the
/// call. The conf names the vllm dialect and asks for reasoning, which is what
/// puts `thinking_token_budget` on the wire at all.
#[tokio::test]
async fn coding_run_sends_the_reasonings_stop_point_when_call_reserve_is_named() {
    let (dir, target) = make_repo();
    let script = vec![
        edit_turn(&target),
        answer_turn("Done: double returns n * 2."),
    ];

    // The mock's own numbers (tests/common/openai_mock.rs) and the fixture's.
    const WINDOW: i64 = 32768;
    const COUNTED: i64 = 128;
    const RESERVE: i64 = 1024;
    const CALL_RESERVE: i64 = 512;

    let ran = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[("CODING_CALL_RESERVE_TEST", &CALL_RESERVE.to_string())],
    )
    .await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");

    let expected = WINDOW - COUNTED - RESERVE - CALL_RESERVE;
    for (n, body) in ran.bodies.iter().enumerate() {
        assert_eq!(
            body["thinking_token_budget"].as_i64(),
            Some(expected),
            "request {n} should carry the stop point {expected}; it was:\n{body:#?}"
        );
    }
    assert!(!ran.bodies.is_empty(), "the run should have sent requests");
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

    let ran = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[("CODING_OMIT_RESERVE", "1")],
    )
    .await;

    says(&ran.stdout, "CODING_MOCK_REFUSED");
    says(&ran.stdout, "the reply's room is not named");
    assert_eq!(ran.calls, 0, "the refusal comes before any model call");
}
