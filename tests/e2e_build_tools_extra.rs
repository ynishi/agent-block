mod common;

use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn build_tools_mcp_group_filter() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("build_tools_mcp_group.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("case1.outline_included=true")
                .and(predicate::str::contains("case1.search_excluded=true"))
                .and(predicate::str::contains("case2.search_included=true"))
                .and(predicate::str::contains("case2.outline_excluded=true"))
                .and(predicate::str::contains("case3.all_tools_count=2_expected=2"))
                .and(predicate::str::contains("case4.mcp_not_in_default=true")),
        );
}

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
fn compile_loop_make_default_dedup() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("build_tools_dedup.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("dedup=ok"));
}

#[test]
fn dispatch_extra_tools_via_registry() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("dispatch_extra_tools.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("dispatch=ok"));
}
