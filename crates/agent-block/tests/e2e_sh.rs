mod common;

use predicates::prelude::*;

#[test]
fn sh_exec_echo() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("sh_exec.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ok=true")
                .and(predicate::str::contains("code=0"))
                .and(predicate::str::contains("stdout=ok")),
        );
}
