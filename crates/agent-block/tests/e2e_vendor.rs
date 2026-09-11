//! `agent-block vendor`, from the outside.
//!
//! The claim the subcommand makes is not that it writes a file — it is that the
//! file it writes is the one the project resolves from then on. So the test
//! that matters runs a script afterwards and asks the module which copy
//! answered: vendor `session`, mark the copy, `require("session")`, and read the
//! mark back. The rest pins what the caller is told along the way — the header
//! on the copy, the listing, the refusal to overwrite, and the seal.

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
    assert!(before.contains("sealed"), "{before}");

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

/// A module that cannot be shadowed cannot be vendored either: handing over a
/// copy would be handing over a file that fails the next run.
#[test]
fn a_sealed_module_is_refused_and_nothing_is_written() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["vendor", "knl"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("`knl` is sealed"))
        .stderr(predicate::str::contains("require(\"embedded.knl\")"));

    assert!(
        !project.path().join(".agent-block").exists(),
        "a refusal writes nothing"
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
