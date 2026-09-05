mod common;

use agent_block_testkit::server::MockLlm;
use agent_block_testkit::shapes::{anthropic, openai};
use predicates::prelude::*;
use serde_json::json;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

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
///   - Turn 1: an `fs_edit` whose `expect` does not match the file. std.fs
///     refuses it and returns the text actually at those lines.
///   - Turn 2+: an `fs_edit` whose `expect` matches, so the file is written and
///     the runner sees "world".
///
/// Validates that:
///   - The tool-based edit path is wired correctly end to end.
///   - A rejected edit comes back as a tool result the model can recover from,
///     not as a silent skip or a raised error.
///   - The runner is invoked by the loop after the turns, not by the model.
///   - Every tool call is logged with the iteration it belongs to. The tool set
///     is built once, outside the iteration loop, so this correlation is easy
///     to lose and nothing else would notice.
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
            .env("RUST_LOG", "info")
            // compile_loop's obs trail is gated on the dump mode.
            .env("AGENT_BLOCK_LLM_DUMP", "meta")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_DIFF_MOCK_PASS"))
            // the refusal is observed, named, and attributed to its iteration
            .stdout(predicate::str::contains(
                "event=tool_use_fail component=compile_loop iter=1",
            ))
            .stdout(predicate::str::contains("tool=fs_edit err=expect_mismatch"))
            // and the write that lands belongs to the next iteration, not the
            // one that was refused
            .stdout(predicate::str::contains(
                "event=tool_use component=compile_loop iter=2",
            ))
            .stdout(predicate::str::contains("tool=fs_edit ok=true"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        4,
        "two iterations, two calls each: the model asks for the edit, then sees \
         its result and answers DONE"
    );
    ct.cancel();
}

