//! The embedded block layers, from the outside: what a project may replace,
//! what it may not, and how a replacement reaches the module it replaced.
//!
//! Each test builds a project root in a tempdir, because that is the thing
//! under test — `lib/` in the project root is the highest-priority place
//! `require` looks after the script's own directory, and a checked-in one
//! would apply to every other fixture.

mod common;

use predicates::prelude::*;
use std::path::Path;
use tempfile::tempdir;

/// Write `<project>/lib/<rel>` (creating the directories) and return it.
fn write_project_block(project: &Path, rel: &str, source: &str) -> std::path::PathBuf {
    let path = project.join("lib").join(rel);
    std::fs::create_dir_all(path.parent().expect("block path has a parent")).expect("mkdir");
    std::fs::write(&path, source).expect("write block");
    path
}

/// A project block that shadows `agent` and delegates the one call it changes
/// back to the embedded module. `run` short-circuits rather than reaching a
/// provider: what is being proved is that `base` resolved, which
/// `type(base.run)` says without a model call.
const AGENT_OVERRIDE: &str = r#"
local base = require("embedded.agent")
local M = setmetatable({}, { __index = base })

function M.run(opts)
    print("OVERRIDE")
    print("BASE_RUN_TYPE=" .. type(base.run))
    return { from = "override", prompt = opts.prompt }
end

return M
"#;

// ── (a) the seal ──────────────────────────────────────────────────────────

#[test]
fn a_project_kernel_fails_the_run_before_the_script() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    let shadow = write_project_block(project.path(), "knl/init.lua", "return {}\n");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("hello.lua")])
        .assert()
        .failure()
        .stderr(predicate::str::contains("knl"))
        .stderr(predicate::str::contains(shadow.display().to_string()))
        // The refusal is a refusal, not a warning the script runs past.
        .stdout(predicate::str::contains("hello from agent-block").not());
}

#[test]
fn the_refusal_says_why_and_names_the_escape_hatch() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write_project_block(project.path(), "knl/init.lua", "return {}\n");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("hello.lua")])
        .assert()
        .failure()
        .stderr(predicate::str::contains("sealed module"))
        .stderr(predicate::str::contains("Change it upstream"))
        .stderr(predicate::str::contains("AGENT_BLOCK_UNSEAL=1"));
}

#[test]
fn unseal_downgrades_the_refusal_to_a_warning() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write_project_block(project.path(), "knl/init.lua", "return {}\n");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .env("AGENT_BLOCK_UNSEAL", "1")
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("hello.lua")])
        .assert()
        .success()
        // `tracing` writes to stdout in script mode (stderr is reserved for
        // the MCP subcommand, whose stdout is the protocol).
        .stdout(predicate::str::contains("AGENT_BLOCK_UNSEAL=1"))
        .stdout(predicate::str::contains("hello from agent-block"));
}

#[test]
fn a_project_lshape_sub_module_is_sealed_too() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write_project_block(project.path(), "lshape/t.lua", "return {}\n");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("hello.lua")])
        .assert()
        .failure()
        .stderr(predicate::str::contains("lshape.t"));
}

// ── (b) shadow and delegate ───────────────────────────────────────────────

#[test]
fn a_project_agent_can_delegate_to_the_embedded_one() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write_project_block(project.path(), "agent/init.lua", AGENT_OVERRIDE);

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("embedded_agent_override.lua")])
        .assert()
        .success()
        // `require("agent")` reached the project's module …
        .stdout(predicate::str::contains("OVERRIDE"))
        .stdout(predicate::str::contains("RESULT_FROM=override"))
        .stdout(predicate::str::contains("RESULT_PROMPT=not sent anywhere"))
        // … and that module reached the one it replaced.
        .stdout(predicate::str::contains("BASE_RUN_TYPE=function"));
}

// ── (c) the alias, including for sealed modules ───────────────────────────

#[test]
fn the_embedded_alias_resolves_a_sealed_module() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("embedded_alias_knl.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("EMBEDDED_KNL_TYPE=table"))
        .stdout(predicate::str::contains("HAS_BEAT=function"));
}

#[test]
fn a_filesystem_embedded_directory_does_not_shadow_the_alias() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    // Named exactly as `require("embedded.knl")` would be read off disk. The
    // alias resolver sits ahead of the filesystem ones, so this is never it.
    write_project_block(
        project.path(),
        "embedded/knl.lua",
        "return { sentinel = true, beat = 1 }\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("embedded_alias_knl.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("SENTINEL=nil"))
        .stdout(predicate::str::contains("HAS_BEAT=function"));
}
