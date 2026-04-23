mod common;

use predicates::prelude::*;

#[test]
fn obs_trace_is_consistent_across_http_and_mcp() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("obs_trace_e2e.lua")])
        .env("AGENT_BLOCK_TRACE_ID", "e2e-trace-obs-01")
        .env("AGENT_BLOCK_RUN_ID", "e2e-run-obs-01")
        .env("AGENT_BLOCK_AGENT_ID", "e2e-agent-obs-01")
        .env("AGENT_BLOCK_AGENT_NAME", "e2e-obs-agent")
        .assert()
        .success()
        .stdout(predicate::str::contains("http_error_ok"))
        .stdout(predicate::str::contains("mcp_error_ok"))
        .stdout(predicate::str::contains(
            "prefix=ab.obs event=http_request component=http trace_id=e2e-trace-obs-01",
        ))
        .stdout(predicate::str::contains(
            "prefix=ab.obs event=mcp_call component=mcp trace_id=e2e-trace-obs-01",
        ))
        .stdout(predicate::str::contains(
            "prefix=ab.obs event=mcp_result component=mcp trace_id=e2e-trace-obs-01",
        ));
}