/// Verifies that compile_loop iterates exactly twice: once returning broken code
/// (mock_runner fails) and once returning fixed code (mock_runner passes).
///
/// Spawns an in-process OpenAI mock that returns a broken Lua fenced block on
/// the first HTTP request and a fixed Lua fenced block on the second.
/// The Lua fixture's `mock_runner` closure enforces strict fail-then-pass
/// sequencing via a `call_count` upvalue (Crux #2).
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys (Crux #3).
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
/// Spawns an in-process Anthropic mock that returns a broken Lua fenced block on
/// the first POST /v1/messages request and a fixed Lua fenced block on the second.
/// The Lua fixture's `mock_runner` closure enforces strict fail-then-pass
/// sequencing via a `call_count` upvalue (Crux #2).
///
/// Validates Crux #1: the fixture supplies `base_url` from `ANTHROPIC_BASE_URL_TEST`;
/// if `blocks/tools/compile_loop/init.lua` did not forward `opts.base_url` to the Anthropic
/// client (ST1 fix), the request would not reach the mock and the test would fail.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys (Crux #3).
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
/// Scenario: 1 iteration, 2 LLM calls.
///   - Call 1: mock returns one `fs_edit` tool_use block per file, both in a single turn.
///     Both edits apply, so both files are written.
///   - Call 2: the model sees the edit results and answers DONE, ending the tool loop.
///     compile_loop then verifies → mock_runner receives the paths list → ok=true.
///
/// Validates:
///   - target_files list is accepted (Crux #2 backward-compatible conf API).
///   - Each fs_edit is routed to the path it names, so both files change (Crux #1).
///   - Runner is called with a list of paths, not a single string (Crux #3 signature toggle).
///   - result.modified_files contains 2 paths; result.artifact_path is nil.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_diff_multi_anthropic_mock_iterates_until_pass() {
    let handle = MockLlm::anthropic(|req| {
        if req.has_tool_results {
            anthropic::text_response("DONE")
        } else {
            let (path_a, path_b) = two_paths(&req.paths);
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

    // One iteration, two calls: both files are patched in a single tool turn,
    // then the model sees the two edit results and answers DONE.
    assert_eq!(
        handle.state.call_count(),
        2,
        "expected exactly 2 HTTP calls to the multi diff mock (2 fs_edits in 1 turn, then DONE)"
    );
    handle.ct.cancel();
}

/// Verifies the fs_edit write-channel tool.
///
/// Scenario: 1 iteration, 2 LLM calls.
///   - Call 1: mock returns two fs_edit tool_use blocks (file_a, file_b).
///     compile_loop applies both edits via the tool handler and writes them to disk.
///   - Call 2: mock returns the plain text "DONE" (no SR blocks). Because tool-channel
///     edits were applied this iter, the loop proceeds to verify instead of treating
///     the missing SR text as a parse failure → mock_runner sees "new" → ok=true.
///
/// Validates:
///   - tool_mode default "auto" declares fs_edit (asserted via mock state).
///   - Tool-channel edits count as applied edits (no zero-edit retry / no dead end).
///   - result.modified_files carries the tool-written paths.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_fs_edit_tool_converges() {
    let handle = MockLlm::anthropic(|req| {
        if !req.has_tool_results {
            let (path_a, path_b) = two_paths(&req.paths);
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
        2,
        "expected exactly 2 HTTP calls (tool_use turn + DONE turn)"
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
/// Scenario: 1 iteration, 2 LLM calls.
///   - Call 1: two fs_edit tool_calls (object args, no ids).
///     cl_oai_normalize must accept the object arguments and synthesize
///     deterministic call_synth_N ids.
///   - Call 2: request must carry role="tool" messages whose tool_call_id
///     starts with "call_synth_" (asserted via mock state); mock replies "DONE"
///     → loop proceeds to verify via tool-channel edits → ok=true.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_broken_openai_tool_calls_shape_converges() {
    let handle = MockLlm::openai(|req| {
        if !req.has_tool_results {
            let (path_a, path_b) = two_paths(&req.paths);
            openai::tool_calls_response(vec![
                openai::tool_call_object_args_no_id(
                    "fs_edit",
                    json!({"path": path_a, "edits": [{
                        "start_line": 1, "end_line": 1,
                        "expect": "print(\"a-old\")", "replace": "print(\"a-new\")"
                    }]}),
                ),
                openai::tool_call_object_args_no_id(
                    "fs_edit",
                    json!({"path": path_b, "edits": [{
                        "start_line": 1, "end_line": 1,
                        "expect": "print(\"b-old\")", "replace": "print(\"b-new\")"
                    }]}),
                ),
            ])
        } else {
            openai::text_response("DONE")
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
        "expected exactly 2 HTTP calls (broken tool_calls turn + DONE turn)"
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
    handle.ct.cancel();
}

/// Verifies compile_loop in multi-file diff mode converges after a rejected edit (2-iter).
///
/// Scenario: 2 iterations, 2 LLM calls each.
///   - Iter 1, call 1: one `fs_edit` on file_a whose `expect` does not match the
///     file. std.fs rejects it, so the iteration applies zero edits.
///     Call 2: the model answers DONE, ending the tool loop. compile_loop sees
///     zero edits, skips the runner, and carries the rejection into iter 2.
///   - Iter 2, call 3: correct `fs_edit` for both files → both apply.
///     Call 4: DONE → compile_loop verifies → ok=true.
///
/// Validates:
///   - A rejected edit in multi-file mode triggers a retry (not a silent skip).
///   - A zero-edit iteration does not end the run.
///   - result.modified_files contains 2 paths; result.artifact_path is nil.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_diff_multi_anthropic_mock_two_iter_converges() {
    let handle = MockLlm::anthropic(|req| {
        if req.has_tool_results {
            // The edit result is in hand; one edit per iteration is enough.
            return anthropic::text_response("DONE");
        }
        let (path_a, path_b) = two_paths(&req.paths);
        if req.call_index == 0 {
            // Iter 1: file_a only, and `expect` is not what line 1 holds.
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

    // 2 iterations, 2 calls each: the model asks for an edit, then sees its
    // result and answers DONE. Iter 1's edit is rejected, iter 2's applies.
    assert_eq!(
        handle.state.call_count(),
        4,
        "expected exactly 4 HTTP calls to the multi diff mock (iter1: edit-rejected + DONE, \
         iter2: edits applied + DONE)"
    );
    handle.ct.cancel();
}

/// Verifies the compile_loop distill subloop with an OpenAI provider mock.
///
/// Scenario (3-turn per iter, with distill LLM calls interleaved):
///   Turn 0 (main, with tools):   mock returns tool_use=read_file for the target file.
///                                 compile_loop dispatches read_file → size > threshold →
///                                 calls distill_subloop → N HTTP calls to mock (no tools).
///   Turn 1 (distill, no tools):  mock returns short text summaries.
///                                 distill_call_count incremented per chunk.
///   Turn 2 (main, with tools + tool results): mock returns SR pass block.
///                                 compile_loop applies SR → mock_runner returns ok=true.
///
/// Asserts:
///   - COMPILE_LOOP_DISTILL_MOCK_PASS received (loop converged).
///   - distill_call_count > 0 (distill subloop was triggered).
///   - received_distill_body has no `tools` key (BC5: provider-agnostic distill).
///
/// No `#[ignore]` — runs under plain `cargo test` with no real API keys.
#[tokio::test]
async fn compile_loop_distill_openai_mock_iterates_until_pass() {
    let (addr, state) = common::compile_loop_distill_mock::spawn_distill_mock("openai").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let base_url = format!("http://{addr}");
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("distill_target.lua");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("compile_loop_distill_mock.lua")])
            .env("OPENAI_BASE_URL_TEST", &base_url)
            .env("DISTILL_MOCK_PROVIDER", "openai")
            .env(
                "COMPILE_LOOP_DISTILL_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_DISTILL_MOCK_PASS"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert!(
        state.distill_call_count.load(Ordering::SeqCst) > 0,
        "distill_call_count must be > 0: distill subloop was not triggered"
    );

    let body_guard = state.received_distill_body.lock().unwrap();
    let distill_body = body_guard
        .as_ref()
        .expect("received_distill_body must be set after distill call");
    assert!(
        distill_body.get("tools").is_none(),
        "BC5: distill LLM call must not include `tools` field in request body"
    );
}

/// Verifies the compile_loop distill subloop with an Anthropic provider mock.
///
/// Same scenario as `compile_loop_distill_openai_mock_iterates_until_pass` but
/// using the Anthropic Messages API schema. Confirms provider-agnostic distill
/// (crux-card §2: distill uses the same call path regardless of provider).
///
/// Asserts:
///   - COMPILE_LOOP_DISTILL_MOCK_PASS received.
///   - distill_call_count > 0.
///   - received_distill_body has no `tools` key (BC5).
///
/// No `#[ignore]` — runs under plain `cargo test` with no real API keys.
#[tokio::test]
async fn compile_loop_distill_anthropic_mock_iterates_until_pass() {
    let (addr, state) = common::compile_loop_distill_mock::spawn_distill_mock("anthropic").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let base_url = format!("http://{addr}");
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("distill_target.lua");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("compile_loop_distill_mock.lua")])
            .env("ANTHROPIC_BASE_URL_TEST", &base_url)
            .env("DISTILL_MOCK_PROVIDER", "anthropic")
            .env(
                "COMPILE_LOOP_DISTILL_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_DISTILL_MOCK_PASS"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert!(
        state.distill_call_count.load(Ordering::SeqCst) > 0,
        "distill_call_count must be > 0: distill subloop was not triggered"
    );

    let body_guard = state.received_distill_body.lock().unwrap();
    let distill_body = body_guard
        .as_ref()
        .expect("received_distill_body must be set after distill call");
    assert!(
        distill_body.get("tools").is_none(),
        "BC5: distill LLM call must not include `tools` field in request body"
    );
}

/// Verifies that read_file_range returns verbatim source lines without distillation
/// even when the target file exceeds READ_FILE_FULL_THRESHOLD (crux-card §3).
///
/// Scenario (Anthropic mock, 3 turns):
///   Turn 0: mock returns tool_use=read_file_range(path, 10, 20).
///           compile_loop dispatches to read_file_range_tool_handler.
///           Handler reads lines 10-20 verbatim (no distill path).
///   Turn 1: mock returns tool_use=fs_edit (REPLACE_ME → DONE) on the marker line.
///   Turn 2: mock returns "DONE" after the edit result, ending the tool loop.
///           compile_loop then verifies → mock_runner returns ok=true.
///
/// Asserts:
///   - READ_FILE_RANGE_VERBATIM_PASS received (loop converged using range access).
///   - The range tool result is the requested lines verbatim, each prefixed with
///     its 1-based line number — the numbers fs_edit addresses lines by.
///   - call_count == 3 (exactly 3 main LLM calls, no distill interleaved).
///
/// No `#[ignore]` — runs under plain `cargo test` with no real API keys.
#[tokio::test]
async fn compile_loop_read_file_range_verbatim() {
    let (addr, state) = common::compile_loop_distill_mock::spawn_range_mock().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let base_url = format!("http://{addr}");
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("range_target.lua");
        common::agent_block_cmd()
            .args([
                "-s",
                &common::fixture("compile_loop_distill_range_mock.lua"),
            ])
            .env("ANTHROPIC_BASE_URL_TEST", &base_url)
            .env(
                "COMPILE_LOOP_RANGE_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("READ_FILE_RANGE_VERBATIM_PASS"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    // The fixture writes "-- verbatim-line-NN" on lines 10..=20; the handler
    // returns those lines untouched, line-numbered for fs_edit to address.
    let expected_range = (10..=20)
        .map(|n| format!("{n}\t-- verbatim-line-{n:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let results = state.tool_result_texts();
    assert_eq!(
        results.first().map(String::as_str),
        Some(expected_range.as_str()),
        "read_file_range must hand back lines 10-20 verbatim and line-numbered, got {:?}",
        results.first()
    );

    // Range mock: exactly 3 main calls (read_file_range, fs_edit, DONE), no distill calls.
    assert_eq!(
        state.call_count.load(Ordering::SeqCst),
        3,
        "expected exactly 3 HTTP calls to the range mock \
         (turn 0: read_file_range, turn 1: fs_edit, turn 2: DONE)"
    );
    assert_eq!(
        state.distill_call_count.load(Ordering::SeqCst),
        0,
        "range mock must not trigger distill subloop (read_file_range bypasses distill)"
    );
}

/// Verifies that compile_loop with the OpenAI provider converges after exactly 3 turns
/// (broken1 → broken2 → fixed) and that the same input sequence produces the same
/// output sequence across two independent subprocess runs (Crux: deterministic across runs).
///
/// ## Scenario
/// - Turn 1: mock returns broken Lua code A; mock_runner returns {ok=false, stderr="iter 1"}.
/// - Turn 2: mock returns broken Lua code B (different from A to avoid early stagnation);
///           mock_runner returns {ok=false, stderr="iter 2"}.
/// - Turn 3: mock returns fixed Lua code; mock_runner returns {ok=true}.
///
/// ## Determinism check (Crux constraint — 1 spawn縮退不可)
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

/// Verifies that compile_loop emits ab.obs events when AGENT_BLOCK_LLM_DUMP=meta.
///
/// Reuses the Anthropic mock (fail-then-pass shape, 2 HTTP calls).
/// With AGENT_BLOCK_LLM_DUMP=meta the obs helpers are activated and the three
/// events that appear on the PASS path — iter_start, iter_result, converged —
/// must appear in stdout.
///
/// stagnation and max_iters_reached are not asserted: they do not occur in the
/// 2-iteration PASS shape produced by this mock.
#[tokio::test]
async fn compile_loop_anthropic_mock_emits_obs_events() {
    let (base_url, call_count, ct) =
        common::compile_loop_anthropic_mock::spawn_compile_loop_anthropic_mock_server().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || {
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
            .env("RUST_LOG", "info")
            .env("AGENT_BLOCK_LLM_DUMP", "meta")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_MOCK_PASS"))
            .stdout(predicate::str::contains(
                "prefix=ab.obs event=iter_start component=compile_loop",
            ))
            .stdout(predicate::str::contains(
                "prefix=ab.obs event=iter_result component=compile_loop",
            ))
            .stdout(predicate::str::contains(
                "prefix=ab.obs event=converged component=compile_loop",
            ));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "expected exactly 2 HTTP calls to the anthropic mock"
    );
    ct.cancel();
}

/// Verifies that AGENT_BLOCK_LLM_DUMP=full emits prompt/response bodies from the
/// compile_loop llm_call path, and that meta mode does not.
///
/// Reuses the Anthropic mock (fail-then-pass shape, 2 HTTP calls) and runs the
/// subprocess twice against it:
///   Run 1 (full): stdout must carry `event=request_body component=compile_loop`
///                 (the request body holds the full messages array — the audit trail).
///   Run 2 (meta): the same events must NOT appear; meta output stays meta-only.
///
/// No `#[ignore]` — self-contained, runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_anthropic_mock_full_dump_emits_bodies() {
    let (base_url, call_count, ct) =
        common::compile_loop_anthropic_mock::spawn_compile_loop_anthropic_mock_server().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // --- Run 1: full mode → request/response bodies present ---
    let url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || {
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
            .env("RUST_LOG", "info")
            .env("AGENT_BLOCK_LLM_DUMP", "full")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_MOCK_PASS"))
            .stdout(predicate::str::contains(
                "prefix=ab.obs event=request_body component=compile_loop",
            ))
            .stdout(predicate::str::contains(
                "prefix=ab.obs event=response_body component=compile_loop",
            ))
            .stdout(predicate::str::contains(
                "prefix=ab.obs event=request_headers component=compile_loop",
            ))
            .stdout(predicate::str::contains("***REDACTED***"))
            // The fixture injects api_key="dummy"; it must never reach the dump.
            // Global (whole-stdout) form is safe here: the key value is not logged
            // anywhere else and the mock never echoes it back.
            .stdout(predicate::str::contains("dummy").not());
    })
    .await
    .expect("subprocess assertion task (full) should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "full run: expected exactly 2 HTTP calls to the anthropic mock"
    );

    // Reset between runs — the mock keys its fail-then-pass shape off call_count,
    // and the run 1 subprocess has already exited.
    call_count.store(0, Ordering::SeqCst);

    // --- Run 2: meta mode → bodies must stay absent ---
    let url_clone2 = base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        let target_file = tmp.path().join("target.lua");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("compile_loop_anthropic_mock.lua")])
            .env("ANTHROPIC_BASE_URL_TEST", &url_clone2)
            .env(
                "COMPILE_LOOP_TARGET",
                target_file.to_str().expect("utf8 path"),
            )
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "info")
            .env("AGENT_BLOCK_LLM_DUMP", "meta")
            .assert()
            .success()
            .stdout(predicate::str::contains("COMPILE_LOOP_MOCK_PASS"))
            .stdout(predicate::str::contains("event=request_body").not())
            .stdout(predicate::str::contains("event=response_body").not());
    })
    .await
    .expect("subprocess assertion task (meta) should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "meta run: expected exactly 2 HTTP calls to the anthropic mock"
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
        match req.body.to_string().matches("tool_result").count() {
            0 => anthropic::tool_use_response(vec![anthropic::tool_use(
                "toolu_hint",
                "get_hint",
                json!({}),
            )]),
            1 => {
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
            _ => anthropic::text_response("DONE"),
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
