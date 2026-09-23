//! `agent-block knl <verb>` with no `-p`: the project is the working
//! directory, and the reader finds the database the host wrote there.
//!
//! The two runs have to agree about one thing — which project this is — and
//! they reach it from opposite sides: the host writes the database at a path
//! derived from the root it resolved, and the reader has to derive the same
//! path from the root the command line gave it. `-p/--project` defaults to
//! `.`, so for a while they did not: the host canonicalized and the
//! subcommand did not, `project_slug` left `.` as `.`, and a reader run from
//! inside the project was told there was no database — naming a path under
//! `projects/./` that nothing had ever written. So the test runs the binary
//! for both halves, because the agreement between them is the whole subject,
//! and a test that computed the path itself would be a third opinion.

mod common;

use predicates::prelude::*;

/// Open one session in the project's own database and close it.
const SEED: &str = r#"
local knl = require("knl")
knl.session({ owner = "reader-test" }, function(s)
    s:append({ kind = "msg_user", data = { content = "hi" } })
end)
"#;

/// A project directory holding the seed script, and a home to put the
/// database under. Both temporary, and the home is named by the environment
/// so the run cannot reach the developer's own.
fn seeded_project() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("tempdir");
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::write(project.path().join("seed.lua"), SEED).expect("write the seed script");

    common::agent_block_cmd()
        .current_dir(project.path())
        .env("AGENT_BLOCK_HOME", home.path())
        // The flag's own environment form, unset: this test is about what
        // happens with NOTHING naming the project, and a developer who has it
        // exported would otherwise be testing their own shell.
        .env_remove("AGENT_BLOCK_PROJECT")
        .args(["-s", "seed.lua"])
        .assert()
        .success();

    (home, project)
}

#[test]
fn sessions_with_no_project_flag_reads_the_working_directory() {
    let (home, project) = seeded_project();

    common::agent_block_cmd()
        .current_dir(project.path())
        .env("AGENT_BLOCK_HOME", home.path())
        .env_remove("AGENT_BLOCK_PROJECT")
        .args(["knl", "sessions"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"session\""));
}

/// The same root, written the way the default writes it. `-p .` is what the
/// default IS, so if the two ever answer differently the default has stopped
/// being resolved somewhere.
#[test]
fn sessions_with_an_explicitly_relative_project_reads_the_same_database() {
    let (home, project) = seeded_project();

    common::agent_block_cmd()
        .current_dir(project.path())
        .env("AGENT_BLOCK_HOME", home.path())
        .env_remove("AGENT_BLOCK_PROJECT")
        .args(["knl", "sessions", "-p", "."])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"session\""));
}

/// And the absolute form, which is the one that always worked — here so that
/// a change which fixed the default by breaking this would not pass.
#[test]
fn sessions_with_an_absolute_project_reads_the_same_database() {
    let (home, project) = seeded_project();

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .env_remove("AGENT_BLOCK_PROJECT")
        .args(["knl", "sessions"])
        .args(["-p", &project.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"session\""));
}

/// A root that is not there is refused in its own terms, rather than carried
/// into a path built from it and reported as a missing database.
#[test]
fn a_project_that_does_not_exist_is_refused_as_a_project() {
    let home = tempfile::tempdir().expect("tempdir");
    let project = tempfile::tempdir().expect("tempdir");
    let absent = project.path().join("no-such-directory");

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .env_remove("AGENT_BLOCK_PROJECT")
        .args(["knl", "sessions"])
        .args(["-p", &absent.to_string_lossy()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("could not be resolved"))
        .stderr(predicate::str::contains("no kernel database").not());
}

/// A project whose log has genuinely never been written says so, and names
/// the root it derived the path from — the fact a reader needs to tell "not
/// yet" from "wrong directory", which the slugged path alone does not give.
#[test]
fn a_project_with_no_database_names_the_project_it_looked_for() {
    let home = tempfile::tempdir().expect("tempdir");
    let project = tempfile::tempdir().expect("tempdir");

    common::agent_block_cmd()
        .current_dir(project.path())
        .env("AGENT_BLOCK_HOME", home.path())
        .env_remove("AGENT_BLOCK_PROJECT")
        .args(["knl", "sessions"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "no kernel database for the project at",
        ))
        .stderr(predicate::str::contains("-p/--project names another"));
}
