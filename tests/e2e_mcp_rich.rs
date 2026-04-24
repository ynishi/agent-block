//! E2E tests for MCP rich client (HTTP transport round-trip).
//!
//! Spins up a real `rmcp::StreamableHttpService` in-process on an ephemeral
//! port, then invokes the `agent-block` binary with a Lua fixture that calls
//! `mcp.connect_http` → `mcp.list_tools`.  This exercises the full Rust→Lua
//! bridge path without requiring any external network services.
//!
//! In-process unit/integration tests for resources/prompts/progress live in
//! `src/mcp_client/mod.rs::rich_tests` because the crate is binary-only
//! (no lib target) and those tests need direct access to `McpManager`
//! internals.

mod common;

use predicates::prelude::*;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::{MaybeSendFuture, RequestContext},
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, RoleServer, ServerHandler,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ── Minimal in-process MCP server ────────────────────────────────────────────

/// A trivial counter server that exposes a single `increment` tool.
///
/// Stateless — every `tools/call` returns `"1"` — so no shared state is
/// required, keeping the test simple and deterministic.
#[derive(Clone)]
struct CounterServer;

impl ServerHandler for CounterServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + MaybeSendFuture + '_
    {
        let tools = vec![Tool::new(
            "increment",
            "Increment the counter by one",
            Arc::new(serde_json::Map::new()),
        )];
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    async fn call_tool(
        &self,
        _params: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text("1")]))
    }
}

// ── Helper: spawn the in-process HTTP server ─────────────────────────────────

/// Bind an ephemeral TCP port, mount `CounterServer` behind a
/// `StreamableHttpService`, and start serving in a background task.
///
/// Returns the base URL (e.g. `"http://127.0.0.1:<port>/mcp"`) and a
/// `CancellationToken` that stops the server when cancelled.
///
/// Must be called from within a tokio async context.
async fn spawn_counter_http_server() -> (String, CancellationToken) {
    let ct = CancellationToken::new();

    // Stateless mode: each POST is self-contained — no session affinity is
    // needed for the single initialize → tools/list exchange in the fixture.
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_sse_keep_alive(None)
        .with_cancellation_token(ct.child_token());

    let service: StreamableHttpService<CounterServer, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(CounterServer), Default::default(), config);

    let router = axum::Router::new().nest_service("/mcp", service);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}/mcp"), ct)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Round-trip: `mcp.connect_http` → `mcp.cancel` against an in-process
/// `StreamableHttpService` (stateless mode).
///
/// Verifies that `mcp.cancel(server_name, request_id)` does not throw even
/// when the server does not recognise the (sentinel) request ID.
#[tokio::test]
async fn connect_http_then_cancel_roundtrip() {
    let (url, ct) = spawn_counter_http_server().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = url.clone();
    tokio::task::spawn_blocking(move || {
        common::agent_block_cmd()
            .args(["-s", &common::fixture("mcp_http_cancel.lua")])
            .env("MCP_HTTP_URL", &url_clone)
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("CONNECT_HTTP_OK"))
            .stdout(predicate::str::contains("CANCEL_OK"))
            .stdout(predicate::str::contains("FIXTURE_DONE"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    ct.cancel();
}

/// Round-trip: `mcp.connect_http` → `mcp.list_tools` against an in-process
/// `StreamableHttpService` (stateless / SSE mode).
///
/// The in-process HTTP server is started on an ephemeral port before the
/// `agent-block` binary is invoked.  The binary receives the server URL via
/// the `MCP_HTTP_URL` environment variable and exercises the full Lua bridge
/// path (`mcp.connect_http` → `mcp.list_tools`).
///
/// Asserts:
/// - The binary exits successfully (exit code 0).
/// - stdout contains `CONNECT_HTTP_OK` — the MCP initialize handshake
///   completed successfully over HTTP transport.
/// - stdout contains `LIST_TOOLS_OK` — `list_tools` returned at least one
///   tool and the `increment` tool was found in the response.
/// - stdout contains `FIXTURE_DONE` — the fixture ran to completion without
///   assertion errors.
#[tokio::test]
async fn connect_http_then_list_tools_roundtrip() {
    let (url, ct) = spawn_counter_http_server().await;

    // Give the axum listener a moment to fully enter the accept loop before
    // the subprocess sends its first request.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // `agent_block_cmd` / `assert` are blocking — run in a blocking thread so
    // we do not stall the tokio runtime while the subprocess executes.
    let url_clone = url.clone();
    tokio::task::spawn_blocking(move || {
        common::agent_block_cmd()
            .args(["-s", &common::fixture("mcp_http_tools.lua")])
            .env("MCP_HTTP_URL", &url_clone)
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("CONNECT_HTTP_OK"))
            .stdout(predicate::str::contains("LIST_TOOLS_OK"))
            .stdout(predicate::str::contains("FIXTURE_DONE"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    ct.cancel();
}
