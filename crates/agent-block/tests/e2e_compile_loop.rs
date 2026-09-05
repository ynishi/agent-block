mod common;

use agent_block_testkit::server::MockLlm;
use agent_block_testkit::shapes::{anthropic, openai};
use predicates::prelude::*;
use serde_json::json;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

// Note on turn counts. One iteration of compile_loop is one beat — one model
// call plus the tools that call asked for — and the verify runs after it
// whatever the model said. There is no "DONE" turn: the model does not get to
// end the run, the runner does. So a scenario that used to need two calls per
// iteration (ask, then see the result and answer DONE) needs one.

/// Pick (path_a, path_b) from the request-extracted paths, with basename
/// fallbacks (fallbacks should not be hit in normal runs).
fn two_paths(paths: &[String]) -> (String, String) {
    match paths.len() {
        0 => ("file_a.lua".to_string(), "file_b.lua".to_string()),
        1 => (paths[0].clone(), "file_b.lua".to_string()),
        _ => (paths[0].clone(), paths[1].clone()),
    }
}

/// One `fs_edit` tool_use block rewriting the single line of a fixture file.
///
/// The multi-file fixtures write exactly `print("<tag>-old")\n`, so line 1 is
/// the whole address space and `expect` is that line verbatim.
fn fs_edit_line1(id: &str, path: &str, expect: &str, replace: &str) -> serde_json::Value {
    anthropic::tool_use(
        id,
        "fs_edit",
        json!({
            "path": path,
            "edits": [{
                "start_line": 1,
                "end_line": 1,
                "expect": expect,
                "replace": replace
            }]
        }),
    )
}

/// The two `fs_edit` blocks that patch both files (a-old→a-new, b-old→b-new).
fn fs_edit_both(path_a: &str, path_b: &str) -> Vec<serde_json::Value> {
    vec![
        fs_edit_line1(
            "toolu_multi_a",
            path_a,
            "print(\"a-old\")",
            "print(\"a-new\")",
        ),
        fs_edit_line1(
            "toolu_multi_b",
            path_b,
            "print(\"b-old\")",
            "print(\"b-new\")",
        ),
    ]
}

/// Verifies compile_loop in diff mode (edit_mode="diff") with the Anthropic provider.
///
/// Scenario: the model edits through the `fs_edit` tool, gets it wrong once,
/// and recovers.
///   - Iteration 1: an `fs_edit` whose `expect` does not match the file. std.fs
///     refuses it and returns the text actually at those lines. The verify runs
///     anyway and fails.
///   - Iteration 2: an `fs_edit` whose `expect` matches, so the file is written
///     and the runner sees "world".
///
/// Validates that:
///   - The tool-based edit path is wired correctly end to end.
///   - A rejected edit comes back as a tool result the model can recover from,
///     not as a silent skip or a raised error.
///   - The runner is invoked by the loop after each beat, not by the model.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_diff_anthropic_mock_iterates_until_pass() {
    let (base_url, call_count, ct) =
        common::compile_loop_diff_anthropic_mock::spawn_compile_loop_diff_anthropic_mock_server()
            .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("target.lua");
        common::agent_block_cmd()
            .args([
                "-s",
                &common::fixture("compile_loop_diff_anthropic_mock.lua"),
            ])
            .env("ANTHROPIC_BASE_URL_TEST", &url_clone)
            .env(
                "COMPILE_LOOP_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_DIFF_MOCK_PASS"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "two iterations, one call each: the edit that is refused, then the one that lands"
    );
    ct.cancel();
}

