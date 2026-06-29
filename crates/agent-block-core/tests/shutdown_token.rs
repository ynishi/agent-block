//! Integration test: SDK consumer supplies a `shutdown_token` to
//! `BlockConfig`, spawns `run()` as a tokio task driving a long-running
//! script, then cancels the token from outside. The script is
//! interrupted, the shutdown sequence still runs, and `run()` returns
//! `BlockError::Cancelled`.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use agent_block_core::host::{run, BlockConfig, ScriptSource};
use agent_block_types::error::BlockError;

#[tokio::test]
async fn shutdown_token_interrupts_long_running_script() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script_path = dir.path().join("loop.lua");
    let mut f = std::fs::File::create(&script_path).expect("create script");
    // Spin in pure-Lua so the AsyncIsle debug hook is the only thing
    // that can break us out. Long enough that the test would time out
    // if cancellation does not work.
    writeln!(
        f,
        r#"
        local n = 0
        while true do
            n = n + 1
            if n % 1000 == 0 then
                -- give the scheduler a chance to fire the cancel hook
                coroutine.yield()
            end
        end
        "#
    )
    .expect("write script");
    drop(f);

    let shutdown = CancellationToken::new();

    let config = BlockConfig {
        script: ScriptSource::Path(script_path.clone()),
        project_root: dir.path().to_path_buf(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: None,
        context: None,
        host_handlers: HashMap::new(),
        host_handler: None,
        host_tools: Vec::new(),
        http_client: None,
        sql_path: None,
        kv_path: None,
        ts_path: None,
        extra_globals: HashMap::new(),
        auto_serve_bus: false,
        shutdown_token: Some(shutdown.clone()),
    };

    let handle = tokio::spawn(run(config));

    // Let the script reach the busy loop, then cancel.
    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("run() did not return within 5s after cancel")
        .expect("join error");

    match result {
        Err(BlockError::Cancelled) => {}
        other => panic!("expected BlockError::Cancelled, got {other:?}"),
    }
}
