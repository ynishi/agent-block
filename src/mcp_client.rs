//! MCP Client — manages MCP server child processes via rmcp.
//!
//! Uses `rmcp` (1.4.x) `RunningService<RoleClient, ()>` internally.
//! The `()` unit type provides the default `ClientHandler` implementation
//! which returns `method_not_found` for `create_message` (sampling not advertised).
//!
//! All rmcp round-trips are wrapped in a per-call timeout so a hung child
//! cannot block a Lua coroutine indefinitely.
//!
//! # Concurrency contract
//!
//! `list_tools` and `call_tool` take `&self`, so the manager can be held
//! under `tokio::sync::RwLock` and multiple RPCs — including against the
//! same server — can proceed in parallel via read guards. Request/response
//! multiplexing on a single server is handled by rmcp's `Peer`, which
//! pairs each outbound request with a `oneshot` receiver keyed by request
//! ID. `connect` and `disconnect` are mutating (`&mut self`) and must take
//! the write guard.
//!
//! This contract is covered by in-process unit tests in `#[cfg(test)]` at
//! the bottom of this file. If rmcp alters its `Peer` concurrency model,
//! or if this module is refactored to re-serialize RPCs, those tests fail.
//!
//! # Usage from Lua
//!
//! ```lua
//! mcp.connect("outline", "outline-mcp", {})
//! local tools = mcp.list_tools("outline")
//! local result = mcp.call("outline", "shelf", {})
//! mcp.disconnect("outline")
//! ```

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use rmcp::{
    model::CallToolRequestParams,
    service::{RoleClient, RunningService},
    transport::TokioChildProcess,
    ServiceExt,
};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

use crate::error::{BlockError, BlockResult};

/// Default RPC round-trip timeout when no explicit value is provided.
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

