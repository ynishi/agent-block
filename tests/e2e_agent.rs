mod common;

use predicates::prelude::*;

#[test]
fn agent_require_succeeds() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("agent_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent module loaded successfully"));
}

#[test]
fn agent_run_is_function() {
    common::agent_block_cmd()
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
