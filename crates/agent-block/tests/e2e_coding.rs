//! `coding.run` end to end, against a scripted in-process model.
//!
//! The specs in `blocks/lib/coding/spec/` cover the module's pure parts — what the
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

/// An assistant turn that asks for several tool calls at once, in order.
fn tool_calls(calls: &[(&str, &str, Value)]) -> Value {
    let calls: Vec<Value> = calls
        .iter()
        .map(|(id, name, arguments)| {
            json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments.to_string() }
            })
        })
        .collect();
    json!({ "role": "assistant", "content": null, "tool_calls": calls })
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
    says(&ran.stdout, "config.room.window=32768");
    says(&ran.stdout, "config.room.result_within_beat=true");
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

/// The two steps a plan-mode run needs, as a `plan` tool call.
fn plan_turn() -> Value {
    tool_call(
        "call_plan_1",
        "plan",
        json!({
            "steps": [
                { "step": "double returns n * 2", "check": "grep -qF 'return n * 2' lib.lua" },
                { "step": "the module still exports double", "check": "grep -qF 'function M.double' lib.lua" }
            ]
        }),
    )
}

/// `done = "plan"`: the checks the model filed are what end the run.
///
/// The model files two steps through the `plan` tool, then edits. The harness
/// runs both checks after each iteration; they pass once the edit has landed,
/// and the run ends there — on the checks, with no declaration asked for. The
/// script carries a third turn that is never reached, which is what says so:
/// the mock would have served it.
#[tokio::test]
async fn coding_run_in_plan_mode_ends_when_every_filed_check_passes() {
    let (dir, target) = make_repo();
    let script = vec![plan_turn(), edit_turn(&target), answer_turn("Plan done.")];

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
    assert_eq!(
        ran.calls, 2,
        "the plan and the edit: the third scripted turn was never needed"
    );
}

/// A plan-mode model that never stops calling tools still ends the run.
///
/// The reported failure: a model files its plan, edits, and then keeps calling
/// tools — reading files, checking its work — as models do. It never produces
/// the bare answer that used to be required on top of the checks, so the run
/// went to the iteration cap and reported `max_iters` with the plan complete
/// and the verify green. Here every turn after the edit is another read, and
/// the mock is scripted with more of them than the run may take: reaching the
/// end of the script at all would mean the loop was still going.
#[tokio::test]
async fn coding_run_in_plan_mode_ends_without_a_declaration_from_the_model() {
    let (dir, target) = make_repo();
    let read = || tool_call("call_read_n", "fs_read", json!({ "path": target }));
    let script = vec![
        plan_turn(),
        edit_turn(&target),
        read(),
        read(),
        read(),
        read(),
    ];

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
    // The symptom, named: this is what the run used to report.
    assert!(
        !ran.stdout.contains("max_iters"),
        "the run ended on its checks, not at the iteration cap:\n{}",
        ran.stdout
    );
    assert_eq!(
        ran.calls, 2,
        "the plan and the edit; the reads scripted after them were never asked for"
    );
}