pub struct McpManager {
    servers: HashMap<String, RunningService<RoleClient, ()>>,
    rpc_timeout: Duration,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
        }
    }

    /// Construct a manager with a caller-specified RPC timeout.
    /// Applies to `connect`, `list_tools`, and `call_tool` alike.
    ///
    /// `rpc_timeout` must be non-zero. `Duration::ZERO` would cause every
    /// `tokio::time::timeout` to fire immediately, silently turning every
    /// MCP round-trip into a timeout error — for an autonomous agent that
    /// is a "everything looks broken" failure mode. We reject it at
    /// construction time so the misconfiguration surfaces loudly at
    /// startup instead of being swallowed at the first RPC.
    pub fn with_rpc_timeout(rpc_timeout: Duration) -> BlockResult<Self> {
        if rpc_timeout.is_zero() {
            return Err(BlockError::Mcp(
                "rpc_timeout must be > 0 (got Duration::ZERO); \
                 every MCP RPC would time out immediately"
                    .to_string(),
            ));
        }
        Ok(Self {
            servers: HashMap::new(),
            rpc_timeout,
        })
    }

    /// Spawn the MCP server process and complete the MCP initialize handshake.
    pub async fn connect(&mut self, name: &str, command: &str, args: &[String]) -> BlockResult<()> {
        let mut cmd = Command::new(command);
        cmd.args(args).stderr(Stdio::inherit());
        let transport = TokioChildProcess::new(cmd).map_err(|e| {
            warn!(server = %name, command = %command, error = %e, "mcp spawn failed");
            BlockError::Mcp(format!("spawn '{command}': {e}"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let running = timeout(rpc_timeout, ().serve(transport))
            .await
            .map_err(|_| {
                warn!(server = %name, timeout = ?rpc_timeout, "mcp initialize timed out");
                BlockError::Timeout(format!(
                    "initialize '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, error = %e, "mcp initialize failed");
                BlockError::Mcp(format!("initialize '{name}': {e}"))
            })?;
        self.servers.insert(name.to_string(), running);
        Ok(())
    }

    /// Call `tools/list` and return the tools as a JSON array.
    ///
    /// Immutable receiver so concurrent readers can share an `RwLock<McpManager>`.
    pub async fn list_tools(&self, name: &str) -> BlockResult<serde_json::Value> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, "mcp list_tools on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let tools = timeout(rpc_timeout, srv.list_all_tools())
            .await
            .map_err(|_| {
                warn!(server = %name, timeout = ?rpc_timeout, "mcp list_tools timed out");
                BlockError::Timeout(format!(
                    "list_tools '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, error = %e, "mcp list_tools failed");
                BlockError::Mcp(format!("list_tools '{name}': {e}"))
            })?;
        serde_json::to_value(&tools)
            .map_err(|e| BlockError::Mcp(format!("serialize list_tools result: {e}")))
    }

    /// Call `tools/call` with the given tool name and arguments.
    ///
    /// Returns the full rmcp `CallToolResult` serialized to JSON
    /// (`{"content": [...], "isError": bool, ...}`) on success, including
    /// the `isError` flag — tool-execution errors are passed through to
    /// the caller, following the MCP spec's intent that the LLM sees them
    /// and self-corrects. Only protocol / transport / timeout failures
    /// surface as `Err(BlockError::*)`.
    ///
    /// `arguments` must be a JSON `Object` or `Null`. `Null` is treated as
    /// "no arguments"; any other shape (array, scalar) returns an error
    /// rather than silently dropping the payload.
    /// Immutable receiver so concurrent readers can share an `RwLock<McpManager>`.
    pub async fn call_tool(
        &self,
        name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> BlockResult<serde_json::Value> {
        // Validate argument shape early so the error does not depend on
        // whether the server is registered or reachable. MCP spec requires
        // `arguments` to be an object (or absent); an array/scalar would
        // serialize into `CallToolRequestParams` as-is and the server
        // would reject it with an opaque protocol error.
        let mut params = CallToolRequestParams::new(tool_name.to_string());
        match arguments {
            serde_json::Value::Object(obj) => {
                params = params.with_arguments(obj);
            }
            serde_json::Value::Null => {}
            other => {
                let kind = match other {
                    serde_json::Value::Array(_) => "array",
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Bool(_) => "bool",
                    _ => "unknown",
                };
                return Err(BlockError::Mcp(format!(
                    "call_tool '{tool_name}' on '{name}': arguments must be a JSON object \
                     (got {kind})"
                )));
            }
        }
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, tool = %tool_name, "mcp call_tool on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let result = timeout(rpc_timeout, srv.call_tool(params))
            .await
            .map_err(|_| {
                warn!(server = %name, tool = %tool_name, timeout = ?rpc_timeout, "mcp call_tool timed out");
                BlockError::Timeout(format!(
                    "call_tool '{tool_name}' on '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, tool = %tool_name, error = %e, "mcp call_tool failed");
                BlockError::Mcp(format!("call_tool '{tool_name}' on '{name}': {e}"))
            })?;
        serde_json::to_value(&result)
            .map_err(|e| BlockError::Mcp(format!("serialize call_tool result: {e}")))
    }

    /// Cancel the named server and remove it from the manager.
    ///
    /// The server is removed from the internal map **before** the cancel
    /// round-trip begins, so a slow or failed cancel never leaves a
    /// zombie entry behind. If graceful cancel exceeds `rpc_timeout`,
    /// the service handle is dropped at the end of the match arm —
    /// rmcp's `Drop` impl cancels the peer's cancellation token, which
    /// terminates the internal task and closes the transport — and
    /// `BlockError::Timeout` is returned.
    ///
    /// The same `rpc_timeout` is reused here so callers have a single
    /// knob governing every MCP round-trip (see `with_rpc_timeout`).
    ///
    /// Callers may re-`connect` the same name safely after any outcome.
    pub async fn disconnect(&mut self, name: &str) -> BlockResult<()> {
        let Some(running) = self.servers.remove(name) else {
            return Ok(());
        };
        let cancel_timeout = self.rpc_timeout;
        match timeout(cancel_timeout, running.cancel()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                warn!(server = %name, error = %e, "mcp cancel failed");
                Err(BlockError::Mcp(format!("cancel '{name}': {e}")))
            }
            Err(_) => {
                warn!(server = %name, timeout = ?cancel_timeout, "mcp cancel timed out");
                Err(BlockError::Timeout(format!(
                    "cancel '{name}' timed out after {cancel_timeout:?}"
                )))
            }
        }
    }

    /// Cancel all managed servers.
    ///
    /// Every server is disconnected regardless of individual failures.
    /// The first error encountered is returned so shutdown can signal
    /// a problem; **subsequent** errors are logged at `warn` level so
    /// they are not silently discarded.
    pub async fn disconnect_all(&mut self) -> BlockResult<()> {
        let mut first_err: Option<BlockError> = None;
        let names: Vec<String> = self.servers.keys().cloned().collect();
        for name in names {
            if let Err(e) = self.disconnect(&name).await {
                if first_err.is_none() {
                    first_err = Some(e);
                } else {
                    warn!(server = %name, error = %e, "disconnect failed during disconnect_all");
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
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
}

/// Concurrency contract tests.
///
/// These tests nail down the **intended** concurrency model of `McpManager`
/// regardless of what rmcp does internally:
///
/// 1. `list_tools` / `call_tool` are `&self` ⇒ usable under `RwLock::read`.
/// 2. Two concurrent RPCs against the **same** server must overlap in
///    wall time (they do not serialize at the `McpManager` layer).
/// 3. The lock primitive is `RwLock`, not `Mutex` — concurrent reads
///    coexist and a write blocks while any read is held.
///
/// If rmcp changes its `Peer` concurrency contract, or if this module is
/// refactored back to `Mutex` / `&mut self`, these tests break loudly.
#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::RwLock;

    use rmcp::{
        model::{CallToolRequestParams, CallToolResult, Content, ServerCapabilities, ServerInfo},
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
        ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + MaybeSendFuture + '_
        {
            let delay = self.delay;
            async move {
                tokio::time::sleep(delay).await;
                Ok(CallToolResult::success(vec![Content::text("ok")]))
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

        let running =
            ().serve(client_side)
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
        ) -> Result<CallToolResult, McpError> {
            Ok(CallToolResult::error(vec![Content::text("tool blew up")]))
        }
    }

    async fn attach_is_error_server(mgr: &mut McpManager, name: &str) {
        let (server_side, client_side) = tokio::io::duplex(8192);
        tokio::spawn(async move {
            if let Ok(running) = IsErrorServer.serve(server_side).await {
                let _ = running.waiting().await;
            }
        });
        let running = ().serve(client_side).await.expect("handshake");
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
}
