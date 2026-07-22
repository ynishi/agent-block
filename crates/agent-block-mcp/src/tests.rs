//! Basic `McpManager` unit tests.
//!
//! Moved verbatim out of `lib.rs` (`#[cfg(test)] mod tests`) to keep the
//! module body focused on production code. `super` still resolves to the
//! crate root, so `use super::*` behaves identically to the inline form.

use super::*;

#[tokio::test]
async fn new_manager_is_empty() {
    let mgr = McpManager::new();
    assert!(mgr.servers.is_empty());
}

#[tokio::test]
async fn with_rpc_timeout_rejects_zero() {
    // A ZERO timeout would make every `tokio::time::timeout` fire
    // immediately, silently turning every RPC into a timeout error.
    // For an autonomous agent that is a catastrophic failure mode —
    // the misconfiguration must surface at construction, not be
    // swallowed at the first MCP call.
    let err = match McpManager::with_rpc_timeout(Duration::ZERO) {
        Ok(_) => panic!("Duration::ZERO must be rejected"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("rpc_timeout must be > 0"),
        "unexpected error: {err}",
    );
}

#[tokio::test]
async fn with_rpc_timeout_accepts_positive() {
    let mgr = match McpManager::with_rpc_timeout(Duration::from_millis(1)) {
        Ok(m) => m,
        Err(e) => panic!("positive timeout must be accepted: {e}"),
    };
    assert!(mgr.servers.is_empty());
}

#[tokio::test]
async fn disconnect_nonexistent_is_ok() {
    let mut mgr = McpManager::new();
    assert!(mgr.disconnect("ghost").await.is_ok());
}

#[tokio::test]
async fn call_unknown_server_returns_error() {
    // `let mgr =` (not `let mut`) also asserts at compile time that
    // `call_tool` takes `&self`. Reverting to `&mut self` would break
    // this call site.
    let mgr = McpManager::new();
    let res = mgr.call_tool("none", "dummy", serde_json::json!({})).await;
    assert!(res.is_err());
}

#[tokio::test]
async fn list_tools_takes_shared_receiver() {
    // Mirror guard for `list_tools(&self)`.
    let mgr = McpManager::new();
    let res = mgr.list_tools("none").await;
    assert!(res.is_err());
}

#[tokio::test]
async fn disconnect_all_empties_map() {
    let mut mgr = McpManager::new();
    mgr.disconnect_all()
        .await
        .expect("disconnect_all on empty manager should succeed");
    assert!(mgr.servers.is_empty());
}

#[tokio::test]
async fn call_tool_rejects_non_object_arguments() {
    // Argument validation runs before the server lookup, so an
    // array/scalar is rejected even without a live server.
    let mgr = McpManager::new();
    for bad in [
        serde_json::json!([1, 2, 3]),
        serde_json::json!("string"),
        serde_json::json!(42),
        serde_json::json!(true),
    ] {
        let res = mgr.call_tool("anything", "dummy", bad.clone()).await;
        let err = res.expect_err("non-object args must error");
        let msg = err.to_string();
        assert!(
            msg.contains("arguments must be a JSON object"),
            "unexpected error for {bad}: {msg}",
        );
    }
}

#[tokio::test]
async fn call_tool_accepts_null_arguments_as_absent() {
    // Null is the documented "no arguments" form. It must pass the
    // validation gate (and fail at the server-lookup step instead).
    let mgr = McpManager::new();
    let res = mgr
        .call_tool("ghost", "dummy", serde_json::Value::Null)
        .await;
    let err = res.expect_err("expected no-server error, not arg-shape error");
    assert!(
        err.to_string().contains("no server named"),
        "Null args should reach the lookup step: {err}",
    );
}
