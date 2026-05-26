mod common;

use predicates::prelude::*;

#[test]
fn prompt_flag_injects_global() {
    common::agent_block_cmd()
        .args([
            "--prompt",
            "hello world",
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:hello world"))
        .stdout(predicate::str::contains("CONTEXT:nil"));
}

#[test]
fn context_flag_injects_global() {
    common::agent_block_cmd()
        .args([
            "-c",
            "system ctx",
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:nil"))
        .stdout(predicate::str::contains("CONTEXT:system ctx"));
}

#[test]
fn both_flags_inject_globals() {
    common::agent_block_cmd()
        .args([
            "--prompt",
            "ask me",
            "-c",
            "be helpful",
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:ask me"))
        .stdout(predicate::str::contains("CONTEXT:be helpful"));
}

#[test]
fn no_flags_globals_are_nil() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("prompt_flag.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:nil"))
        .stdout(predicate::str::contains("CONTEXT:nil"));
}
