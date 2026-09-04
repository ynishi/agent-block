mod common;

use predicates::prelude::*;

#[test]
fn knl_beat_poc_runs_in_the_full_host() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("knl_beat_test.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("[KNL] all_ok"));
}
