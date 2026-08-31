//! Integration test: MCP server subprocesses spawned via `McpManager::connect`
//! do not inherit the host's own credential variables, and `opts.env` injects
//! variables after that removal.
//!
//! The probe is a `sh -c` "server": it dumps the two variables under test into
//! a file inside a tempdir and then blocks on `cat`, so the MCP handshake times
//! out (as in `mcp_cwd.rs`) while the spawn itself — the thing under test — has
//! already happened. Only the two variables are dumped, never the whole
//! environment: a failing assertion prints the captured content, and in CI a
//! full `env` dump would carry real secrets.

use std::time::Duration;

use agent_block_mcp::McpManager;
use agent_block_types::error::BlockError;

const HOST_KEY: &str = "dummy-anthropic-6b2e0d";

/// Spawn the probe with the given injected env and return what the child saw.
/// `label` keeps each probe's dump file distinct so a later probe can never
/// read an earlier probe's content.
async fn probe_child_env(dir: &std::path::Path, label: &str, env: &[(String, String)]) -> String {
    let out = dir.join(format!("probe-{label}.txt"));
    let script = format!(
        "printf 'A=%s\\nT=%s\\n' \"$ANTHROPIC_API_KEY\" \"$MY_SERVER_TOKEN\" > {}; exec cat",
        out.display()
    );
    let args = ["-c".to_string(), script];
    let mut mgr =
        McpManager::with_rpc_timeout(Duration::from_millis(300)).expect("McpManager::new");

    // `sh` is not an MCP server, so the handshake times out. A spawn failure
    // would surface as BlockError::Mcp("spawn ...") and must fail the test.
    match mgr
        .connect("env_probe", "sh", &args, false, Some(dir), env)
        .await
    {
        Err(BlockError::Timeout(_)) => {}
        other => panic!("expected handshake timeout after a successful spawn, got {other:?}"),
    }
    let _ = mgr.disconnect_all().await;

    // The child writes before blocking on `cat`, but the write races the
    // handshake timeout on a loaded machine — poll until the line we assert on
    // is present.
    for _ in 0..40 {
        if let Ok(content) = std::fs::read_to_string(&out) {
            if content.contains("T=") {
                return content;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("probe child never wrote {}", out.display());
}

#[tokio::test]
async fn mcp_child_loses_own_credentials_and_takes_explicit_env() {
    // Single test (not two) so the process-wide env mutation below cannot race
    // a sibling test spawning its own child.
    std::env::set_var("ANTHROPIC_API_KEY", HOST_KEY);
    let dir = tempfile::tempdir().expect("tempdir");

    // 1. Host credential stripped; an ordinary injected variable arrives.
    let injected = [("MY_SERVER_TOKEN".to_string(), "injected-9f3a".to_string())];
    let seen = probe_child_env(dir.path(), "stripped", &injected).await;
    assert!(
        seen.contains("A=\n"),
        "ANTHROPIC_API_KEY must not reach the MCP child, child saw: {seen}"
    );
    assert!(
        !seen.contains(HOST_KEY),
        "host credential leaked into the MCP child: {seen}"
    );
    assert!(
        seen.contains("T=injected-9f3a\n"),
        "opts.env variable must reach the child, child saw: {seen}"
    );

    // 2. Explicit injection is applied after the removal, so it is the escape
    //    hatch for handing a stripped variable to a server that needs it.
    let explicit = [("ANTHROPIC_API_KEY".to_string(), "explicit-7c1b".to_string())];
    let seen = probe_child_env(dir.path(), "explicit", &explicit).await;
    assert!(
        seen.contains("A=explicit-7c1b\n"),
        "explicit env injection must win over the credential strip, child saw: {seen}"
    );
    assert!(
        !seen.contains(HOST_KEY),
        "host credential leaked into the MCP child: {seen}"
    );
}
