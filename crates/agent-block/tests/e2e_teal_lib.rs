//! A module written in Teal (`.tl`) is a module like any other, from the outside.
//!
//! The `require` chain puts a Teal resolver ahead of the Lua one in every
//! filesystem tier (`host.rs`, `build_isle_init`): `name.tl` is type-checked and
//! generated at the `require`, `name.d.tl` beside a `name.lua` types the Lua
//! without replacing it, and a type error in a `.tl` fails the `require` rather
//! than falling through to a copy in a lower tier. These tests pin those facts
//! through the same fixture `e2e_block_dirs.rs` drives the Lua tiers with — and
//! the ones that follow from the checker being a state of its own: a project's
//! `.tl` reads the host's globals through the declarations the binary ships, a
//! project's `htl.toml` reaches the checker, a declaration for an embedded
//! module never hides it, and a script cannot reach the compiler.

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

/// The refusal names the file and the line, so a reader who has only the
/// output has something to open.
#[test]
fn a_type_error_names_the_file_and_the_line() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "lib/mylib.tl",
        "local record MyLib\n  from: string\nend\nlocal M: MyLib = { from = 42 }\nreturn M\n",
    );
    write(
        project.path(),
        "probe.lua",
        "local ok, err = pcall(require, \"mylib\")\nprint(\"ERR=\" .. tostring(err))\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &project.path().join("probe.lua").to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("mylib.tl:4:"))
        .stdout(predicate::str::contains("expected string"));
}

/// A declaration a project puts in a tier for an EMBEDDED module — `lib/
/// knl.d.tl`, to type its own `.tl` against the kernel — types it without
/// replacing it. The Teal resolver, on a `.d.tl` with nothing beside it,
/// answers with a stub unless the name is preloaded, and every embedded name
/// is; so the chain goes on and the embedded module answers. `lshape` is the
/// one the bridges require at start, so a stub there would keep the host
/// from coming up at all.
#[test]
fn a_declaration_in_a_tier_for_an_embedded_module_leaves_the_embedded_one_to_run() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "lib/lshape.d.tl",
        "local record lshape\n  t: any\nend\nreturn lshape\n",
    );
    write(
        project.path(),
        "lib/knl.d.tl",
        "local record knl\n  open: function(any): any\nend\nreturn knl\n",
    );
    write(
        project.path(),
        "probe.lua",
        "print(\"LSHAPE_T=\" .. type(require(\"lshape\").t))\n\
         print(\"KNL_OPEN=\" .. type(require(\"knl\").open))\n\
         print(\"FS_TOOLS=\" .. type(std.fs.register_tools))\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &project.path().join("probe.lua").to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("LSHAPE_T=table"))
        .stdout(predicate::str::contains("KNL_OPEN=function"))
        .stdout(predicate::str::contains("FS_TOOLS=function"));
}

/// A project's `htl.toml` is applied to the checker as `htl check` applies
/// it: a declaration under `[check] paths` types a module the project's
/// `.tl` requires, without that directory being a `require` tier — the
/// module itself comes from wherever `require` finds it.
#[test]
fn a_project_htl_toml_puts_its_declarations_on_the_checker_path() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(project.path(), "htl.toml", "[check]\npaths = [\"types\"]\n");
    write(
        project.path(),
        "types/origin.d.tl",
        "local record Origin\n  name: string\nend\nreturn Origin\n",
    );
    write(
        project.path(),
        "lib/origin.lua",
        "return { name = \"declared\" }\n",
    );
    write(
        project.path(),
        "lib/mylib.tl",
        "local origin = require(\"origin\")\n\
         local record MyLib\n  from: string\nend\n\
         local M: MyLib = { from = origin.name }\n\
         return M\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_FROM=declared"));
}

/// A project's `.tl` reads the host's globals the way the embedded modules
/// do — `require("host_types")`, `global std: host.Std` — against the
/// declarations the binary writes out for the checker at start. Nothing in
/// the project declares `std`; the binary that runs the module does.
#[test]
fn a_project_tl_module_reads_the_host_globals_through_host_types() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "lib/mylib.tl",
        "local host = require(\"host_types\")\n\
         global std: host.Std\n\
         local record MyLib\n  from: string\nend\n\
         local M: MyLib = { from = std.json.encode({ typed = true }) }\n\
         return M\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_FROM={\"typed\":true}"));
}

/// A `.tl` in the project's `.agent-block/lib/` tier resolves like one in
/// `lib/`, and requires another `.tl` from a lower tier through the checker's
/// search path, which holds every tier.
#[test]
fn a_tl_module_in_the_vendored_tier_requires_a_tl_module_from_a_lower_tier() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        home.path(),
        "lib/origin.tl",
        "local record Origin\n  name: string\nend\nlocal M: Origin = { name = \"home\" }\nreturn M\n",
    );
    write(
        project.path(),
        ".agent-block/lib/mylib/init.tl",
        "local origin = require(\"origin\")\n\
         local record MyLib\n  from: string\nend\n\
         local M: MyLib = { from = \"vendored+\" .. origin.name }\n\
         return M\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_FROM=vendored+home"));
}

/// The compiler is the checker's, in a state of its own: a script cannot
/// `require("tl")` and reach it, and the VM's `package.path` carries none of
/// the checker's templates.
#[test]
fn a_script_cannot_reach_the_teal_compiler() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "probe.lua",
        "print(\"TL_OK=\" .. tostring((pcall(require, \"tl\"))))\n\
         print(\"PATH_HAS_TEAL=\" .. tostring(package.path:find(\"?/?.lua\", 1, true) ~= nil))\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &project.path().join("probe.lua").to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("TL_OK=false"))
        .stdout(predicate::str::contains("PATH_HAS_TEAL=false"));
}