/// Verifies that compile_loop iterates exactly twice: once returning broken code
/// (mock_runner fails) and once returning fixed code (mock_runner passes).
///
/// Spawns an in-process OpenAI mock that returns a broken Lua fenced block on
/// the first HTTP request and a fixed Lua fenced block on the second.
/// The Lua fixture's `mock_runner` closure enforces strict fail-then-pass
/// sequencing via a `call_count` upvalue.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
/// `OPENAI_API_KEY` is not set; `api_key="dummy"` is injected as a literal.
#[tokio::test]
async fn compile_loop_openai_mock_iterates_until_pass() {
    let (base_url, call_count, ct) =
        common::compile_loop_openai_mock::spawn_compile_loop_openai_mock_server().await;
    // Give the server a moment to start accepting connections before the subprocess runs.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || {
        // Safety: tempdir() panics only on OS-level temp directory failure,
        // which is a fatal test infra condition, not a recoverable error.
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("target.lua");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("compile_loop_openai_mock.lua")])
            .env("OPENAI_BASE_URL_TEST", &url_clone)
            .env(
                "COMPILE_LOOP_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_MOCK_PASS"));
    })
    .await
    // Safety: spawn_blocking does not panic on its own; any panic would come from
    // the assertion block above failing, which we want to propagate.
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "expected exactly 2 HTTP calls to the mock (turn 1: broken, turn 2: fixed)"
    );
    ct.cancel();
}

/// Verifies that compile_loop iterates exactly twice with the Anthropic provider:
/// once returning broken code (mock_runner fails) and once returning fixed code
/// (mock_runner passes).
///
/// Validates that the fixture's `base_url` reaches the provider Port: if
/// `conf.llm` were not forwarded verbatim, the request would not reach the mock
/// and the test would fail.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
/// `ANTHROPIC_API_KEY` is not set; `api_key="dummy"` is injected as a literal.
#[tokio::test]
async fn compile_loop_anthropic_mock_iterates_until_pass() {
    let (base_url, call_count, ct) =
        common::compile_loop_anthropic_mock::spawn_compile_loop_anthropic_mock_server().await;
    // Give the server a moment to start accepting connections before the subprocess runs.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || {
        // Safety: tempdir() panics only on OS-level temp directory failure,
        // which is a fatal test infra condition, not a recoverable error.
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("target.lua");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("compile_loop_anthropic_mock.lua")])
            .env("ANTHROPIC_BASE_URL_TEST", &url_clone)
            .env(
                "COMPILE_LOOP_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_MOCK_PASS"));
    })
    .await
    // Safety: spawn_blocking does not panic on its own; any panic would come from
    // the assertion block above failing, which we want to propagate.
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "expected exactly 2 HTTP calls to the anthropic mock"
    );
    ct.cancel();
}

/// Verifies compile_loop in multi-file diff mode (happy path, 1 iteration, 2 files).
///
/// Scenario: one iteration, one LLM call. The mock returns one `fs_edit`
/// tool_use block per file in a single turn; both apply, the loop verifies, and
/// the runner receives the paths list.
///
/// Validates:
///   - target_files list is accepted.
///   - Each fs_edit is routed to the path it names, so both files change.
///   - Runner is called with a list of paths, not a single string.
///   - result.modified_files contains 2 paths; result.artifact_path is nil.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_diff_multi_anthropic_mock_iterates_until_pass() {
    let handle = MockLlm::anthropic(|req| {
        let (path_a, path_b) = two_paths(&req.paths);
        if req.call_index == 0 {
            anthropic::tool_use_response(fs_edit_both(&path_a, &path_b))
        } else {
            // Not reached on the happy path; a second call would mean the
            // first iteration did not converge.
            anthropic::text_response("DONE")
        }
    })
    .spawn()
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = handle.base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let file_a = tmp.path().join("file_a.lua");
        let file_b = tmp.path().join("file_b.lua");
        common::agent_block_cmd()
            .args([
                "-s",
                &common::fixture("compile_loop_diff_multi_anthropic_mock.lua"),
            ])
            .env("ANTHROPIC_BASE_URL_TEST", &url_clone)
            .env(
                "COMPILE_LOOP_TARGET_FILES",
                format!(
                    "{}:{}",
                    file_a.to_str().expect("utf8 path"),
                    file_b.to_str().expect("utf8 path")
                ),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "COMPILE_LOOP_DIFF_MULTI_MOCK_PASS",
            ));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        handle.state.call_count(),
        1,
        "one iteration: both files are patched in one turn and the verify passes"
    );
    handle.ct.cancel();
}