/// A plan check whose path is percent-encoded is refused at the filing, and
/// the model refiles.
///
/// The shape that prompted it: a framework's dynamic-route directory
/// (`[ns]`, `[id]`) reaching a check as `%5Bns%5D`, naming a file that does
/// not exist. Such a check cannot pass in this run or any other, so the run
/// spent every iteration re-running it and ended at the cap. Here the same
/// mistake — a path escape in a check — is answered at the filing: the model
/// is told what it wrote and what it means, files the plan again, and the run
/// ends on that plan in the SAME iteration. No plan is stored for the refused
/// filing and no edit is attributed to it.
#[tokio::test]
async fn coding_run_refuses_a_plan_check_whose_path_is_percent_encoded() {
    let (dir, target) = make_repo();
    let repo = dir.path().to_string_lossy().into_owned();
    let encoded = tool_call(
        "call_plan_bad",
        "plan",
        json!({
            "steps": [
                { "step": "double returns n * 2", "check": format!("test -f {repo}%2Flib.lua") }
            ]
        }),
    );
    let good = tool_call(
        "call_plan_good",
        "plan",
        json!({
            "steps": [
                { "step": "double returns n * 2", "check": "grep -qF 'return n * 2' lib.lua" }
            ]
        }),
    );
    let script = vec![
        encoded,
        good,
        edit_turn(&target),
        answer_turn("Plan refiled and done."),
    ];

    let ran = run_fixture(script, &repo, &[("CODING_DONE_TEST", "plan")]).await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");
    says(&ran.stdout, "done=plan");
    // The refiled plan is the one that was stored: one check, and it passed.
    says(&ran.stdout, "plan.total=1 plan.passed=1 plan.filed=true");
    // The refusal cost an iteration nothing: the whole exchange — refused
    // filing, refiling, edit — happened inside the first.
    says(&ran.stdout, "iters=1");
    // And it reached the model, in the terms it has to act on.
    assert!(
        ran.a_later_request_says("percent-encoded"),
        "the refusal was not sent back to the model"
    );
    assert!(
        ran.a_later_request_says("exactly as written"),
        "the refusal did not tell the model what to write instead"
    );
    assert!(
        ran.a_later_request_says("bad_plan"),
        "the refusal was not the filing's own"
    );
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
/// window, so the number is arithmetic rather than a guess: the reply's room
/// — the window less what the request costs, with no `max_tokens` on this
/// conf to cap it lower — less what is kept for the call. The fold's
/// `reserve` is not in the sum: it is what made that room, not a second
/// deduction from it. The conf names the vllm dialect and asks for
/// reasoning, which is what puts `thinking_token_budget` on the wire at all.
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
    const CALL_RESERVE: i64 = 512;

    let ran = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[("CODING_CALL_RESERVE_TEST", &CALL_RESERVE.to_string())],
    )
    .await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");

    let expected = WINDOW - COUNTED - CALL_RESERVE;
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

/// One turn that asks for three reads at once, of files sized so the third
/// crosses the beat's budget: `policy.beat_cap` refuses that one, answers the
/// model with the refusal in the call's place, and the run carries on to the
/// edit and the declaration.
///
/// The decision itself is replayed from a real run's log in
/// `policy/spec/replay_spec.lua`; this is the wiring — that `coding.run` puts
/// the cap on the tools the model calls, over the room it built. The conf
/// names the `openai` dialect so the Port counts by its byte estimate (the
/// mock's `/tokenize` answers a fixed count, under which every result costs
/// the same). With `reserve = 1024` and no `max_tokens` the room's limit is
/// 32768 - 1024 = 31744: one result may take 7936 tokens and one beat's
/// results 15872 together. Each file renders to about 5,800 tokens — under
/// one result's limit, two of them under the beat's, the third over it.
#[tokio::test]
async fn coding_run_refuses_the_read_that_crosses_the_beats_budget_and_carries_on() {
    let (dir, target) = make_repo();
    let names = ["one.lua", "two.lua", "three.lua"];
    let body: String = (0..400)
        .map(|i| format!("-- line {i:04}: {}\n", "x".repeat(30)))
        .collect();
    let mut reads = Vec::new();
    for (n, name) in names.iter().enumerate() {
        let path = dir.path().join(name);
        std::fs::write(&path, &body).expect("write a file to read");
        reads.push((
            format!("call_read_{}", n + 1),
            json!({ "path": path.to_string_lossy() }),
        ));
    }
    let script = vec![
        tool_calls(
            &reads
                .iter()
                .map(|(id, args)| (id.as_str(), "fs_read", args.clone()))
                .collect::<Vec<_>>(),
        ),
        edit_turn(&target),
        answer_turn("Done: double returns n * 2."),
    ];

    let ran = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[
            ("CODING_DIALECT_TEST", "openai"),
            ("CODING_EXTRA_TARGETS_TEST", &names.join(",")),
        ],
    )
    .await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "config.room.beat_budget=15872");
    // Refused, not stopped: the run reaches the edit and converges.
    says(&ran.stdout, "ok=true");
    says(&ran.stdout, "failure_reason=nil");
    assert_eq!(ran.calls, 3, "the reads, the edit, the declaration");

    // What the model was told about the three reads is in the request after
    // them, one tool message per call.
    let answered = |id: &str| -> String {
        ran.bodies[1]["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
            .unwrap_or_else(|| panic!("no tool message for {id} in:\n{:#?}", ran.bodies[1]))
            ["content"]
            .as_str()
            .expect("a tool message's content is text")
            .to_string()
    };
    for id in ["call_read_1", "call_read_2"] {
        let content = answered(id);
        assert!(
            content.contains("line 0399") && !content.contains("beat_budget"),
            "{id} should be the file itself; it was:\n{content}"
        );
    }
    let third = answered("call_read_3");
    assert!(
        third.contains("\"reason\":\"beat_budget\"") && third.contains("\"limit\":15872"),
        "the third read should be the beat_budget refusal; it was:\n{third}"
    );
}

