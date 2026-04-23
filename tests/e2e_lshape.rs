mod common;

use predicates::prelude::*;

#[test]
fn lshape_is_available_from_vendored_blocks() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("lshape_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("lshape_ok"))
        .stdout(predicate::str::contains("luacats_ok"));
}
