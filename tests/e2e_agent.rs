mod common;

use predicates::prelude::*;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

#[test]
fn agent_require_succeeds() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("agent_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent module loaded successfully"));
}

#[test]
fn agent_run_is_function() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("agent_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent.run type: function"));
}

/// Requires ANTHROPIC_API_KEY — skipped in CI.
#[test]
#[ignore]
fn agent_run_basic() {
    common::agent_block_cmd()
        .args(["-s", "examples/test_agent.lua"])
        .env("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")
        .assert()
        .success();
}

/// Requires ANTHROPIC_API_KEY — skipped in CI.
/// Verifies structured `ab.llm` metadata lines are emitted.
#[test]
#[ignore]
fn agent_run_emits_structured_meta_logs() {
    common::agent_block_cmd()
        .args(["-s", "examples/test_agent_log_meta.lua"])
        .env("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")
        .env("AGENT_BLOCK_LLM_DUMP", "meta")
        .env("AGENT_BLOCK_TRACE_ID", "e2e-trace-01")
        .env("AGENT_BLOCK_AGENT_ID", "e2e-agent-01")
        .env("AGENT_BLOCK_AGENT_NAME", "e2e-agent")
        .env("AGENT_BLOCK_RUN_ID", "e2e-run-01")
        .assert()
        .success()
        .stdout(predicate::str::contains("prefix=ab.llm event=request"))
        .stdout(predicate::str::contains("trace_id=e2e-trace-01"))
        .stdout(predicate::str::contains("agent_id=e2e-agent-01"))
        .stdout(predicate::str::contains("run_id=e2e-run-01"));
}

/// Verifies the OpenAI provider path with an in-process mock server.
///
/// Spawns a minimal axum HTTP server that simulates a 2-turn Chat Completions
/// exchange:
///   - Turn 1: assistant returns `tool_calls` → agent dispatches the `echo` tool
///   - Turn 2: assistant returns `finish_reason="stop"` with final content
///
/// The mock URL is passed to the fixture via `OPENAI_BASE_URL_TEST`.
/// No external API key is required.
#[tokio::test]
async fn agent_run_openai_mock_dispatches_tool() {
    let (base_url, call_count, ct) = common::openai_mock::spawn_openai_mock_server().await;
    // Give the server a moment to start accepting connections.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || {
        let tmp = tempdir().expect("tempdir");
        common::agent_block_cmd()
            .args(["-s", &common::fixture("agent_openai_mock.lua")])
            .env("OPENAI_BASE_URL_TEST", &url_clone)
            .env("AGENT_BLOCK_HOME", tmp.path())
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("OPENAI_MOCK_TOOL_DISPATCHED_OK"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "expected exactly 2 HTTP calls to the mock (turn 1: tool_calls, turn 2: stop)"
    );
    ct.cancel();
}
