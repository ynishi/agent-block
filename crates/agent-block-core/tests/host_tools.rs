//! Integration test: SDK consumer injects a Rust-implemented tool via
//! `BlockConfig.host_tools`. The Lua script invokes it via
//! `tool.call(...)` and the host captures the result via
//! `bus.emit("_", r)` + `host_handler`.
//!
//! Also exercises `inspect_tools()` for the static introspection
//! contract (host_tools + embedded blocks merged, no MCP).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use agent_block_core::bus::{AckResult, Handler};
use agent_block_core::host::{
    inspect_tools, run, BlockConfig, HostToolSpec, ScriptSource, ToolHandler, ToolSource,
};
use agent_block_types::error::{BlockError, BlockResult};

struct AdderTool;

#[async_trait]
impl ToolHandler for AdderTool {
    async fn call(&self, input: Value) -> BlockResult<Value> {
        let a = input
            .get("a")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| BlockError::Runtime("missing 'a'".into()))?;
        let b = input
            .get("b")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| BlockError::Runtime("missing 'b'".into()))?;
        Ok(json!({ "sum": a + b }))
    }
}

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

/// Rust-implemented tool `add(a,b)` is callable from the Lua script and
/// the returned value is forwarded back to the host via `bus.emit`.
#[tokio::test]
async fn host_tool_dispatches_through_lua_registry() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (tx, rx) = oneshot::channel::<Value>();
    let captor: Arc<dyn Handler> = Arc::new(CaptureHandler {
        tx: tokio::sync::Mutex::new(Some(tx)),
    });

    let adder = HostToolSpec {
        name: "add".to_string(),
        description: "Add two integers.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "a": { "type": "integer" },
                "b": { "type": "integer" },
            },
            "required": ["a", "b"],
        }),
        group: Some("math".to_string()),
        handler: Arc::new(AdderTool),
    };

    let script = r#"
        local r = tool.call("add", { a = 17, b = 25 })
        bus.emit("_", r)
    "#;

    let config = BlockConfig {
        script: ScriptSource::Inline {
            source: script.to_string(),
            name: "host_tool_smoke.lua".to_string(),
        },
        project_root: dir.path().to_path_buf(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: None,
        context: None,
        host_handlers: HashMap::new(),
        host_handler: Some(captor),
        host_tools: vec![adder],
        http_client: None,
        sql_path: None,
        kv_path: None,
        ts_path: None,
        extra_globals: HashMap::new(),
        auto_serve_bus: true,
        shutdown_token: None,
    };

    run(config).await.expect("run ok");

    let payload = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("oneshot did not receive within 2s")
        .expect("oneshot canceled");

    assert_eq!(payload, json!({ "sum": 42 }));
}

/// `inspect_tools` merges `host_tools` and embedded blocks, with
/// proper `ToolSource` tagging. MCP servers are omitted.
#[tokio::test]
async fn inspect_tools_lists_host_and_embedded_sources() {
    let adder = HostToolSpec {
        name: "add".to_string(),
        description: "Add two integers.".to_string(),
        input_schema: json!({ "type": "object" }),
        group: Some("math".to_string()),
        handler: Arc::new(AdderTool),
    };

    let config = BlockConfig {
        script: ScriptSource::DefaultAgent,
        project_root: std::env::temp_dir(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: None,
        context: None,
        host_handlers: HashMap::new(),
        host_handler: None,
        host_tools: vec![adder],
        http_client: None,
        sql_path: None,
        kv_path: None,
        ts_path: None,
        extra_globals: HashMap::new(),
        auto_serve_bus: false,
        shutdown_token: None,
    };

    let tools = inspect_tools(&config);

    let host_rust: Vec<&str> = tools
        .iter()
        .filter(|m| m.source == ToolSource::HostRust)
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(host_rust, vec!["add"]);

    let embedded: Vec<&str> = tools
        .iter()
        .filter(|m| m.source == ToolSource::EmbeddedBlock)
        .map(|m| m.name.as_str())
        .collect();
    assert!(
        embedded.contains(&"agent"),
        "embedded list should include 'agent', got {embedded:?}"
    );
    assert!(
        embedded.contains(&"compile_loop"),
        "embedded list should include 'compile_loop', got {embedded:?}"
    );
}