/// The value of the marker line `key=...` the fixture printed.
fn marker<'a>(stdout: &'a str, key: &str) -> &'a str {
    let prefix = format!("{key}=");
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("expected a `{key}=` line; the fixture printed:\n{stdout}"))
}

/// A finished run's events, as `agent-block knl export --as events` prints
/// them: one JSON object a line. The run's session is closed by the time
/// `coding.run` returns and the kernel does not resume a closed stream, so
/// this is how its record reaches a reader after the fact.
fn export_events(knl: &std::path::Path, session: &str) -> String {
    let home = tempdir().expect("tempdir for AGENT_BLOCK_HOME");
    let out = common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["knl", "export", "--session", session, "--as", "events"])
        .args(["--store", &knl.to_string_lossy()])
        .assert()
        .success();
    String::from_utf8_lossy(&out.get_output().stdout).into_owned()
}

/// How many of an export's events are of `kind`.
fn kinds_in(events: &str, kind: &str) -> usize {
    events
        .lines()
        .filter(|line| {
            serde_json::from_str::<Value>(line).expect("an export line is JSON")["kind"] == kind
        })
        .count()
}

/// The state of the files is on the record: one checkpoint before the first
/// beat and one after the beat that landed the edit, and `coding.restore`
/// puts the file back as it was before the run touched it.
///
/// The run is the declare case (an edit, then the answer), against a store
/// the test names so the finished run can be exported. The edit is the only
/// beat that landed one, so two states are recorded — the declaring beat
/// edited nothing and records none. Two contents, two blobs. The restore runs
/// in a second process over the export, as a caller after the fact would.
#[tokio::test]
async fn coding_run_records_the_files_and_restore_puts_the_baseline_back() {
    let (dir, target) = make_repo();
    let store = tempdir().expect("tempdir for the store");
    let knl = store.path().join("knl.sqlite");
    let knl_env = knl.to_string_lossy().into_owned();
    let script = vec![edit_turn(&target), answer_turn("Done.")];

    let ran = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[("AGENT_BLOCK_KNL_PATH", &knl_env)],
    )
    .await;

    says(&ran.stdout, "CODING_MOCK_DONE");
    says(&ran.stdout, "ok=true");
    says(&ran.stdout, "checkpoints=2");
    let after = std::fs::read_to_string(&target).expect("read the target back");
    assert!(after.contains(LIB_AFTER_LINE), "the edit landed:\n{after}");

    let events = export_events(&knl, marker(&ran.stdout, "session"));
    assert_eq!(
        kinds_in(&events, "checkpoint"),
        2,
        "before the first beat, and after the edit"
    );
    assert_eq!(
        kinds_in(&events, "checkpoint_blob"),
        2,
        "the content before, and after"
    );
    let events_file = store.path().join("events.jsonl");
    std::fs::write(&events_file, &events).expect("write the export");

    let restored = tokio::task::spawn_blocking({
        let repo = dir.path().to_string_lossy().into_owned();
        let events_file = events_file.to_string_lossy().into_owned();
        move || {
            let home = tempdir().expect("tempdir for AGENT_BLOCK_HOME");
            let out = common::agent_block_cmd()
                .args(["-s", &common::fixture("coding_openai_mock.lua")])
                .env("CODING_REPO_TEST", &repo)
                .env("CODING_RESTORE_EVENTS_TEST", &events_file)
                .env("CODING_RESTORE_BEAT_TEST", "baseline")
                .env("AGENT_BLOCK_HOME", home.path())
                .assert()
                .success();
            String::from_utf8_lossy(&out.get_output().stdout).into_owned()
        }
    })
    .await
    .expect("the restore process should not panic");

    says(&restored, "CODING_MOCK_RESTORED");
    let beats = marker(&restored, "checkpoint.beats");
    assert!(
        beats.starts_with("baseline,") && beats.split(',').count() == 2,
        "the baseline, then the edit's beat: {beats}"
    );
    says(&restored, &format!("restored={target}"));
    says(&restored, "restore.missing=0");
    assert_eq!(
        std::fs::read_to_string(&target).expect("read the target back"),
        LIB_BEFORE,
        "restore to the baseline puts the original content back"
    );
}

