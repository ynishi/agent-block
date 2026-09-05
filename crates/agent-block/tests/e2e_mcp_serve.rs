//! E2E coverage for `agent-block mcp` — the block server, driven by this
//! workspace's own MCP client.
//!
//! The client half (`agent-block-mcp`) already spawns servers as child
//! processes and speaks the protocol, so pointing it at the binary's new server
//! mode exercises both sides of the boundary through a real stdio transport
//! rather than a hand-built request. That also covers the one failure this mode
//! can produce that nothing else would catch: a stray log line on stdout would
//! corrupt the JSON-RPC stream, and every assertion below would stop working.

mod common;

use agent_block_mcp::McpManager;
use std::path::Path;

/// Write a block into `dir` and return its name.
fn write_block(dir: &Path, name: &str, body: &str) -> String {
    std::fs::write(dir.join(format!("{name}.lua")), body).expect("write block");
    name.to_string()
}

/// Connect a client to `agent-block mcp` serving `dir`.
async fn connect(dir: &Path) -> McpManager {
    let bin = assert_cmd::cargo::cargo_bin("agent-block");
    let mut mgr = McpManager::new();
    mgr.connect(
        "blocks",
        bin.to_str().expect("binary path is utf-8"),
        &[
            "mcp".to_string(),
            "--block-dir".to_string(),
            dir.display().to_string(),
            "--project".to_string(),
            dir.display().to_string(),
        ],
        false,
        Some(dir),
        // The user tier is served too; pinning it to the tempdir (which has
        // no `blocks/`) keeps the developer's own `~/.agent-block/blocks/`
        // out of the registry under test.
        &[("AGENT_BLOCK_HOME".to_string(), dir.display().to_string())],
    )
    .await
    .expect("connect to agent-block mcp");
    mgr
}

/// The text of the first content block of a `tools/call` result.
fn content_text(result: &serde_json::Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// A block's JSON return value is what the caller receives, and the registry
/// is what the `block` argument accepts — the two halves of the contract the
/// guide states.
#[tokio::test]
async fn a_registered_block_runs_and_returns_its_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_block(
        dir.path(),
        "echo",
        "-- Echo the prompt back as JSON.\nreturn std.json.encode({ ok = true, prompt = _PROMPT })\n",
    );

    let mgr = connect(dir.path()).await;

    // The callable set is the registered set: the model sees the names in the
    // schema instead of having to guess a path.
    let tools = mgr.list_tools("blocks").await.expect("list_tools");
    let tool = tools
        .as_array()
        .expect("tools array")
        .iter()
        .find(|t| t["name"] == "run_block")
        .expect("run_block is served");
    let names = tool["inputSchema"]["properties"]["block"]["enum"]
        .as_array()
        .expect("block enum");
    assert_eq!(names.len(), 1, "one block registered");
    assert_eq!(names[0], "echo");
    assert!(
        tool["description"]
            .as_str()
            .unwrap_or_default()
            .contains("Echo the prompt back as JSON."),
        "the block's own header comment reaches the tool description: {tool:?}"
    );

    let result = mgr
        .call_tool(
            "blocks",
            "run_block",
            serde_json::json!({ "block": "echo", "prompt": "hello from the caller" }),
        )
        .await
        .expect("call run_block");

    assert_ne!(result["isError"], serde_json::Value::Bool(true));
    let decoded: serde_json::Value =
        serde_json::from_str(&content_text(&result)).expect("the block returned a JSON string");
    assert_eq!(decoded["ok"], true);
    assert_eq!(decoded["prompt"], "hello from the caller");
}

/// A block that fails to run is a failed call, not a successful call carrying
/// a failure — the distinction the envelope keeps as two separate fields.
#[tokio::test]
async fn a_block_that_raises_comes_back_as_a_failed_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_block(
        dir.path(),
        "boom",
        "-- Always raises.\nerror(\"deliberate failure\")\n",
    );

    let mgr = connect(dir.path()).await;
    let result = mgr
        .call_tool(
            "blocks",
            "run_block",
            serde_json::json!({ "block": "boom" }),
        )
        .await
        .expect("the call itself completes");

    assert_eq!(result["isError"], true, "result: {result}");
    assert!(
        content_text(&result).contains("deliberate failure"),
        "the Lua error reaches the caller: {result}"
    );
}

