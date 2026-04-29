mod common;

use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn build_tools_extra_flatten() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("build_tools_extra.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("nested.name=nested_x")
                .and(predicate::str::contains("nested.description=nested desc"))
                .and(predicate::str::contains("nested.handler=nil"))
                .and(predicate::str::contains("nested.schema=nil"))
                .and(predicate::str::contains("flat.name=flat_y"))
                .and(predicate::str::contains("flat.description=flat desc")),
        );
}

#[test]
fn compile_loop_make_register_false_no_dedup() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("build_tools_dedup.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("dedup=ok"));
}
