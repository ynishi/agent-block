mod common;

use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn kv_roundtrip() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("kv_roundtrip.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("get_count=42")
                .and(predicate::str::contains("get_name=agent-block"))
                .and(predicate::str::contains("get_missing=nil"))
                .and(predicate::str::contains("list=count,name"))
                .and(predicate::str::contains("prefix_list=run-1,run-2"))
                .and(predicate::str::contains("delete_existing=true"))
                .and(predicate::str::contains("delete_missing=false"))
                .and(predicate::str::contains("after_delete=nil")),
        );
}
