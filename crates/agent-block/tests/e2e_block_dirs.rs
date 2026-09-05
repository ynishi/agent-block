//! The `blocks/` / `lib/` split and its tiers, from the outside.
//!
//! `blocks/` holds entry points and `lib/` holds modules, at two levels each:
//! the project root and `$AGENT_BLOCK_HOME`. These tests pin the three facts
//! the split rests on — a block resolves by name from either tier, a module
//! resolves by `require` from either tier with the project winning, and the
//! two never cross (a file in `blocks/` cannot be required, a file in `lib/`
//! cannot be run by name). The MCP server is driven through the same tiers
//! so that a block is the same block on both surfaces without a `--block-dir`.

mod common;

use agent_block_mcp::McpManager;
use predicates::prelude::*;
use std::path::Path;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, source: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("path has a parent")).expect("mkdir");
    std::fs::write(&path, source).expect("write");
}

const PRINTS_ORIGIN: &str = "print(\"BLOCK_FROM=\" .. _ORIGIN)\n";

fn block_printing(origin: &str) -> String {
    format!("-- A block that says where it lives.\nlocal _ORIGIN = \"{origin}\"\n{PRINTS_ORIGIN}")
}

// ── blocks by name ────────────────────────────────────────────────────────

#[test]
fn a_block_runs_by_name_from_the_project_tier() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "blocks/hello.lua",
        &block_printing("project"),
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-b", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BLOCK_FROM=project"));
}

#[test]
fn a_block_runs_by_name_from_the_user_tier_when_the_project_has_none() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        home.path(),
        "blocks/hello/init.lua",
        &block_printing("user"),
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["--block", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BLOCK_FROM=user"));
}

#[test]
fn the_project_tier_shadows_the_user_tier_by_name() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(home.path(), "blocks/hello.lua", &block_printing("user"));
    write(
        project.path(),
        "blocks/hello.lua",
        &block_printing("project"),
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-b", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BLOCK_FROM=project"));
}

#[test]
fn an_unknown_name_fails_and_lists_what_is_registered() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "blocks/hello.lua",
        &block_printing("project"),
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-b", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown block 'nope'"))
        .stderr(predicate::str::contains("registered: [hello]"));
}

/// A module in `lib/` is not an entry point: the name is not registered even
/// though the file would run if handed to `-s`.
#[test]
fn a_lib_module_is_not_a_block() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(project.path(), "lib/hello.lua", &block_printing("lib"));

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-b", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown block 'hello'"));
}

// ── modules by require ────────────────────────────────────────────────────

#[test]
fn a_module_resolves_from_the_project_lib() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "lib/mylib.lua",
        "return { from = \"project\" }\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_FROM=project"));
}

#[test]
fn a_module_resolves_from_the_user_lib_when_the_project_has_none() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        home.path(),
        "lib/mylib/init.lua",
        "return { from = \"user\" }\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_FROM=user"));
}

#[test]
fn the_project_lib_shadows_the_user_lib() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(home.path(), "lib/mylib.lua", "return { from = \"user\" }\n");
    write(
        project.path(),
        "lib/mylib.lua",
        "return { from = \"project\" }\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_FROM=project"));
}

/// The other half of the split: a file in `blocks/` is an entry point, not a
/// module, so `require` does not see it in either tier.
#[test]
fn a_block_cannot_be_required() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "blocks/mylib.lua",
        "return { from = \"project-blocks\" }\n",
    );
    write(
        home.path(),
        "blocks/mylib.lua",
        "return { from = \"user-blocks\" }\n",
    );

    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("lib_require.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("MYLIB_MISSING"));
}

// ── the MCP server sees the same tiers ────────────────────────────────────

/// Connect a client to `agent-block mcp` with no `--block-dir`: what it serves
/// is exactly what the tiers hold.
async fn connect(project: &Path, home: &Path) -> McpManager {
    let bin = assert_cmd::cargo::cargo_bin("agent-block");
    let mut mgr = McpManager::new();
    mgr.connect(
        "blocks",
        bin.to_str().expect("binary path is utf-8"),
        &[
            "mcp".to_string(),
            "--project".to_string(),
            project.display().to_string(),
        ],
        false,
        Some(project),
        &[("AGENT_BLOCK_HOME".to_string(), home.display().to_string())],
    )
    .await
    .expect("connect to agent-block mcp");
    mgr
}

#[tokio::test]
async fn the_mcp_server_serves_both_tiers_without_a_block_dir() {
    let home = tempdir().expect("tempdir");
    let project = tempdir().expect("tempdir");
    write(
        project.path(),
        "blocks/from_project/init.lua",
        "-- project block\nreturn std.json.encode({ from = \"project\" })\n",
    );
    write(
        home.path(),
        "blocks/from_user.lua",
        "-- user block\nreturn std.json.encode({ from = \"user\" })\n",
    );
    // Not a block: modules are never served.
    write(project.path(), "lib/helper.lua", "return {}\n");

    let mgr = connect(project.path(), home.path()).await;

    let tools = mgr.list_tools("blocks").await.expect("list_tools");
    let tool = tools
        .as_array()
        .expect("tools array")
        .iter()
        .find(|t| t["name"] == "run_block")
        .expect("run_block is served");
    let names: Vec<String> = tool["inputSchema"]["properties"]["block"]["enum"]
        .as_array()
        .expect("block enum")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, ["from_project", "from_user"]);

    let result = mgr
        .call_tool(
            "blocks",
            "run_block",
            serde_json::json!({ "block": "from_project" }),
        )
        .await
        .expect("call_tool");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert_eq!(text, "{\"from\":\"project\"}");
}
