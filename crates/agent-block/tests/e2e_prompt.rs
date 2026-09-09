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
        .args([
            "--prompt-file",
            path,
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
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
        .args([
            "--context-file",
            path,
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
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

/// The config file is the lowest of the three layers: it stands in where the
/// command line gave nothing, and loses to a flag that did.
#[test]
fn config_file_supplies_the_run_and_a_flag_overrides_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("run.json");
    std::fs::write(
        &config,
        r#"{ "prompt": "from the file", "context": "ctx from the file" }"#,
    )
    .expect("write config");

    common::agent_block_cmd()
        .args([
            "--config",
            config.to_str().expect("utf-8"),
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:from the file"))
        .stdout(predicate::str::contains("CONTEXT:ctx from the file"));

    common::agent_block_cmd()
        .args([
            "--config",
            config.to_str().expect("utf-8"),
            "--prompt",
            "from the flag",
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:from the flag"))
        .stdout(predicate::str::contains("CONTEXT:ctx from the file"));
}

/// Free text goes in the file as text. A prompt with quotes and newlines in
/// it is what a command line cannot promise and a file can — which is the
/// reason a job manager writes one instead of building a longer command.
#[test]
fn a_config_file_carries_text_a_command_line_would_mangle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("run.json");
    std::fs::write(
        &config,
        serde_json::to_string(&serde_json::json!({
            "prompt": "it's \"due\"\nand then some $HOME `date`",
        }))
        .expect("json"),
    )
    .expect("write config");

    common::agent_block_cmd()
        .args([
            "--config",
            config.to_str().expect("utf-8"),
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:it's \"due\""))
        .stdout(predicate::str::contains("and then some $HOME `date`"));
}

/// A misspelled key is a run that would have gone without its prompt, so the
/// file is closed and the mistake is said out loud.
#[test]
fn an_unknown_key_in_the_config_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("run.json");
    std::fs::write(&config, r#"{ "promt": "typo" }"#).expect("write config");

    common::agent_block_cmd()
        .args([
            "--config",
            config.to_str().expect("utf-8"),
            "-s",
            &common::fixture("prompt_flag.lua"),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("promt"));
}

/// The environment no longer carries one run's identity: a variable is
/// inherited, and a block that starts another `agent-block` would have handed
/// its child its own prompt.
#[test]
fn the_environment_no_longer_supplies_the_prompt() {
    common::agent_block_cmd()
        .env("AGENT_BLOCK_PROMPT", "from the environment")
        .env("AGENT_BLOCK_CONTEXT", "ctx from the environment")
        .args(["-s", &common::fixture("prompt_flag.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROMPT:nil"))
        .stdout(predicate::str::contains("CONTEXT:nil"));
}