/// `checkpoint = false` records no state of the files: the result has no
/// count and the record has neither kind.
#[tokio::test]
async fn coding_run_records_no_checkpoint_when_turned_off() {
    let (dir, target) = make_repo();
    let store = tempdir().expect("tempdir for the store");
    let knl = store.path().join("knl.sqlite");
    let knl_env = knl.to_string_lossy().into_owned();
    let script = vec![edit_turn(&target), answer_turn("Done.")];

    let ran = run_fixture(
        script,
        &dir.path().to_string_lossy(),
        &[
            ("AGENT_BLOCK_KNL_PATH", &knl_env),
            ("CODING_CHECKPOINT_TEST", "0"),
        ],
    )
    .await;

    says(&ran.stdout, "ok=true");
    says(&ran.stdout, "checkpoints=nil");
    let events = export_events(&knl, marker(&ran.stdout, "session"));
    assert_eq!(kinds_in(&events, "checkpoint"), 0);
    assert_eq!(kinds_in(&events, "checkpoint_blob"), 0);
}

/// One `search_replace` from `search` to `replace` in `target`.
fn replace_turn(id: &str, target: &str, search: &str, replace: &str) -> Value {
    tool_call(
        id,
        "fs_search_replace",
        json!({ "path": target, "edits": [{ "search": search, "replace": replace }] }),
    )
}

/// The events of an export, decoded, in order.
fn decoded(events: &str) -> Vec<Value> {
    events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("an export line is JSON"))
        .collect()
}

