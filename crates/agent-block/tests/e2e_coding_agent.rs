mod common;

use predicates::prelude::*;
use tempfile::tempdir;

/// The coding_agent built-in runners are only reachable through the facade
/// (`register_tool` returns a tool name, not the runner), so the fixture drives
/// the "lua" runner via `coding_agent._test_helpers()` and asserts the
/// `{ok, stdout, stderr, exit_code}` contract compile_loop depends on.
///
/// `require("coding_agent")` resolves to the embedded facade; CWD is the
/// repository root only so the runner's relative scratch paths line up.
///
/// The fixture needs a standalone `lua` interpreter for the pass/fail cases.
/// Where none is installed it prints `SKIP_NO_LUA` and asserts only the
/// spawn-failure shape; the marker makes that visible instead of silently
/// reporting green on an untested path.
///
/// Scratch files go in a per-run tempdir handed over in `RUNNER_SCRATCH_DIR`
/// (same pattern as `AGENT_BLOCK_HOME` in `e2e_build_tools_extra.rs`): fixed
/// `/tmp` names would race a concurrent run and collide with files owned by
/// another user on a shared machine.
#[test]
fn coding_agent_lua_runner_contract() {
    let repo_root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let scratch = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .current_dir(repo_root)
        .env("RUNNER_SCRATCH_DIR", scratch.path())
        .args(["-s", &common::fixture("coding_agent_runner.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("RUNNER_CONTRACT_OK"));
}
