//! `agent-block vendor`, from the outside.
//!
//! The claim the subcommand makes is not that it writes a file — it is that the
//! file it writes is the one the project resolves from then on. So the test
//! that matters runs a script afterwards and asks the module which copy
//! answered: vendor `session`, mark the copy, `require("session")`, and read the
//! mark back. The rest pins what the caller is told along the way — the header
//! on the copy, the listing, and the refusal to overwrite.

mod common;

use predicates::prelude::*;
use std::path::Path;
use tempfile::tempdir;

/// The vendored `session`, where it lands under a project root.
fn vendored_session(project: &Path) -> std::path::PathBuf {
    project.join(".agent-block/lib/session/init.lua")
}

/// Mark the vendored copy so a script can tell it from the embedded one: the
/// file ends in `return M`, and a field set just before that is visible on the
/// table `require` hands back whatever else the module does.
fn mark(path: &Path) {
    let source = std::fs::read_to_string(path).expect("read the vendored copy");
    let (body, tail) = source
        .rsplit_once("return M")
        .expect("the module ends by returning its table");
    std::fs::write(path, format!("{body}M.vendored = true\nreturn M{tail}")).expect("write");
}

#[test]
fn a_vendored_module_is_the_one_the_project_resolves() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    let target = vendored_session(project.path());

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "session"])
        .assert()
        .success()
        .stdout(predicate::str::contains(target.to_string_lossy().as_ref()));

    let written = std::fs::read_to_string(&target).expect("the copy is there");
    let mut lines = written.lines();
    assert_eq!(
        lines.next().expect("a header"),
        format!(
            "-- vendored from agent-block {} (embedded session)",
            env!("CARGO_PKG_VERSION")
        ),
        "{written}"
    );
    assert!(
        written.contains("-- This copy is what require(\"session\")"),
        "{written}"
    );
    assert!(
        written.contains("require(\"embedded.session\"). Edit freely;"),
        "{written}"
    );
    // The source is under the header, verbatim.
    assert!(
        written.contains("M.NS = \"_agent_block_session\""),
        "{written}"
    );

    mark(&target);

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("vendored_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("SESSION_VENDORED=true"));
}

#[test]
fn the_listing_says_what_this_project_has_already_taken_over() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    let list = |project: &Path| {
        common::agent_block_cmd()
            .env("AGENT_BLOCK_HOME", home.path())
            .args(["-p", &project.to_string_lossy()])
            .args(["vendor", "--list"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };

    let before = String::from_utf8(list(project.path())).expect("utf-8");
    assert!(
        before
            .lines()
            .any(|l| l.starts_with("session") && !l.contains("vendored")),
        "{before}"
    );
    // Every root is listed as vendorable; only a pack carries a tag of its own.
    assert!(
        before
            .lines()
            .any(|l| l.starts_with("knl") && l.contains("lib")),
        "{before}"
    );
    assert!(before.contains("pack"), "{before}");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "session"])
        .assert()
        .success();

    let after = String::from_utf8(list(project.path())).expect("utf-8");
    assert!(
        after
            .lines()
            .any(|l| l.starts_with("session") && l.ends_with("vendored")),
        "{after}"
    );
}

/// The copy is the project's own the moment it exists, so a second vendor is a
/// refusal — and `--force` is the caller saying they meant it.
#[test]
fn a_second_vendor_is_refused_unless_forced() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    let target = vendored_session(project.path());
    let vendor = |args: &[&str]| {
        let mut cmd = common::agent_block_cmd();
        cmd.env("AGENT_BLOCK_HOME", home.path())
            .args(["-p", &project.path().to_string_lossy()])
            .arg("vendor")
            .args(args);
        cmd
    };

    vendor(&["session"]).assert().success();
    mark(&target);

    vendor(&["session"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is already there"))
        .stderr(predicate::str::contains("--force"));
    assert!(
        std::fs::read_to_string(&target)
            .expect("read")
            .contains("M.vendored = true"),
        "the refused run left the copy alone"
    );

    vendor(&["--force", "session"]).assert().success();
    assert!(
        !std::fs::read_to_string(&target)
            .expect("read")
            .contains("M.vendored = true"),
        "--force wrote the embedded source over the copy"
    );
}

/// The kernel vendors like every other module: the copy lands on the require
/// path with the specs that check it, and the header names the original it
/// came from.
#[test]
fn the_kernel_is_written_with_the_specs_that_check_it() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    let target = project.path().join(".agent-block/lib/knl/init.lua");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "knl"])
        .assert()
        .success()
        .stdout(predicate::str::contains(target.to_string_lossy().as_ref()));

    let written = std::fs::read_to_string(&target).expect("the copy is there");
    assert_eq!(
        written.lines().next().expect("a header"),
        format!(
            "-- vendored from agent-block {} (embedded knl)",
            env!("CARGO_PKG_VERSION")
        ),
        "{written}"
    );
    assert!(
        written.contains("require(\"embedded.knl\")"),
        "the header names the original: {written}"
    );

    let specs = project.path().join(".agent-block/lib/knl/spec");
    let written_specs: Vec<String> = std::fs::read_dir(&specs)
        .expect("the specs came with it")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !written_specs.is_empty(),
        "the kernel's specs are what check a replacement: {written_specs:?}"
    );
}

