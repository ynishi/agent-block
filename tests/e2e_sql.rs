mod common;

use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn sql_roundtrip() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("sql_roundtrip.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("affected=1")
                .and(predicate::str::contains("row_count=1"))
                .and(predicate::str::contains("k=hello"))
                .and(predicate::str::contains("v=world"))
                .and(predicate::str::contains("updated=1"))
                .and(predicate::str::contains("after_update=planet"))
                .and(predicate::str::contains("deleted=1"))
                .and(predicate::str::contains("count=0")),
        );
}
