mod common;

use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn register_tools_roundtrip() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("register_tools.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("kv_registered=k_get,k_set,k_list")
                .and(predicate::str::contains("k_get.value=123"))
                .and(predicate::str::contains("k_list.keys=x"))
                .and(predicate::str::contains(
                    "locked_registered=ldemo_set,ldemo_get",
                ))
                .and(predicate::str::contains("locked.value=from-locked"))
                .and(predicate::str::contains("locked.other=still-locked"))
                .and(predicate::str::contains("ignored_list="))
                .and(predicate::str::contains(
                    "sql_registered=sql_query,sql_exec",
                ))
                .and(predicate::str::contains("sql_exec.affected=1"))
                .and(predicate::str::contains("sql_query.body=hello"))
                .and(predicate::str::contains("ddl_blocked=true"))
                .and(predicate::str::contains("dml_in_query_blocked=true"))
                .and(predicate::str::contains("poc_notes_count=1")),
        );
}
