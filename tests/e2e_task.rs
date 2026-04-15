mod common;

use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn task_phase1_fixture() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("task_phase1.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("h1.id_type=string")
                .and(predicate::str::contains("v1=42"))
                .and(predicate::str::contains("slept_ok=true"))
                .and(predicate::str::contains("v2=a"))
                .and(predicate::str::contains("v3=b"))
                .and(predicate::str::contains("concurrent_ok=true"))
                .and(predicate::str::contains("h4.name=worker"))
                .and(predicate::str::contains("h5.elapsed_positive=true"))
                .and(predicate::str::contains("yield_ok=true"))
                .and(predicate::str::contains("sql_from_task=from_task"))
                .and(predicate::str::contains("abort_ok=true"))
                .and(predicate::str::contains("done")),
        );
}
