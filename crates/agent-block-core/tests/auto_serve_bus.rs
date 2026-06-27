//! Integration test: SDK-embed caller installs a host handler, runs a
//! script that calls `bus.emit(...)`, and expects the handler to receive
//! the payload before `run()` returns. Exercises the
//! `BlockConfig.auto_serve_bus` path end-to-end.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use agent_block_core::bus::{AckResult, Handler};
use agent_block_core::host::{run, BlockConfig};

struct CaptureHandler {
    tx: tokio::sync::Mutex<Option<oneshot::Sender<Value>>>,
}

#[async_trait]
impl Handler for CaptureHandler {
    async fn call(&self, _kind: String, _id: String, payload: Value, _meta: Value) -> AckResult {
        if let Some(tx) = self.tx.lock().await.take() {
            let _ = tx.send(payload);
        }
        Ok(Value::Null)
    }
}

#[tokio::test]
async fn auto_serve_bus_delivers_emit_to_host_handler() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script_path = dir.path().join("emit_script.lua");
    let mut f = std::fs::File::create(&script_path).expect("create script");
    writeln!(
        f,
        r#"bus.emit("worker_result", {{ ok = true, value = 42 }})"#
    )
    .expect("write script");
    drop(f);

    let (tx, rx) = oneshot::channel::<Value>();
    let handler: Arc<dyn Handler> = Arc::new(CaptureHandler {
        tx: tokio::sync::Mutex::new(Some(tx)),
    });

    let mut host_handlers: HashMap<String, Arc<dyn Handler>> = HashMap::new();
    host_handlers.insert("worker_result".to_string(), handler);

    let config = BlockConfig {
        script_path: script_path.clone(),
        project_root: dir.path().to_path_buf(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: None,
        context: None,
        host_handlers,
        auto_serve_bus: true,
        shutdown_token: None,
    };

    run(config).await.expect("run ok");

    let payload = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("oneshot did not receive within 2s")
        .expect("oneshot canceled");

    assert_eq!(payload, json!({ "ok": true, "value": 42 }));
}