/// Verifies the fs_edit write-channel tool.
///
/// Scenario: one iteration, one LLM call returning two fs_edit tool_use blocks.
/// Both edits are applied and written to disk, and the verify then passes.
///
/// Validates:
///   - tool_mode default "auto" declares fs_edit (asserted via mock state).
///   - Tool-channel edits count as applied edits (no zero-edit retry).
///   - result.modified_files carries the tool-written paths.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_fs_edit_tool_converges() {
    let handle = MockLlm::anthropic(|req| {
        let (path_a, path_b) = two_paths(&req.paths);
        if req.call_index == 0 {
            anthropic::tool_use_response(vec![
                anthropic::tool_use(
                    "toolu_asr_1",
                    "fs_edit",
                    json!({"path": path_a, "edits": [{
                        "start_line": 1, "end_line": 1,
                        "expect": "print(\"a-old\")", "replace": "print(\"a-new\")"
                    }]}),
                ),
                anthropic::tool_use(
                    "toolu_asr_2",
                    "fs_edit",
                    json!({"path": path_b, "edits": [{
                        "start_line": 1, "end_line": 1,
                        "expect": "print(\"b-old\")", "replace": "print(\"b-new\")"
                    }]}),
                ),
            ])
        } else {
            anthropic::text_response("DONE")
        }
    })
    .spawn()
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = handle.base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let file_a = tmp.path().join("file_a.lua");
        let file_b = tmp.path().join("file_b.lua");
        common::agent_block_cmd()
            .args([
                "-s",
                &common::fixture("compile_loop_asr_anthropic_mock.lua"),
            ])
            .env("ANTHROPIC_BASE_URL_TEST", &url_clone)
            .env(
                "COMPILE_LOOP_TARGET_FILES",
                format!(
                    "{}:{}",
                    file_a.to_str().expect("utf8 path"),
                    file_b.to_str().expect("utf8 path")
                ),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_ASR_MOCK_PASS"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        handle.state.call_count(),
        1,
        "expected exactly 1 HTTP call (both edits in one turn, then the verify)"
    );
    assert!(
        handle.state.declared_count_of("fs_edit") >= 1,
        "tool_mode=auto must declare the fs_edit tool in the request"
    );
    handle.ct.cancel();
}

/// Verifies wire-shape tolerance for the "broken OpenAI" tool_calls form.
///
/// Observed on OpenAI-compatible stacks in the wild (Ollama native leak-through,
/// Gemini functionCall.args, some vLLM tool-call parsers):
///   * function.arguments arrives as a JSON *object* instead of a string.
///   * the id field is absent.
///
/// Scenario: two iterations, one call each.
///   - Iteration 1: two fs_edit tool_calls in the broken shape, with an
///     `expect` that does not match. std.fs rejects both, so the loop's next
///     request has to carry two tool results back — which is where the
///     synthesized ids have to survive.
///   - Iteration 2: the same two calls with a matching `expect`; both apply and
///     the verify passes.
///
/// The second request must carry `role="tool"` messages whose `tool_call_id` is
/// a synthesized 9-character id (Mistral's chat template rejects any other
/// shape), and the two must be distinct.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_broken_openai_tool_calls_shape_converges() {
    let handle = MockLlm::openai(|req| {
        let (path_a, path_b) = two_paths(&req.paths);
        // Iteration 1 aims at text that is not there, so both edits are
        // rejected and their results go back in the next request.
        let (expect_a, expect_b) = if req.call_index == 0 {
            ("print(\"WRONG-a\")", "print(\"WRONG-b\")")
        } else {
            ("print(\"a-old\")", "print(\"b-old\")")
        };
        openai::tool_calls_response(vec![
            openai::tool_call_object_args_no_id(
                "fs_edit",
                json!({"path": path_a, "edits": [{
                    "start_line": 1, "end_line": 1,
                    "expect": expect_a, "replace": "print(\"a-new\")"
                }]}),
            ),
            openai::tool_call_object_args_no_id(
                "fs_edit",
                json!({"path": path_b, "edits": [{
                    "start_line": 1, "end_line": 1,
                    "expect": expect_b, "replace": "print(\"b-new\")"
                }]}),
            ),
        ])
    })
    .spawn()
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = handle.base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let file_a = tmp.path().join("file_a.lua");
        let file_b = tmp.path().join("file_b.lua");
        common::agent_block_cmd()
            .args([
                "-s",
                &common::fixture("compile_loop_broken_openai_mock.lua"),
            ])
            .env("OPENAI_BASE_URL_TEST", &url_clone)
            .env(
                "COMPILE_LOOP_TARGET_FILES",
                format!(
                    "{}:{}",
                    file_a.to_str().expect("utf8 path"),
                    file_b.to_str().expect("utf8 path")
                ),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "COMPILE_LOOP_BROKEN_OPENAI_MOCK_PASS",
            ));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        handle.state.call_count(),
        2,
        "expected exactly 2 HTTP calls (rejected broken-shape edits, then the ones that land)"
    );
    let ids = handle.state.tool_result_ids();
    assert!(
        ids.len() >= 2,
        "expected >=2 role=tool messages carrying tool results, got {ids:?}"
    );
    // Synthesized ids are exactly 9 alphanumerics: Mistral's chat template
    // rejects any other shape when the id comes back on a tool result.
    assert!(
        ids.iter()
            .all(|id| id.len() == 9 && id.chars().all(|c| c.is_ascii_alphanumeric())),
        "every role=tool message must carry a synthesized 9-char alphanumeric id, got {ids:?}"
    );
    assert_ne!(
        ids[0], ids[1],
        "two tool calls in one turn must get distinct synthesized ids, got {ids:?}"
    );
    handle.ct.cancel();
}

