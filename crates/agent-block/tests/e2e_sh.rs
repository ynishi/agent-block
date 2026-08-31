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

/// The host's own credentials must not reach `sh.exec` children, while every
/// other inherited variable still does.
///
/// The fixture echoes only the four variables under test (`A` / `O` / `M` / `V`)
/// rather than dumping `env`: a failing assertion makes assert_cmd print the
/// captured stdout verbatim, and in CI that dump would carry real secrets.
#[test]
fn sh_exec_strips_own_credentials() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("sh_exec_env.lua")])
        .env("ANTHROPIC_API_KEY", "dummy-anthropic-3f9c1a")
        .env("OPENAI_API_KEY", "dummy-openai-3f9c1a")
        .env("AGENT_BLOCK_MESH_SECRET_KEY", "dummy-mesh-3f9c1a")
        .env("TEST_VISIBLE_VAR", "visible123")
        .assert()
        .success()
        .stdout(
            // Credentials arrive empty, the ordinary variable arrives intact.
            predicate::str::contains("\nA=\n")
                .and(predicate::str::contains("\nO=\n"))
                .and(predicate::str::contains("\nM=\n"))
                .and(predicate::str::contains("\nV=visible123\n"))
                .and(predicate::str::contains("dummy-anthropic-3f9c1a").not())
                .and(predicate::str::contains("dummy-openai-3f9c1a").not())
                .and(predicate::str::contains("dummy-mesh-3f9c1a").not()),
        );
}