/// An unknown name is answered with what there is, and a sub-module with the
/// module it is part of.
#[test]
fn an_unknown_name_and_a_sub_module_are_refused_with_the_way_forward() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    let vendor = |name: &str| {
        let mut cmd = common::agent_block_cmd();
        cmd.env("AGENT_BLOCK_HOME", home.path())
            .args(["-p", &project.path().to_string_lossy()])
            .args(["vendor", name]);
        cmd
    };

    vendor("nope")
        .assert()
        .failure()
        .stderr(predicate::str::contains("`nope` is not an embedded module"))
        .stderr(predicate::str::contains("session"));

    vendor("llm_proto.openai")
        .assert()
        .failure()
        .stderr(predicate::str::contains("vendor llm_proto"));
}

/// A module vendors whole: the root and its sub-modules, laid out the way
/// `require` reads a dotted name.
#[test]
fn a_module_with_sub_modules_is_written_whole() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "llm_proto"])
        .assert()
        .success();

    let lib = project.path().join(".agent-block/lib/llm_proto");
    for rel in ["init.lua", "openai.lua", "anthropic.lua"] {
        let path = lib.join(rel);
        assert!(path.is_file(), "missing {}", path.display());
    }
    let sub = std::fs::read_to_string(lib.join("openai.lua")).expect("read");
    assert!(
        sub.starts_with(&format!(
            "-- vendored from agent-block {} (embedded llm_proto.openai)",
            env!("CARGO_PKG_VERSION")
        )),
        "{sub}"
    );
}

/// `agent` is an embedded consumer, and its copy lands on the require path like
/// every other one — never in `blocks/`, which is the project's own to fill.
#[test]
fn an_embedded_consumer_lands_on_the_require_path() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains(".agent-block/lib/agent/init.lua"));

    assert!(project
        .path()
        .join(".agent-block/lib/agent/init.lua")
        .is_file());
    assert!(
        !project.path().join(".agent-block/blocks").exists(),
        "vendor does not write entry points"
    );
}

/// The help says both forms, because the two do different things and one of
/// them takes no name at all.
#[test]
fn the_help_gives_both_forms() {
    common::agent_block_cmd()
        .args(["vendor", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "agent-block vendor [--path <dir>] [--force] <name>...",
        ))
        .stdout(predicate::str::contains("agent-block vendor --list"));
}

/// The Lua half of a bridge vendors like any module, and the copy is what the
/// host runs: `std.fs.register_tools` comes from `.agent-block/lib/fs_tools/`
/// once that exists, with no install in between.
#[test]
fn a_vendored_bridge_tool_module_is_the_one_the_host_runs() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    let target = project.path().join(".agent-block/lib/fs_tools/init.lua");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "fs_tools"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fs_tools/init.lua"));

    // Mark the copy where the host will see it. The module is a library — it
    // answers a table and the bridge installs what that table holds — so the
    // mark is one more export, added before the `return M` the copy ends with.
    let source = std::fs::read_to_string(&target).expect("read the copy");
    let marked = source.replace(
        "\nreturn M\n",
        "\nfunction M.vendored_marker()\n    return 'from the copy'\nend\n\nreturn M\n",
    );
    assert_ne!(marked, source, "the vendored copy ends with `return M`");
    std::fs::write(&target, marked).expect("write");

    let script = project.path().join("probe.lua");
    std::fs::write(
        &script,
        "local marker = std.fs.vendored_marker\n\
         print('marker=' .. tostring(marker and marker()))\n\
         print('tools=' .. tostring(type(std.fs.register_tools)))\n",
    )
    .expect("write");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &script.to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("marker=from the copy"))
        .stdout(predicate::str::contains("tools=function"));

    // And with no copy, the embedded module is what runs — the marker is nil.
    let bare = tempdir().expect("tempdir");
    let script2 = bare.path().join("probe.lua");
    std::fs::copy(&script, &script2).expect("copy");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &bare.path().to_string_lossy()])
        .args(["-s", &script2.to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("marker=nil"))
        .stdout(predicate::str::contains("tools=function"));
}