/// `coding.fork` from a finished run's export: the child copies the parent's
/// record up to the end of its first beat, finds the file as that beat left
/// it under a fresh directory, and goes on under a different script.
///
/// The parent takes a wrong step first (`n + 1`, red), corrects it (`n * 2`,
/// green) and declares — three beats. The fork is at the first: the child's
/// file reads `n + 1`, its model corrects it its own way and declares. The
/// child's record opens with `forked_from` naming the parent and that beat,
/// carries the parent's first edit and none of its second, and names the
/// child's directory in its last `config`; the parent's file is untouched.
#[tokio::test]
async fn coding_fork_continues_a_finished_run_from_its_first_beat_under_another_script() {
    let (dir, target) = make_repo();
    let store = tempdir().expect("tempdir for the store");
    let knl = store.path().join("knl.sqlite");
    let knl_env = knl.to_string_lossy().into_owned();
    let parent_script = vec![
        replace_turn("call_p1", &target, "    return nil", "    return n + 1"),
        replace_turn("call_p2", &target, "    return n + 1", "    return n * 2"),
        answer_turn("Done."),
    ];
    let parent = run_fixture(
        parent_script,
        &dir.path().to_string_lossy(),
        &[("AGENT_BLOCK_KNL_PATH", &knl_env)],
    )
    .await;
    says(&parent.stdout, "ok=true");
    says(&parent.stdout, "iters=3");
    let parent_session = marker(&parent.stdout, "session").to_string();
    let parent_after = std::fs::read_to_string(&target).expect("read the parent's file");

    let events = export_events(&knl, &parent_session);
    let events_file = store.path().join("parent.jsonl");
    std::fs::write(&events_file, &events).expect("write the export");

    // The child's directory is fresh: the fork writes the file into it. The
    // child's repo is that directory resolved, so the paths the tools are
    // locked to, and the one its config records, are the canonical ones.
    let child_dir = tempdir().expect("tempdir for the child's repo");
    let child_real = child_dir
        .path()
        .canonicalize()
        .expect("canonicalize the child's repo");
    let child_target = child_real.join("lib.lua");
    let child_target = child_target.to_string_lossy().into_owned();
    let child_script = vec![
        replace_turn(
            "call_c1",
            &child_target,
            "    return n + 1",
            "    return n * 2 -- forked",
        ),
        answer_turn("Done, from the fork."),
    ];
    let events_env = events_file.to_string_lossy().into_owned();
    let child = run_fixture(
        child_script,
        &child_dir.path().to_string_lossy(),
        &[
            ("AGENT_BLOCK_KNL_PATH", &knl_env),
            ("CODING_FORK_EVENTS_TEST", &events_env),
            ("CODING_FORK_PARENT_TEST", &parent_session),
        ],
    )
    .await;

    says(&child.stdout, "CODING_MOCK_DONE");
    says(&child.stdout, "ok=true");
    says(&child.stdout, "iters=2");
    assert_eq!(child.calls, 2, "the child's edit, then its answer");
    let beat = marker(&child.stdout, "fork.beat").to_string();
    // The first request the child sent carries the parent's first edit and
    // the note in the seed's place, and not the parent's second edit.
    let first = child.bodies[0].to_string();
    assert!(
        first.contains("return n + 1"),
        "the copied edit is in the request:\n{first}"
    );
    assert!(
        first.contains("This run continues another"),
        "the fork's note is in the request:\n{first}"
    );
    assert!(
        !first.contains("call_p2"),
        "the parent's second beat was not copied:\n{first}"
    );

    let child_file = std::fs::read_to_string(&child_target).expect("read the child's file");
    assert!(
        child_file.contains("return n * 2 -- forked"),
        "the child's own edit, over beat 1's file:\n{child_file}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("read the parent's file again"),
        parent_after,
        "the fork does not touch the parent's file"
    );

    let record = decoded(&export_events(&knl, marker(&child.stdout, "session")));
    let opened = record
        .iter()
        .find(|ev| {
            !matches!(
                ev["kind"].as_str(),
                Some("session_opened" | "budget_granted")
            )
        })
        .expect("the child's record has a caller event");
    assert_eq!(
        opened["kind"], "forked_from",
        "the record opens with its lineage"
    );
    assert_eq!(opened["data"]["session"], parent_session.as_str());
    assert_eq!(opened["data"]["beat"], beat.as_str());
    assert_eq!(opened["data"]["reason"], "e2e");
    let calls: Vec<&str> = record
        .iter()
        .filter(|ev| ev["kind"] == "tool_call")
        .filter_map(|ev| ev["data"]["call_id"].as_str())
        .collect();
    assert_eq!(
        calls,
        ["call_p1", "call_c1"],
        "the parent's first edit, then the child's"
    );
    let repos: Vec<&str> = record
        .iter()
        .filter(|ev| ev["kind"] == "config")
        .filter_map(|ev| ev["data"]["values"]["repo"]["value"].as_str())
        .collect();
    assert_eq!(
        repos.len(),
        2,
        "the parent's config, copied, then the child's"
    );
    assert_eq!(
        *repos.last().expect("a config"),
        child_real.to_string_lossy(),
        "the last config names the child's repo"
    );
}