/// Verifies compile_loop in multi-file diff mode converges after a rejected edit.
///
/// Scenario: two iterations, one call each.
///   - Iteration 1: one `fs_edit` on file_a whose `expect` does not match.
///     std.fs rejects it, so the iteration applies zero edits; the verify runs
///     and fails, and the rejection is carried into iteration 2.
///   - Iteration 2: correct `fs_edit`s for both files → both apply → the verify
///     passes.
///
/// Validates:
///   - A rejected edit triggers another iteration (not a silent skip).
///   - A zero-edit iteration does not end the run.
///   - result.modified_files contains 2 paths; result.artifact_path is nil.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_diff_multi_anthropic_mock_two_iter_converges() {
    let handle = MockLlm::anthropic(|req| {
        let (path_a, path_b) = two_paths(&req.paths);
        if req.call_index == 0 {
            // Iteration 1: file_a only, and `expect` is not what line 1 holds.
            anthropic::tool_use_response(vec![fs_edit_line1(
                "toolu_multi_wrong",
                &path_a,
                "print(\"WRONG\")",
                "print(\"a-new\")",
            )])
        } else {
            anthropic::tool_use_response(fs_edit_both(&path_a, &path_b))
        }
    })
    .spawn()
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = handle.base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let file_a = tmp.path().join("file_a.lua");
        let file_b = tmp.path().join("file_b.lua");
        common::agent_block_cmd()
            .args([
                "-s",
                &common::fixture("compile_loop_diff_multi_anthropic_mock_two_iter.lua"),
            ])
            .env("ANTHROPIC_BASE_URL_TEST", &url_clone)
            .env(
                "COMPILE_LOOP_TARGET_FILES",
                format!(
                    "{}:{}",
                    file_a.to_str().expect("utf8 path"),
                    file_b.to_str().expect("utf8 path")
                ),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "COMPILE_LOOP_DIFF_MULTI_MOCK_TWO_ITER_PASS",
            ));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        handle.state.call_count(),
        2,
        "two iterations, one call each: the rejected edit, then the two that apply"
    );
    handle.ct.cancel();
}

