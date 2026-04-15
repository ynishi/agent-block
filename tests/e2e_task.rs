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

#[test]
fn task_phase2_fixture() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("task_phase2.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("scope_elapsed_ok=true")
                .and(predicate::str::contains("scope_children_done=true"))
                .and(predicate::str::contains("scope_name=worker_group"))
                .and(predicate::str::contains("cooperative_cancel_ok=true"))
                .and(predicate::str::contains("timeout_raises=true"))
                .and(predicate::str::contains("timeout_success_val=ok"))
                .and(predicate::str::contains("token_initial=false"))
                .and(predicate::str::contains("token_after_cancel=true"))
                .and(predicate::str::contains("token_check_raises=true"))
                .and(predicate::str::contains("scope_error_propagated=true"))
                .and(predicate::str::contains("sibling_cancelled_ok=true"))
                .and(predicate::str::contains("scope_spawn_join=7"))
                .and(predicate::str::contains("done")),
        );
}

#[test]
fn task_phase3_fixture() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("task_phase3.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("outside_current_nil=true")
                .and(predicate::str::contains("current_id_type=string"))
                .and(predicate::str::contains("current_name=introspect"))
                .and(predicate::str::contains("current_cancelled=false"))
                .and(predicate::str::contains("coro_val=coro_done"))
                .and(predicate::str::contains("coro_sleep_ok=true"))
                .and(predicate::str::contains("coro_yield_val=99"))
                .and(predicate::str::contains("coro_concurrent_ok=true"))
                .and(predicate::str::contains("unknown_driver_rejected=true"))
                .and(predicate::str::contains("coro_current_name=coro_named"))
                .and(predicate::str::contains("done")),
        );
}
