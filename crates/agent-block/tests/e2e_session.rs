mod common;

use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn session_roundtrip() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("session_roundtrip.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("first_type=table")
                .and(predicate::str::contains("first_count=0"))
                .and(predicate::str::contains("loaded_count=3"))
                .and(predicate::str::contains("loaded_role1=user"))
                .and(predicate::str::contains("loaded_content2=hi there"))
                .and(predicate::str::contains("loaded_role3=user"))
                .and(predicate::str::contains("clear_existing=true"))
                .and(predicate::str::contains("clear_missing=false"))
                .and(predicate::str::contains("after_clear_count=0"))
                .and(predicate::str::contains("reject_empty=true"))
                .and(predicate::str::contains("reject_nil=true")),
        );
}
