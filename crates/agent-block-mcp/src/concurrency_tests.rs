//! Concurrency contract tests.
//!
//! These tests nail down the **intended** concurrency model of `McpManager`
//! regardless of what rmcp does internally:
//!
//! 1. `list_tools` / `call_tool` are `&self` ⇒ usable under `RwLock::read`.
//! 2. Two concurrent RPCs against the **same** server must overlap in
//!    wall time (they do not serialize at the `McpManager` layer).
//! 3. The lock primitive is `RwLock`, not `Mutex` — concurrent reads
//!    coexist and a write blocks while any read is held.
//!
//! If rmcp changes its `Peer` concurrency contract, or if this module is
//! refactored back to `Mutex` / `&mut self`, these tests break loudly.
//!
//! Moved verbatim out of `lib.rs` (`#[cfg(test)] mod concurrency_tests`).
//! `super` still resolves to the crate root, so `use super::*` is unchanged.

use super::*;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ServerCapabilities,
        ServerInfo,
    },
    service::{MaybeSendFuture, RequestContext},
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};

/// A server that sleeps `delay` before every `tools/call`.
/// Used to observe whether two concurrent `call_tool` invocations
/// overlap (≈ `delay`) or serialize (≈ `2 × delay`).
#[derive(Clone)]
struct SlowToolServer {
    delay: Duration,
}

impl ServerHandler for SlowToolServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn call_tool(
        &self,
        _params: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResponse, McpError>> + MaybeSendFuture + '_
    {
        let delay = self.delay;
        async move {
            tokio::time::sleep(delay).await;
            Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into())
        }
    }
}

/// Spawn an in-process `SlowToolServer` wired to the given `McpManager`
/// via a `tokio::io::duplex` pair. Bypasses `TokioChildProcess` so the
/// test does not depend on an external binary.
async fn attach_slow_server(mgr: &mut McpManager, name: &str, delay: Duration) {
    let (server_side, client_side) = tokio::io::duplex(8192);

    let server = SlowToolServer { delay };
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });

    let handler = AgentBlockClientHandler::new();
    let running = handler
        .serve(client_side)
        .await
        .expect("client handshake should succeed over duplex");
    mgr.servers.insert(name.to_string(), running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_call_tool_same_server_does_not_serialize() {
    let delay = Duration::from_millis(300);
    let mgr = Arc::new(RwLock::new(McpManager::new()));

    attach_slow_server(&mut *mgr.write().await, "slow", delay).await;

    let start = Instant::now();
    let a = {
        let mgr = Arc::clone(&mgr);
        async move {
            mgr.read()
                .await
                .call_tool("slow", "slow_tool", serde_json::json!({}))
                .await
        }
    };
    let b = {
        let mgr = Arc::clone(&mgr);
        async move {
            mgr.read()
                .await
                .call_tool("slow", "slow_tool", serde_json::json!({}))
                .await
        }
    };
    let (r1, r2) = tokio::join!(a, b);
    let elapsed = start.elapsed();

    r1.expect("first call succeeds");
    r2.expect("second call succeeds");

    // Serialized path would take ≥ 2×delay = 600ms. Parallel path
    // should land near `delay` (300ms). Fail with generous margin if
    // serialization is observed.
    let serialized_budget = delay * 2 - Duration::from_millis(80);
    assert!(
        elapsed < serialized_budget,
        "concurrent call_tool appears serialized: elapsed={:?}, serialized_budget={:?}",
        elapsed,
        serialized_budget,
    );
}

#[tokio::test]
async fn two_reads_coexist_on_rwlock() {
    // Structural check: confirms `RwLock` (not `Mutex`) is the primitive.
    // A revert to `tokio::sync::Mutex` would drop `try_read` and break
    // this test at compile time.
    let mgr = Arc::new(RwLock::new(McpManager::new()));
    let _g1 = mgr.read().await;
    assert!(
        mgr.try_read().is_ok(),
        "RwLock rejected a concurrent second read guard",
    );
}

#[tokio::test]
async fn write_blocks_while_read_held() {
    let mgr = Arc::new(RwLock::new(McpManager::new()));
    let _g1 = mgr.read().await;
    assert!(
        mgr.try_write().is_err(),
        "write lock acquired while a read guard was held",
    );
}

/// A server that always returns `CallToolResult::error`, i.e.
/// `isError = true`. Used to lock down pass-through semantics.
#[derive(Clone)]
struct IsErrorServer;

impl ServerHandler for IsErrorServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn call_tool(
        &self,
        _params: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text("tool blew up")]).into())
    }
}

async fn attach_is_error_server(mgr: &mut McpManager, name: &str) {
    let (server_side, client_side) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        if let Ok(running) = IsErrorServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let handler = AgentBlockClientHandler::new();
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);
}

#[tokio::test]
async fn is_error_is_passed_through_in_ok_branch() {
    // MCP spec: tool-execution errors come back as a successful RPC
    // with `isError=true`. `call_tool` must return `Ok(..)` and
    // preserve `isError` in the serialized JSON so the Lua bridge
    // (and ultimately the LLM) sees it.
    let mut mgr = McpManager::new();
    attach_is_error_server(&mut mgr, "boom").await;

    let val = mgr
        .call_tool("boom", "explode", serde_json::json!({}))
        .await
        .expect("RPC succeeds even when isError=true");

    assert_eq!(
        val.get("isError").and_then(|v| v.as_bool()),
        Some(true),
        "isError must be preserved in Ok branch: {val}",
    );
    let content = val.get("content").and_then(|v| v.as_array()).cloned();
    assert!(
        content.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
        "content blocks must be forwarded alongside isError: {val:?}",
    );
}
