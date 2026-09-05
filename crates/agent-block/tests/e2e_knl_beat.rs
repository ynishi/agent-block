mod common;

use predicates::prelude::*;

/// The Lua kernel's spec, run in the full host.
///
/// The assertions live in the fixture rather than here, and deliberately: the
/// invariants being checked are the beat's, the store's and the bridge's, and
/// they are written in the language that drives them.  What this test adds is
/// the environment the pure lspec runner cannot give them — the real `knl`
/// syscall bridge, a real SQLite store — so the whole of it is: run the
/// fixture, and hold the process to the marker the fixture prints last.  A
/// failing assertion inside exits non-zero and fails here as well.
///
/// `AGENT_BLOCK_HOME` points the run at a temp base dir, so the sessions the
/// fixture opens without a `store` land in a kernel database of this test's
/// own (`{temp}/projects/<slug>/knl.sqlite`) rather than accumulating in the
/// developer's `~/.agent-block`. The default store is a file now, so a test
/// that did not say where would be writing to a real one.
#[test]
fn knl_beat_poc_runs_in_the_full_host() {
    let home = tempfile::tempdir().expect("a base dir for this run");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-s", &common::fixture("knl_beat_test.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("[KNL] all_ok"));
}
