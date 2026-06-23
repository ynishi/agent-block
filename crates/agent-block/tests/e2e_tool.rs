mod common;

use predicates::prelude::*;

#[test]
fn tool_register_and_call() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("tool_register.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("echoed: ping"));
}

#[test]
fn tool_list() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("tool_schema.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("tool: greet"));
}
