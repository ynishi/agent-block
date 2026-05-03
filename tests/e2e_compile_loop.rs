mod common;

use predicates::prelude::*;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

/// Verifies compile_loop in diff mode (edit_mode="diff") with the Anthropic provider.
///
/// Scenario: 2 iterations.
///   - Iter 1: mock returns a SEARCH/REPLACE block with a wrong SEARCH text.
///     apply_blocks detects the mismatch → failure feedback sent back → 2nd LLM call.
///   - Iter 2: mock returns a correct SEARCH/REPLACE block (exact match of initial file).
///     apply_blocks succeeds → file patched → mock_runner detects "world" in output → ok=true.
///
/// Validates that:
///   - The diff mode parse/apply pipeline is wired correctly.
///   - A SEARCH mismatch triggers a retry (not a silent skip).
///   - The runner is invoked after a successful apply.
///   - The loop converges on the second LLM call.
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
        "expected exactly 2 HTTP calls to the diff anthropic mock (iter1: apply-fail, iter2: pass)"
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
/// if `blocks/compile_loop/init.lua` did not forward `opts.base_url` to the Anthropic
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

/// Verifies compile_loop in multi-file diff mode (happy path, 1-turn, 2-file).
///
/// Scenario: 1 iteration.
///   - Mock returns path-header SEARCH/REPLACE for both file_a and file_b in a single turn.
///   - apply_blocks succeeds for both files → mock_runner receives paths list → ok=true.
///
/// Validates:
///   - target_files list is accepted (Crux #2 backward-compatible conf API).
///   - Parser extracts path headers and routes each block to the correct file (Crux #1).
///   - Runner is called with a list of paths, not a single string (Crux #3 signature toggle).
///   - result.modified_files contains 2 paths; result.artifact_path is nil.
///   - Loop converges on the first LLM call (call_count == 1).
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_diff_multi_anthropic_mock_iterates_until_pass() {
    let (base_url, call_count, ct) =
        common::compile_loop_diff_multi_anthropic_mock::spawn_compile_loop_diff_multi_anthropic_mock_server()
            .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = base_url.clone();
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

    // Happy path: exactly 1 HTTP call (both files patched in a single LLM turn).
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "expected exactly 1 HTTP call to the multi diff mock (happy path: 2 files in 1 turn)"
    );
    ct.cancel();
}

/// Verifies compile_loop in multi-file diff mode converges after a SEARCH mismatch (2-iter).
///
/// Scenario: 2 iterations.
///   - Iter 1: mock returns file_a SEARCH with wrong text ("WRONG") — apply fails for file_a.
///     compile_loop feeds back a failure message, triggering a second LLM call.
///   - Iter 2: mock returns correct SEARCH for both file_a and file_b → apply succeeds → ok=true.
///
/// Validates:
///   - A SEARCH mismatch in multi-file mode triggers a retry (not a silent skip).
///   - Loop converges on the second LLM call (call_count == 2).
///   - result.modified_files contains 2 paths; result.artifact_path is nil.
///
/// No `#[ignore]` — runs under plain `cargo test` with no API keys.
#[tokio::test]
async fn compile_loop_diff_multi_anthropic_mock_two_iter_converges() {
    let (base_url, call_count, ct) =
        common::compile_loop_diff_multi_anthropic_mock::spawn_compile_loop_diff_multi_anthropic_mock_two_iter_server()
            .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = base_url.clone();
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

    // 2-iter: exactly 2 HTTP calls (iter1: apply-fail, iter2: pass).
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "expected exactly 2 HTTP calls to the multi diff mock (iter1: apply-fail, iter2: pass)"
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
