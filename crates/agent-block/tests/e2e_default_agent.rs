//! `ScriptSource::DefaultAgent` runs `agent.run` with what its profile
//! names — nothing about the run is a number the host chose — and hands
//! the result to the host's own sink.
mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use agent_block_core::bus::{AckResult, Handler};
use agent_block_core::host::{run, AgentProfile, BlockConfig, PromptSource, ScriptSource};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::oneshot;

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

/// The mock answers a tool call first and a final line second; with no
/// `echo` tool registered the first turn's call comes back to the model as
/// the failure it was and the second turn closes the run. Two requests and
/// the model's last line on the bus is the profile having reached
/// `agent.run` whole: provider, endpoint, and the two numbers it requires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_profile_is_what_the_embedded_invoker_runs_with() {
    let (base_url, call_count, _ct) = common::openai_mock::spawn_openai_mock_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let home = tempfile::tempdir().expect("tempdir");
    // The run's session log goes to the state dir; keep it out of the real one.
    std::env::set_var("AGENT_BLOCK_HOME", home.path());
    std::env::set_var("OPENAI_API_KEY", "dummy");

    let (tx, rx) = oneshot::channel::<Value>();
    let captor: Arc<dyn Handler> = Arc::new(CaptureHandler {
        tx: tokio::sync::Mutex::new(Some(tx)),
    });

    let profile = AgentProfile {
        provider: "openai".to_string(),
        base_url: Some(base_url),
        model: Some("qwen-test".to_string()),
        timeout: 30.0,
        max_iterations: 3,
        max_tokens: None,
    };
    let config = BlockConfig::builder(
        ScriptSource::DefaultAgent(profile),
        home.path().to_path_buf(),
    )
    .prompt(PromptSource::Inline(
        "Use the echo tool to say hello".to_string(),
    ))
    .mcp_rpc_timeout(Duration::from_secs(30))
    .host_handler(captor)
    .auto_serve_bus(true)
    .build();

    run(config).await.expect("run ok");

    let payload = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("no result within 5s")
        .expect("oneshot canceled");

    assert_eq!(payload["ok"], Value::Bool(true), "{payload}");
    assert_eq!(
        payload["content"].as_str(),
        Some("Tool result received. Task complete."),
        "{payload}"
    );
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}