/// Verifies the read tools against a file that is too large to send whole.
///
/// Scenario (Anthropic mock, 3 iterations):
///   - Iteration 1: `fs_read` on the whole file. It is over the size threshold,
///     so the answer names the file's length and points at `read_file_range` —
///     there is no digest and no summarising sub-call.
///   - Iteration 2: `read_file_range(path, 10, 20)` hands back those lines
///     verbatim and line-numbered, whatever the file's size.
///   - Iteration 3: `fs_edit` on the marker line; the verify then passes.
///
/// No `#[ignore]` — runs under plain `cargo test` with no real API keys.
#[tokio::test]
async fn compile_loop_reads_a_large_file_by_range() {
    let (addr, state) = common::compile_loop_range_mock::spawn_range_mock().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let base_url = format!("http://{addr}");
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("range_target.lua");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("compile_loop_range_mock.lua")])
            .env("ANTHROPIC_BASE_URL_TEST", &base_url)
            .env(
                "COMPILE_LOOP_RANGE_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("READ_FILE_RANGE_VERBATIM_PASS"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    let results = state.tool_result_texts();
    assert!(
        results.len() >= 2,
        "expected the read and the range results, got {results:?}"
    );
    // The whole-file read is refused with the file's length and where to go
    // instead — the answer that replaced the distilled digest.
    assert!(
        results[0].contains("too large: 600 lines") && results[0].contains("read_file_range"),
        "fs_read on an oversized file must answer its length and point at the range read, got {:?}",
        results[0]
    );
    // The fixture writes "-- verbatim-line-NN" on lines 10..=20; the handler
    // returns those lines untouched, line-numbered for fs_edit to address.
    let expected_range = (10..=20)
        .map(|n| format!("{n}\t-- verbatim-line-{n:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        results[1], expected_range,
        "read_file_range must hand back lines 10-20 verbatim and line-numbered"
    );

    assert_eq!(
        state.call_count.load(Ordering::SeqCst),
        3,
        "expected exactly 3 calls (read, range, edit) — one per iteration"
    );
}

/// Verifies that compile_loop with the OpenAI provider converges after exactly 3 turns
/// (broken1 → broken2 → fixed) and that the same input sequence produces the same
/// output sequence across two independent subprocess runs (deterministic across runs).
///
/// ## Scenario
/// - Turn 1: mock returns broken Lua code A; mock_runner returns {ok=false, stderr="iter 1"}.
/// - Turn 2: mock returns broken Lua code B (different from A to avoid early stagnation);
///           mock_runner returns {ok=false, stderr="iter 2"}.
/// - Turn 3: mock returns fixed Lua code; mock_runner returns {ok=true}.
///
/// ## Determinism check
/// The test spawns two subprocesses against the same mock server:
///   Run 1: assert call_count == 3 and stdout contains "COMPILE_LOOP_MOCK_PASS".
///   call_count.store(0, SeqCst) — reset between runs.
///   Run 2: assert call_count == 3 and stdout contains "COMPILE_LOOP_MOCK_PASS".
/// Both runs passing with identical call counts demonstrates that identical input
/// sequences produce identical tool-call sequences across runs.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
/// api_key is always "dummy" (OPENAI_API_KEY is never read).
#[tokio::test]
async fn compile_loop_openai_mock_three_turn_converges() {
    let (base_url, call_count, ct) = common::compile_loop_openai_mock_three_turn::spawn_compile_loop_openai_mock_three_turn_server().await;
    // Give the server a moment to start accepting connections before the subprocess runs.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // --- Run 1 ---
    let url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("target.lua");
        common::agent_block_cmd()
            .args([
                "-s",
                &common::fixture("compile_loop_openai_mock_three_turn.lua"),
            ])
            .env("OPENAI_BASE_URL_TEST", &url_clone)
            .env(
                "COMPILE_LOOP_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_MOCK_PASS"));
    })
    .await
    .expect("subprocess assertion task (run 1) should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        3,
        "run 1: expected exactly 3 HTTP calls to the 3-turn mock (broken1, broken2, fixed)"
    );

    // Reset between runs — AtomicUsize store is safe here: run 1 subprocess has exited.
    call_count.store(0, Ordering::SeqCst);

    // --- Run 2 (deterministic check: same input → same output sequence) ---
    let url_clone2 = base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("target.lua");
        common::agent_block_cmd()
            .args([
                "-s",
                &common::fixture("compile_loop_openai_mock_three_turn.lua"),
            ])
            .env("OPENAI_BASE_URL_TEST", &url_clone2)
            .env(
                "COMPILE_LOOP_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_MOCK_PASS"));
    })
    .await
    .expect("subprocess assertion task (run 2) should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        3,
        "run 2: expected exactly 3 HTTP calls to the 3-turn mock (broken1, broken2, fixed)"
    );

    ct.cancel();
}

