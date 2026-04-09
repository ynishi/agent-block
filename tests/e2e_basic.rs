mod common;

use predicates::prelude::*;

#[test]
fn hello_script_runs_and_exits_zero() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("hello.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from agent-block"));
}

#[test]
fn missing_script_arg_shows_usage() {
    common::agent_block_cmd()
        .assert()
        .failure()
        .stderr(predicate::str::contains("--script"));
}

#[test]
fn nonexistent_script_fails() {
    common::agent_block_cmd()
        .args(["-s", "/tmp/does_not_exist_12345.lua"])
        .assert()
        .failure();
}

#[test]
fn syntax_error_fails() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("error_syntax.lua")])
        .assert()
        .failure();
}

#[test]
fn runtime_error_fails() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("error_runtime.lua")])
        .assert()
        .failure();
}