/// What one fork into `repo` printed, from the parent's export.
async fn fork_into(repo: &std::path::Path, knl: &str, events: &str, parent: &str) -> String {
    run_fixture(
        vec![answer_turn("Done, from the fork.")],
        &repo.to_string_lossy(),
        &[
            ("AGENT_BLOCK_KNL_PATH", knl),
            ("CODING_FORK_EVENTS_TEST", events),
            ("CODING_FORK_PARENT_TEST", parent),
        ],
    )
    .await
    .stdout
}

/// Assert the fork was refused, with `text` in the reason.
fn refused_with(stdout: &str, text: &str) {
    says(stdout, "CODING_MOCK_REFUSED");
    let reason = marker(stdout, "refused");
    assert!(
        reason.contains(text),
        "expected the refusal to say `{text}`; it said: {reason}"
    );
}

/// `coding.fork` never restores into the parent's repo, under whatever name
/// it is handed, and takes any directory that is neither it, in it, nor
/// around it — on the real file system, where symlinks and `..` are real.
///
/// The parent runs in `<base>/repo` (an edit that turns the verify green,
/// then its answer). While that directory holds the file, every alias of it
/// is refused before anything is made — it holds files. A new directory in
/// it, reached directly or through a symlink, is made, resolved, and refused
/// as inside the parent's repo, and left there empty. A sibling whose name
/// starts with the parent's, a nested directory not there yet, and a
/// symlink to an empty directory are taken, the last recorded in the
/// child's config as the directory it points to. Then the parent's file is
/// removed, so an empty alias of it, and a directory around it, reach the
/// comparison — which refuses them.
#[cfg(unix)]
#[tokio::test]
async fn coding_fork_refuses_the_parents_repo_under_any_alias_and_takes_a_sibling() {
    let base_dir = tempdir().expect("tempdir for the layout");
    let base = base_dir
        .path()
        .canonicalize()
        .expect("canonicalize the layout");
    let parent_repo = base.join("repo");
    std::fs::create_dir(&parent_repo).expect("make the parent's repo");
    let target = parent_repo.join("lib.lua");
    std::fs::write(&target, LIB_BEFORE).expect("write the parent's file");
    let target = target.to_string_lossy().into_owned();
    let store = tempdir().expect("tempdir for the store");
    let knl = store.path().join("knl.sqlite");
    let knl_env = knl.to_string_lossy().into_owned();

    let parent = run_fixture(
        vec![edit_turn(&target), answer_turn("Done.")],
        &parent_repo.to_string_lossy(),
        &[("AGENT_BLOCK_KNL_PATH", &knl_env)],
    )
    .await;
    says(&parent.stdout, "ok=true");
    let parent_session = marker(&parent.stdout, "session").to_string();
    let events_file = store.path().join("parent.jsonl");
    std::fs::write(&events_file, export_events(&knl, &parent_session)).expect("write the export");
    let events = events_file.to_string_lossy().into_owned();
    let parent_after = std::fs::read_to_string(&target).expect("read the parent's file");
    let fork = |repo: std::path::PathBuf| {
        let (knl, events, parent) = (knl_env.clone(), events.clone(), parent_session.clone());
        async move { fork_into(&repo, &knl, &events, &parent).await }
    };

    // The parent's repo holds its file: every alias of it stops at step 1.
    let alias = base.join("alias");
    std::os::unix::fs::symlink(&parent_repo, &alias).expect("symlink to the parent's repo");
    let by_dots = base.join("repo").join("..").join("repo");
    let slashed = std::path::PathBuf::from(format!("{}/", parent_repo.display()));
    for repo in [alias.clone(), by_dots, slashed] {
        refused_with(&fork(repo).await, "already holds files");
    }
    let other = base.join("other");
    std::fs::create_dir(&other).expect("make another directory");
    std::fs::write(other.join("notes.txt"), "x").expect("write a file into it");
    refused_with(&fork(other.clone()).await, "already holds files");

    // A new directory in the parent's repo, directly or through the symlink:
    // made, resolved, refused, and left there empty.
    let inside = parent_repo.join("sub");
    refused_with(&fork(inside.clone()).await, "lies inside");
    assert!(
        std::fs::read_dir(&inside)
            .expect("the made directory is there")
            .next()
            .is_none(),
        "the refused directory is left empty"
    );
    refused_with(&fork(alias.join("new")).await, "lies inside");
    assert!(
        parent_repo.join("new").is_dir(),
        "made through the symlink, in the parent's repo"
    );

    // Taken: a sibling named like the parent, a nested directory not there
    // yet, and a symlink to an empty directory — recorded as its target.
    let sibling = base.join("repo2");
    says(&fork(sibling.clone()).await, "ok=true");
    assert_eq!(
        std::fs::read_to_string(sibling.join("lib.lua")).expect("the sibling's file"),
        parent_after
    );
    let nested = base.join("fresh").join("a").join("b");
    says(&fork(nested.clone()).await, "ok=true");
    assert!(
        nested.join("lib.lua").is_file(),
        "restored into the made directory"
    );
    let empty = base.join("empty");
    std::fs::create_dir(&empty).expect("make an empty directory");
    let to_empty = base.join("to-empty");
    std::os::unix::fs::symlink(&empty, &to_empty).expect("symlink to the empty directory");
    let child = fork(to_empty.clone()).await;
    says(&child, "ok=true");
    assert!(
        empty.join("lib.lua").is_file(),
        "restored through the symlink"
    );
    let record = decoded(&export_events(&knl, marker(&child, "session")));
    let repos: Vec<&str> = record
        .iter()
        .filter(|ev| ev["kind"] == "config")
        .filter_map(|ev| ev["data"]["values"]["repo"]["value"].as_str())
        .collect();
    assert_eq!(
        *repos.last().expect("the child's config"),
        empty.to_string_lossy(),
        "the child's repo is the directory the symlink resolves to"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("read the parent's file again"),
        parent_after,
        "no fork touched the parent's file"
    );

    // The parent's file gone, an empty alias and a directory around the
    // parent's repo reach the comparison, and it refuses them.
    std::fs::remove_file(&target).expect("remove the parent's file");
    for dir in [&inside, &parent_repo.join("new")] {
        std::fs::remove_dir(dir).expect("remove the directory a refusal left");
    }
    for dir in [&other, &sibling, &base.join("fresh"), &empty] {
        std::fs::remove_dir_all(dir).expect("clear the layout");
    }
    std::fs::remove_file(&to_empty).expect("remove the symlink");
    refused_with(
        &fork(alias.clone()).await,
        "is the repo a run in this history edited",
    );
    refused_with(
        &fork(base.join("repo").join("..").join("repo")).await,
        "is the repo a run in this history edited",
    );
    refused_with(&fork(base.clone()).await, "contains");
}

