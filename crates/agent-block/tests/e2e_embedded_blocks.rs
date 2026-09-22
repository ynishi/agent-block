//! The embedded modules from the outside: that a project replaces one by
//! putting a file of the same name in a `lib/` tier, and how the replacement
//! reaches the module it replaced.
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

// ── (a) shadow and delegate ───────────────────────────────────────────────

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

// ── (a2) every module is a module ─────────────────────────────────────────

/// **Every embedded module answers a table, under its own name and under the
/// `embedded.` alias.**
///
/// This is the precondition of the delegation idiom, and it is asserted for
/// the whole set rather than one module at a time because the way it breaks
/// is by omission: a module written as a script — assigning onto a global and
/// returning nothing — answers `true`, `require` is happy, and
/// `require("embedded.<name>").f` is a nil index somewhere else entirely.
/// The four bridge tool modules were exactly that until they became
/// libraries.
///
/// The fixture carries the list (`EMBEDDED_BLOCKS` + `EMBEDDED_LIBS` +
/// `knl_types`) and prints every name that fails, so one run names all of
/// them rather than the first.
#[test]
fn every_embedded_module_answers_a_table() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("embedded_every_module.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("checked=28"))
        .stdout(predicate::str::contains("FAIL").not())
        .stdout(predicate::str::contains("ok"));
}

// ── (b) the alias, the kernel included ─────────────────────────────────────

#[test]
fn the_embedded_alias_resolves_the_kernel() {
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
