//! E2E coverage for MCP error surfacing and CLI guards.
//!
//! These lock down the "autonomous-agent visibility" contract:
//! bad configuration must fail at startup; MCP round-trip errors
//! must reach Lua AND emit a Rust-side `tracing::warn!` so they
//! are never silently swallowed.

mod common;

use predicates::prelude::*;

/// clap rejects `--mcp-timeout-secs 0` at argument-parse time.
/// A zero timeout would make every `tokio::time::timeout` fire
/// immediately — we want the misconfig to fail loudly at startup,
/// not at the first RPC.
#[test]
fn cli_rejects_zero_mcp_timeout() {
    common::agent_block_cmd()
        .args([
            "-s",
            &common::fixture("hello.lua"),
            "--mcp-timeout-secs",
            "0",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("0 is not in 1.."));
}

/// End-to-end error propagation:
///   - `connect` on a hung child → `BlockError::Timeout` surfaces in Lua
///   - `call_tool` / `list_tools` on an unknown server → structured error in Lua
///
/// The fixture asserts the Lua-visible shape; this test asserts the
/// Rust-tracing side (WARN lines) so we know errors are not swallowed
/// at the Rust layer either.
#[test]
fn mcp_errors_propagate_to_lua_and_tracing() {
    let assert = common::agent_block_cmd()
        .args([
            "-s",
            &common::fixture("mcp_errors.lua"),
            "--mcp-timeout-secs",
            "2",
        ])
        .env("RUST_LOG", "agent_block_mcp=warn")
        .assert()
        .success();

    // `tracing_subscriber::fmt` writes to stdout by default, so both
    // the Lua `print` markers AND the Rust `warn!` lines end up on
    // stdout. Assert both streams of evidence on the same channel.
    assert
        // Lua-side: each error path produces its stdout marker.
        .stdout(predicate::str::contains("CONNECT_TIMEOUT_ERR="))
        .stdout(predicate::str::contains("initialize 'stuck' timed out"))
        .stdout(predicate::str::contains("UNKNOWN_CALL_ERR="))
        .stdout(predicate::str::contains("no server named 'ghost'"))
        .stdout(predicate::str::contains("UNKNOWN_LIST_ERR="))
        .stdout(predicate::str::contains("FIXTURE_DONE"))
        // Rust-side: tracing WARN fires on every error path — the
        // "not silently swallowed" contract for autonomous runs.
        .stdout(predicate::str::contains("mcp initialize timed out"))
        .stdout(predicate::str::contains("mcp call_tool on unknown server"))
        .stdout(predicate::str::contains("mcp list_tools on unknown server"));
}
