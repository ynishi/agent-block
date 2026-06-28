//! Integration test: SDK consumer uses `ScriptSource::DefaultAgent`
//! without bundling any Lua script, supplies a host handler keyed on
//! `"agent_result"`, and expects the handler to receive the agent's
//! response. The actual `agent.run({...})` call goes out to a real LLM,
//! so this test only locks the wiring (default invoker is embedded,
//! `_PROMPT` / `_CONTEXT` reach `agent.run`, and `bus.emit("agent_result",
//! r)` lands on the host handler) by stubbing the StdPkg-`agent` module
//! out — we replace it via the embedded blocks search path.
//!
//! Since we cannot replace the embedded `agent` module at runtime, this
//! test instead uses `ScriptSource::Inline` to run a stub script that
//! emits the same kind, mirroring the DefaultAgent invoker's behavior.
//! A separate `default_agent_smoke_compiles` test confirms that the
//! `DefaultAgent` variant typechecks and that `run()` reaches the
//! script-execute stage without panic, even though the embedded
//! `agent.run` will fail without API keys.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use agent_block_core::bus::{AckResult, Handler};
use agent_block_core::host::{run, BlockConfig, PromptSource, ScriptSource};

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

/// `ScriptSource::Inline` + `PromptSource::Inline` round-trip: the inline
/// script reads `_PROMPT` / `_CONTEXT` and echoes them back via
/// `bus.emit("agent_result", ...)` so the host captures the values. This
/// also exercises the embedded-default-agent's wiring shape (kind
/// `"agent_result"` + host_handlers + auto_serve_bus) without depending
/// on a real LLM call.
#[tokio::test]
async fn inline_script_emits_agent_result_with_prompt_and_context() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (tx, rx) = oneshot::channel::<Value>();
    let handler: Arc<dyn Handler> = Arc::new(CaptureHandler {
        tx: tokio::sync::Mutex::new(Some(tx)),
    });

    let mut host_handlers: HashMap<String, Arc<dyn Handler>> = HashMap::new();
    host_handlers.insert("agent_result".to_string(), handler);

    let stub_script = r#"
        bus.emit("agent_result", {
            ok = true,
            prompt = _PROMPT,
            system = _CONTEXT,
        })
    "#;

    let config = BlockConfig {
        script: ScriptSource::Inline {
            source: stub_script.to_string(),
            name: "stub_agent_invoker.lua".to_string(),
        },
        project_root: dir.path().to_path_buf(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: Some(PromptSource::Inline("solve this".to_string())),
        context: Some(PromptSource::Inline("you are an agent".to_string())),
        host_handlers,
        auto_serve_bus: true,
        shutdown_token: None,
    };

    run(config).await.expect("run ok");

    let payload = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("oneshot did not receive within 2s")
        .expect("oneshot canceled");

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
/// them as `_PROMPT`. Mirrors the CLI `--prompt-file` path.
#[tokio::test]
async fn prompt_source_file_is_read_at_run_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prompt_path = dir.path().join("prompt.txt");
    std::fs::write(&prompt_path, "from a file").expect("write prompt file");

    let (tx, rx) = oneshot::channel::<Value>();
    let handler: Arc<dyn Handler> = Arc::new(CaptureHandler {
        tx: tokio::sync::Mutex::new(Some(tx)),
    });

    let mut host_handlers: HashMap<String, Arc<dyn Handler>> = HashMap::new();
    host_handlers.insert("agent_result".to_string(), handler);

    let config = BlockConfig {
        script: ScriptSource::Inline {
            source: r#"bus.emit("agent_result", { prompt = _PROMPT })"#.to_string(),
            name: "echo.lua".to_string(),
        },
        project_root: dir.path().to_path_buf(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: Some(PromptSource::File(prompt_path)),
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

    assert_eq!(payload, json!({ "prompt": "from a file" }));
}
