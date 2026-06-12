mod common;

use predicates::prelude::*;
use std::io::Write as _;

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

#[test]
fn prompt_file_injects_global() {
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(b"from file content").expect("write");
    let path = tmp.path().to_str().expect("path str");

    common::agent_block_cmd()
        .args(["--prompt-file", path, "-s", &common::fixture("prompt_flag.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:from file content"))
        .stdout(predicate::str::contains("CONTEXT:nil"));
}

#[test]
fn context_file_injects_global() {
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(b"file system ctx").expect("write");
    let path = tmp.path().to_str().expect("path str");

    common::agent_block_cmd()
        .args(["--context-file", path, "-s", &common::fixture("prompt_flag.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:nil"))
        .stdout(predicate::str::contains("CONTEXT:file system ctx"));
}

#[test]
fn prompt_file_missing_path_errors() {
    common::agent_block_cmd()
        .args([
            "--prompt-file",
            "/nonexistent/path/that/does/not/exist.txt",
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--prompt-file"));
}
