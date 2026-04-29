mod common;

use predicates::prelude::*;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

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