/// An unregistered name is rejected by the server rather than resolved against
/// the filesystem, so this surface cannot be walked outside the block dirs.
#[tokio::test]
async fn an_unregistered_block_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_block(
        dir.path(),
        "real",
        "return std.json.encode({ ok = true })\n",
    );
    let outside = tempfile::tempdir().expect("tempdir");
    std::fs::write(outside.path().join("secret.lua"), "return \"leaked\"\n").expect("write");

    let mgr = connect(dir.path()).await;

    for name in [
        "secret",
        "../secret",
        outside
            .path()
            .join("secret")
            .display()
            .to_string()
            .trim_start_matches('/'),
    ] {
        let err = mgr
            .call_tool(
                "blocks",
                "run_block",
                serde_json::json!({ "block": name.to_string() }),
            )
            .await;
        assert!(err.is_err(), "'{name}' must not be callable");
    }
}

/// Resources carry the authoring contract and the sources, so a caller can
/// learn how to write a block without being handed the repository.
#[tokio::test]
async fn resources_serve_the_guide_the_registry_and_the_sources() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_block(
        dir.path(),
        "echo",
        "-- Echo the prompt back as JSON.\nreturn std.json.encode({ ok = true })\n",
    );

    let mgr = connect(dir.path()).await;

    let listed = mgr.list_resources("blocks").await.expect("list_resources");
    let uris: Vec<String> = listed
        .as_array()
        .expect("resources array")
        .iter()
        .filter_map(|r| r["uri"].as_str().map(str::to_string))
        .collect();
    assert!(
        uris.contains(&"agent-block://guide".to_string()),
        "{uris:?}"
    );
    assert!(
        uris.contains(&"agent-block://blocks".to_string()),
        "{uris:?}"
    );
    assert!(
        uris.contains(&"agent-block://blocks/echo".to_string()),
        "{uris:?}"
    );

    let guide = mgr
        .read_resource("blocks", "agent-block://guide")
        .await
        .expect("read guide");
    assert!(
        guide.to_string().contains("A block returns a JSON string"),
        "the guide states the return contract"
    );

    // The listing's type and the read's type have to agree over the wire, not
    // only in the server's own head: a client routing on the content's type is
    // reading the second one.
    let advertised = |uri: &str| -> String {
        listed
            .as_array()
            .expect("resources array")
            .iter()
            .find(|r| r["uri"] == uri)
            .and_then(|r| r["mimeType"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(advertised("agent-block://guide"), "text/markdown");
    assert_eq!(
        guide["contents"][0]["mimeType"], "text/markdown",
        "read must not downgrade the advertised type: {guide}"
    );

    let registry = mgr
        .read_resource("blocks", "agent-block://blocks")
        .await
        .expect("read registry");
    assert!(registry.to_string().contains("echo"), "{registry}");
    assert_eq!(advertised("agent-block://blocks"), "application/json");
    assert_eq!(registry["contents"][0]["mimeType"], "application/json");

    let source = mgr
        .read_resource("blocks", "agent-block://blocks/echo")
        .await
        .expect("read block source");
    assert!(
        source.to_string().contains("std.json.encode"),
        "the block's source is served: {source}"
    );
    assert_eq!(advertised("agent-block://blocks/echo"), "text/x-lua");
    assert_eq!(source["contents"][0]["mimeType"], "text/x-lua");

    assert!(
        mgr.read_resource("blocks", "agent-block://blocks/nope")
            .await
            .is_err(),
        "an unknown block has no source to read"
    );
}

/// A block dropped in after the server started is callable without a restart:
/// the registry is scanned per request, matching how a new script needs
/// nothing but a new `-s` argument on the CLI.
#[tokio::test]
async fn a_block_added_after_startup_is_picked_up() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_block(dir.path(), "first", "return std.json.encode({ n = 1 })\n");

    let mgr = connect(dir.path()).await;
    let before = mgr.list_tools("blocks").await.expect("list_tools");
    let before_names = before[0]["inputSchema"]["properties"]["block"]["enum"]
        .as_array()
        .expect("enum")
        .len();
    assert_eq!(before_names, 1);

    write_block(dir.path(), "second", "return std.json.encode({ n = 2 })\n");

    let after = mgr.list_tools("blocks").await.expect("list_tools");
    let after_names: Vec<String> = after[0]["inputSchema"]["properties"]["block"]["enum"]
        .as_array()
        .expect("enum")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert_eq!(after_names, ["first", "second"]);
}

/// `-s` keeps working with the subcommand in place: the original invocation is
/// what every example, runbook and habit already uses.
#[test]
fn the_script_form_still_runs() {
    common::agent_block_cmd()
        .args(["-s", &common::fixture("hello.lua")])
        .assert()
        .success();
}

/// With a subcommand in the grammar, `script` can no longer be clap-required,
/// so the missing-script error is ours. It has to point at both ways forward —
/// `e2e_basic::missing_script_arg_shows_usage` covers the flag half.
#[test]
fn no_script_and_no_subcommand_points_at_the_subcommand_too() {
    common::agent_block_cmd()
        .assert()
        .failure()
        .stderr(predicates::str::contains("use a subcommand"));
}