/// `conf.extra_tools` must reach the model and be dispatched.
///
/// The mock calls `get_hint` before editing, so a run where extra tools stopped
/// being declared fails here rather than looking like a model that chose not to
/// use them.
#[tokio::test]
async fn compile_loop_extra_tools_are_declared_and_dispatched() {
    let handle = MockLlm::anthropic(|req| {
        let declared_hint = req
            .body
            .to_string()
            .contains("Return the replacement the spec is asking for");
        if !declared_hint {
            // Fail loudly rather than converging without the tool.
            return anthropic::text_response("get_hint was not declared");
        }
        if req.call_index == 0 {
            anthropic::tool_use_response(vec![anthropic::tool_use(
                "toolu_hint",
                "get_hint",
                json!({}),
            )])
        } else {
            let path = req.paths.first().cloned().unwrap_or_default();
            anthropic::tool_use_response(vec![anthropic::tool_use(
                "toolu_edit",
                "fs_edit",
                json!({"path": path, "edits": [{
                    "start_line": 1, "end_line": 1,
                    "expect": "print(\"hello\")", "replace": "print(\"world\")"
                }]}),
            )])
        }
    })
    .spawn()
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = handle.base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target = tmp.path().join("target.lua");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("compile_loop_extra_tools_mock.lua")])
            .env("ANTHROPIC_BASE_URL_TEST", &url)
            .env("COMPILE_LOOP_TARGET", target.to_str().expect("utf8 path"))
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("[XT] ok=true"))
            .stdout(predicate::str::contains("[XT] hint_calls=1"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    handle.ct.cancel();
}

/// `tool_mode = "read_only"` withholds the edit tool.
///
/// A read-only run can inspect and cannot converge, so what is asserted is the
/// tool set the request declared and the give-up that follows from it.
#[tokio::test]
async fn compile_loop_read_only_withholds_the_edit_tool() {
    let handle = MockLlm::anthropic(|_req| anthropic::text_response("I would edit line 1."))
        .spawn()
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = handle.base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target = tmp.path().join("target.lua");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("compile_loop_read_only_mock.lua")])
            .env("ANTHROPIC_BASE_URL_TEST", &url)
            .env("COMPILE_LOOP_TARGET", target.to_str().expect("utf8 path"))
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("[RO] ok=false"))
            .stdout(predicate::str::contains("[RO] failure_reason=max_iters"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        handle.state.declared_count_of("fs_edit"),
        0,
        "read_only must not declare the edit tool: {:?}",
        handle.state.declared_tool_names()
    );
    assert!(
        handle.state.declared_count_of("fs_read") >= 1,
        "read_only still declares the reads: {:?}",
        handle.state.declared_tool_names()
    );
    handle.ct.cancel();
}

/// The run stops when the verify keeps saying the same thing.
///
/// Each iteration lands a real edit, so the "no edits applied" counter never
/// trips; what fires is `policy.stagnation` over the verify's stderr, which is
/// identical every time. Three is the count at which "again" stops being a
/// retry.
#[tokio::test]
async fn compile_loop_stops_when_the_verify_repeats_itself() {
    let handle = MockLlm::anthropic(|req| {
        // One line per iteration, so every iteration applies an edit and the
        // signature the policy compares is the verify's, not the edit's.
        let path = req.paths.first().cloned().unwrap_or_default();
        let line = req.call_index + 1;
        anthropic::tool_use_response(vec![anthropic::tool_use(
            &format!("toolu_stag_{line}"),
            "fs_edit",
            json!({"path": path, "edits": [{
                "start_line": line, "end_line": line,
                "expect": format!("line-{line}"), "replace": format!("edited-{line}")
            }]}),
        )])
    })
    .spawn()
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = handle.base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target = tmp.path().join("target.lua");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("compile_loop_stagnation_mock.lua")])
            .env("ANTHROPIC_BASE_URL_TEST", &url)
            .env("COMPILE_LOOP_TARGET", target.to_str().expect("utf8 path"))
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("[STAG] ok=false"))
            .stdout(predicate::str::contains("[STAG] failure_reason=stagnation"))
            .stdout(predicate::str::contains("[STAG] iters=3"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        handle.state.call_count(),
        3,
        "the run gives up on the third identical verify, well inside its budget"
    );
    handle.ct.cancel();
}
