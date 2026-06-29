//! Integration test: SDK consumer parameterises an inline Lua script
//! via `BlockConfig.extra_globals` without baking values into the
//! source. Also exercises `BlockConfig.sql_path` / `kv_path` /
//! `ts_path` override paths (set to `:memory:` to isolate the test
//! from any host-wide SQLite files).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use agent_block_core::bus::{AckResult, Handler};
use agent_block_core::host::{run, BlockConfig, ScriptSource};

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

/// `extra_globals` injects arbitrary Rust values into the Lua global
/// namespace before the script runs. Strings, numbers, booleans, and
/// nested JSON objects all round-trip via `json_to_lua`.
#[tokio::test]
async fn extra_globals_are_visible_to_lua_script() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (tx, rx) = oneshot::channel::<Value>();
    let captor: Arc<dyn Handler> = Arc::new(CaptureHandler {
        tx: tokio::sync::Mutex::new(Some(tx)),
    });

    let mut extra_globals = HashMap::new();
    extra_globals.insert("_USER_ID".to_string(), json!("u_42"));
    extra_globals.insert("_TENANT".to_string(), json!("acme"));
    extra_globals.insert(
        "_FEATURE_FLAGS".to_string(),
        json!({ "beta_search": true, "max_quota": 100 }),
    );

    let script = r#"
        bus.emit("_", {
            user_id = _USER_ID,
            tenant = _TENANT,
            beta_search = _FEATURE_FLAGS.beta_search,
            max_quota = _FEATURE_FLAGS.max_quota,
        })
    "#;

    let config = BlockConfig {
        script: ScriptSource::Inline {
            source: script.to_string(),
            name: "extra_globals_smoke.lua".to_string(),
        },
        project_root: dir.path().to_path_buf(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: None,
        context: None,
        host_handlers: HashMap::new(),
        host_handler: Some(captor),
        host_tools: Vec::new(),
        http_client: None,
        sql_path: Some(PathBuf::from(":memory:")),
        kv_path: Some(PathBuf::from(":memory:")),
        ts_path: Some(PathBuf::from(":memory:")),
        extra_globals,
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
            "user_id": "u_42",
            "tenant": "acme",
            "beta_search": true,
            "max_quota": 100,
        })
    );
}

/// All three `*_path` overrides accept `:memory:` and the script runs
/// cleanly without touching any on-disk SQLite file.
#[tokio::test]
async fn sqlite_path_overrides_accept_in_memory_sentinel() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (tx, rx) = oneshot::channel::<Value>();
    let captor: Arc<dyn Handler> = Arc::new(CaptureHandler {
        tx: tokio::sync::Mutex::new(Some(tx)),
    });

    let config = BlockConfig {
        script: ScriptSource::Inline {
            source: r#"bus.emit("_", { ok = true })"#.to_string(),
            name: "noop.lua".to_string(),
        },
        project_root: dir.path().to_path_buf(),
        relay_url: None,
        secret_key: None,
        mcp_rpc_timeout: Duration::from_secs(30),
        prompt: None,
        context: None,
        host_handlers: HashMap::new(),
        host_handler: Some(captor),
        host_tools: Vec::new(),
        http_client: None,
        sql_path: Some(PathBuf::from(":memory:")),
        kv_path: Some(PathBuf::from(":memory:")),
        ts_path: Some(PathBuf::from(":memory:")),
        extra_globals: HashMap::new(),
        auto_serve_bus: true,
        shutdown_token: None,
    };

    run(config).await.expect("run ok");

    let payload = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("oneshot did not receive within 2s")
        .expect("oneshot canceled");

    assert_eq!(payload, json!({ "ok": true }));
}