/// A symlink planted in an otherwise empty child repo — one to a directory
/// of the parent's repo, one to a parent's file — is refused before the
/// restore writes anything through it, and before it writes any other file.
///
/// The parent's targets are `lib.lua` and `src/x.lua`, so its recorded
/// state has a file at the repo's root and one in a directory. The child
/// with `src -> <parent>/src` is refused on `src/x.lua`; `lib.lua`, which
/// sorts first and lies in the child itself, is not written either — the
/// check covers every file before any is written. The child with `lib.lua ->
/// <parent>/lib.lua` is refused on that file. The parent's files are as the
/// parent left them.
#[cfg(unix)]
#[tokio::test]
async fn coding_fork_refuses_a_symlink_in_the_child_repo_that_leads_out_and_writes_nothing() {
    let base_dir = tempdir().expect("tempdir for the layout");
    let base = base_dir
        .path()
        .canonicalize()
        .expect("canonicalize the layout");
    let parent_repo = base.join("repo");
    std::fs::create_dir_all(parent_repo.join("src")).expect("make the parent's repo");
    let target = parent_repo.join("lib.lua");
    std::fs::write(&target, LIB_BEFORE).expect("write the parent's file");
    let nested = parent_repo.join("src").join("x.lua");
    std::fs::write(&nested, "return 1\n").expect("write the parent's nested file");
    let target = target.to_string_lossy().into_owned();
    let store = tempdir().expect("tempdir for the store");
    let knl = store.path().join("knl.sqlite");
    let knl_env = knl.to_string_lossy().into_owned();
    let extra = ("CODING_EXTRA_TARGETS_TEST", "src/x.lua");

    let parent = run_fixture(
        vec![edit_turn(&target), answer_turn("Done.")],
        &parent_repo.to_string_lossy(),
        &[("AGENT_BLOCK_KNL_PATH", &knl_env), extra],
    )
    .await;
    says(&parent.stdout, "ok=true");
    let parent_session = marker(&parent.stdout, "session").to_string();
    let events_file = store.path().join("parent.jsonl");
    std::fs::write(&events_file, export_events(&knl, &parent_session)).expect("write the export");
    let events = events_file.to_string_lossy().into_owned();
    let parent_lib = std::fs::read_to_string(&target).expect("read the parent's file");
    let parent_nested = std::fs::read_to_string(&nested).expect("read the parent's nested file");
    let fork = |repo: std::path::PathBuf| {
        let (knl, events, parent) = (knl_env.clone(), events.clone(), parent_session.clone());
        async move {
            run_fixture(
                vec![answer_turn("Done, from the fork.")],
                &repo.to_string_lossy(),
                &[
                    ("AGENT_BLOCK_KNL_PATH", &knl),
                    ("CODING_FORK_EVENTS_TEST", &events),
                    ("CODING_FORK_PARENT_TEST", &parent),
                    extra,
                ],
            )
            .await
            .stdout
        }
    };

    // A directory symlink to the parent's `src`.
    let by_dir = base.join("by-dir");
    std::fs::create_dir(&by_dir).expect("make the child's repo");
    std::os::unix::fs::symlink(parent_repo.join("src"), by_dir.join("src"))
        .expect("symlink to the parent's directory");
    let out = fork(by_dir.clone()).await;
    refused_with(&out, "src/x.lua would be written into");
    refused_with(&out, "nothing was restored");
    assert!(
        !by_dir.join("lib.lua").exists(),
        "no file written before the refusal, not even the one inside the child"
    );

    // A file symlink to the parent's `lib.lua`.
    let by_file = base.join("by-file");
    std::fs::create_dir(&by_file).expect("make the child's repo");
    std::os::unix::fs::symlink(&target, by_file.join("lib.lua"))
        .expect("symlink to the parent's file");
    let out = fork(by_file.clone()).await;
    refused_with(&out, "lib.lua leads to");
    assert!(
        !by_file.join("src").join("x.lua").exists(),
        "no file written before the refusal"
    );

    assert_eq!(
        std::fs::read_to_string(&target).expect("the parent's file"),
        parent_lib
    );
    assert_eq!(
        std::fs::read_to_string(&nested).expect("the parent's nested file"),
        parent_nested
    );
}
