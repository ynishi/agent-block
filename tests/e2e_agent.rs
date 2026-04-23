mod common;

use predicates::prelude::*;
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
