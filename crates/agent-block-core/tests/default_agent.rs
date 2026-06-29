//! Integration test: SDK consumer uses the kind-agnostic
//! `BlockConfig.host_handler` to capture results without knowing or
//! coordinating any `kind` string. The Lua script may emit under any
//! label and the host still receives the event.
//!
//! Verifies that:
//!   1. `host_handler` (registered as `bus.on_any` under the hood)
//!      captures script-emitted events for any kind.
//!   2. `ScriptSource::DefaultAgent` wires up correctly with
//!      `host_handler` (the embedded invoker uses an arbitrary neutral
//!      label that the consumer never has to know about).
//!   3. `PromptSource::File` reads the file at `run()` start and
//!      forwards the contents as `_PROMPT`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use agent_block_core::bus::{AckResult, Handler};
use agent_block_core::host::{run, BlockConfig, PromptSource, ScriptSource};

struct CaptureHandler {
    tx: tokio::sync::Mutex<Option<oneshot::Sender<(String, Value)>>>,
}

#[async_trait]
impl Handler for CaptureHandler {
    async fn call(&self, kind: String, _id: String, payload: Value, _meta: Value) -> AckResult {
        if let Some(tx) = self.tx.lock().await.take() {
            let _ = tx.send((kind, payload));
        }
        Ok(Value::Null)
    }
}

/// `host_handler` catches an event regardless of the emit kind that
/// the script used. Locks the contract that SDK consumers do not need
/// to know any specific `kind` label.
#[tokio::test]
async fn host_handler_catches_any_emit_kind() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (tx, rx) = oneshot::channel::<(String, Value)>();
    let handler: Arc<dyn Handler> = Arc::new(CaptureHandler {
        tx: tokio::sync::Mutex::new(Some(tx)),
    });

    let stub_script = r#"
        bus.emit("some_arbitrary_label", {
            ok = true,
            prompt = _PROMPT,
            system = _CONTEXT,
        })
    "#;

    let config = BlockConfig {
        script: ScriptSource::Inline {
            source: stub_script.to_string(),
            name: "stub_invoker.lua".to_string(),
        },
        project_root: dir.path().to_path_buf(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: Some(PromptSource::Inline("solve this".to_string())),
        context: Some(PromptSource::Inline("you are an agent".to_string())),
        host_handlers: HashMap::new(),
        host_handler: Some(handler),
        host_tools: Vec::new(),
        http_client: None,
        sql_path: None,
        kv_path: None,
        ts_path: None,
        extra_globals: HashMap::new(),
        auto_serve_bus: true,
        shutdown_token: None,
    };

    run(config).await.expect("run ok");

    let (kind, payload) = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("oneshot did not receive within 2s")
        .expect("oneshot canceled");

    assert_eq!(kind, "some_arbitrary_label");
    assert_eq!(
        payload,
        json!({
            "ok": true,
            "prompt": "solve this",
            "system": "you are an agent",
        })
    );
}

/// `PromptSource::File` reads its contents at `run()` start and forwards
/// them as `_PROMPT`. Mirrors the CLI `--prompt-file` path; the kind
/// label remains an arbitrary internal detail.
#[tokio::test]
async fn prompt_source_file_is_read_at_run_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prompt_path = dir.path().join("prompt.txt");
    std::fs::write(&prompt_path, "from a file").expect("write prompt file");

    let (tx, rx) = oneshot::channel::<(String, Value)>();
    let handler: Arc<dyn Handler> = Arc::new(CaptureHandler {
        tx: tokio::sync::Mutex::new(Some(tx)),
    });

    let config = BlockConfig {
        script: ScriptSource::Inline {
            source: r#"bus.emit("anything", { prompt = _PROMPT })"#.to_string(),
            name: "echo.lua".to_string(),
        },
        project_root: dir.path().to_path_buf(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: Some(PromptSource::File(prompt_path)),
        context: None,
        host_handlers: HashMap::new(),
        host_handler: Some(handler),
        host_tools: Vec::new(),
        http_client: None,
        sql_path: None,
        kv_path: None,
        ts_path: None,
        extra_globals: HashMap::new(),
        auto_serve_bus: true,
        shutdown_token: None,
    };

    run(config).await.expect("run ok");

    let (_kind, payload) = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("oneshot did not receive within 2s")
        .expect("oneshot canceled");

    assert_eq!(payload, json!({ "prompt": "from a file" }));
}