/// The delegation idiom, on a bridge's tool module: a project's `fs_tools`
/// wraps ONE function of the embedded one and inherits the rest, and what the
/// host installs on `std.fs` is the wrapper — reached by a script that calls
/// `std.fs.tool_specs`, not by requiring anything itself.
///
/// The inherited half is the other assertion here. `register_tools` is not on
/// the wrapper table at all; it is on the base, behind `__index`, and a script
/// that calls it has to find the embedded one and get an array of names back.
#[test]
fn a_vendored_tool_module_can_wrap_the_embedded_one() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    let target = project.path().join(".agent-block/lib/fs_tools/init.lua");
    std::fs::create_dir_all(target.parent().expect("the copy has a parent")).expect("mkdir");
    std::fs::write(
        &target,
        "local base = require(\"embedded.fs_tools\")\n\
         local M = setmetatable({}, { __index = base })\n\
         function M.tool_specs(o)\n\
         \x20   print(\"WRAPPED\")\n\
         \x20   return base.tool_specs(o)\n\
         end\n\
         return M\n",
    )
    .expect("write the wrapper");

    let script = project.path().join("probe.lua");
    std::fs::write(
        &script,
        "local specs = std.fs.tool_specs({ allowed = { \"read\" } })\n\
         print('spec_name=' .. specs[1].name)\n\
         print('handler=' .. type(specs[1].handler))\n\
         print('inherited=' .. type(std.fs.register_tools))\n",
    )
    .expect("write");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &script.to_string_lossy()])
        .assert()
        .success()
        // The wrapper ran …
        .stdout(predicate::str::contains("WRAPPED"))
        // … and what it delegated to is the embedded module's own answer.
        .stdout(predicate::str::contains("spec_name=fs_read"))
        .stdout(predicate::str::contains("handler=function"))
        // The function the wrapper did not define came through `__index`.
        .stdout(predicate::str::contains("inherited=function"));
}

/// A pack is written, with the reason a whole copy is rarely what was meant on
/// stderr beside it.
#[test]
fn vendoring_a_pack_warns_and_writes_it() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "policy"])
        .assert()
        .success()
        .stderr(predicate::str::contains("`policy` is a pack"));

    assert!(project
        .path()
        .join(".agent-block/lib/policy/init.lua")
        .is_file());
}

/// A module vendors with its specs: they land under `spec/` beside the copy,
/// each with a header that says what it is, the listing counts them, and a
/// spec's `require` of its support file resolves from the copy. A module with
/// no `spec/` writes only itself.
#[test]
fn a_module_vendors_with_its_specs_beside_it() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    let lib = project.path().join(".agent-block/lib");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "policy"])
        .assert()
        .success()
        .stdout(predicate::str::contains("policy/init.lua"))
        .stdout(predicate::str::contains("policy/spec/api_spec.lua"))
        .stdout(predicate::str::contains("policy/spec/support.lua"));

    let spec = std::fs::read_to_string(lib.join("policy/spec/api_spec.lua")).expect("the spec");
    let mut lines = spec.lines();
    assert_eq!(
        lines.next().expect("a header"),
        format!(
            "-- vendored from agent-block {} (spec policy/spec/api_spec.lua)",
            env!("CARGO_PKG_VERSION")
        ),
        "{spec}"
    );
    assert!(
        lines
            .next()
            .expect("a second line")
            .contains("checks the vendored module"),
        "{spec}"
    );
    assert!(
        spec.contains("require(\"policy.spec.support\")"),
        "the spec still reaches its support file by the name it always used"
    );

    // The listing counts the specs beside the module that has them.
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "--list"])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"(?m)^policy \(spec/: \d+\).*vendored$").expect("re"));

    // `session` has no spec/: only the module itself is written.
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "session"])
        .assert()
        .success();
    assert!(
        !lib.join("session/spec").exists(),
        "session vendors no spec/"
    );
}
