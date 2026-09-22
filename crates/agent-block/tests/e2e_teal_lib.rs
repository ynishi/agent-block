//! A module written in Teal (`.tl`) is a module like any other, from the outside.
//!
//! The `require` chain puts a Teal resolver ahead of the Lua one in every
//! filesystem tier (`host.rs`, `build_isle_init`): `name.tl` is type-checked and
//! generated at the `require`, `name.d.tl` beside a `name.lua` types the Lua
//! without replacing it, and a type error in a `.tl` fails the `require` rather
//! than falling through to a copy in a lower tier. These tests pin those three
//! facts through the same fixture `e2e_block_dirs.rs` drives the Lua tiers with.

mod common;

use predicates::prelude::*;
use std::path::Path;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, source: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("path has a parent")).expect("mkdir");
    std::fs::write(&path, source).expect("write");
}

/// `mylib.tl`: the record the fixture reads, with a type on it.
const MYLIB_TL: &str =
    "local record MyLib\n  from: string\nend\nlocal M: MyLib = { from = \"teal\" }\nreturn M\n";

#[test]
fn a_tl_module_in_the_project_lib_is_checked_generated_and_required() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(project.path(), "lib/mylib.tl", MYLIB_TL);

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_FROM=teal"));
}

/// Within one tier the Teal resolver is consulted first, so a `.tl` beside a
/// `.lua` of the same name is the one that answers.
#[test]
fn a_tl_module_wins_over_a_lua_sibling_in_the_same_tier() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(project.path(), "lib/mylib.tl", MYLIB_TL);
    write(
        project.path(),
        "lib/mylib.lua",
        "return { from = \"lua\" }\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_FROM=teal"));
}

/// A `.d.tl` beside a `.lua` is a declaration, not a replacement: the Teal
/// resolver steps aside and the Lua runs.
#[test]
fn a_declaration_beside_a_lua_module_leaves_the_lua_to_run() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "lib/mylib.lua",
        "return { from = \"lua\" }\n",
    );
    write(
        project.path(),
        "lib/mylib.d.tl",
        "local record MyLib\n  from: string\nend\nreturn MyLib\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_FROM=lua"));
}

/// A type error is a refused `require`, named with the file and line — not a
/// fall-through to the user's copy, which is what a silent `None` would have
/// been. The fixture's `pcall` turns the refusal into `MYLIB_MISSING`, and the
/// message reaches the caller through it, so it is asserted on the same
/// stdout rather than on the process failing.
#[test]
fn a_type_error_in_a_tl_module_refuses_the_require_instead_of_falling_through() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(home.path(), "lib/mylib.lua", "return { from = \"user\" }\n");
    write(
        project.path(),
        "lib/mylib.tl",
        "local record MyLib\n  from: string\nend\nlocal M: MyLib = { from = 42 }\nreturn M\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_MISSING"))
        .stdout(predicate::str::contains("MYLIB_FROM=user").not());
}
