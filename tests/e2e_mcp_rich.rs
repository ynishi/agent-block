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
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, LoggingLevel,
        LoggingMessageNotificationParam, NumberOrString, PaginatedRequestParams,
        ProgressNotificationParam, ProgressToken, ServerCapabilities, ServerInfo, Tool,
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

// ── on_progress / on_log notification servers ─────────────────────────────────

/// A server that sends a progress notification during `call_tool("emit_progress")`,
/// then returns a success result. Used to test the on_progress callback path.
#[derive(Clone)]
struct ProgressTestServer;

impl ServerHandler for ProgressTestServer {
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
            "emit_progress",
            "Emit a progress notification then return ok",
            Arc::new(serde_json::Map::new()),
        )];
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    async fn call_tool(
        &self,
        _params: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Extract the progressToken that the client auto-attached in _meta.
        // In rmcp 1.4.0, _meta is deserialized into ctx.meta (not params.meta).
        // rmcp's Peer::send_request assigns a counter-based token via
        // AtomicU32ProgressTokenProvider; the fallback "tok-e2e" is never used
        // in practice but keeps the test server robust.
        let token = ctx
            .meta
            .get_progress_token()
            .map(|t| t.0.clone())
            .unwrap_or(NumberOrString::String("tok-e2e".into()));
        // Push a progress notification echoing the token back to the client.
        let _ = ctx
            .peer
            .notify_progress(ProgressNotificationParam {
                progress_token: ProgressToken(token),
                progress: 1.0,
                total: Some(1.0),
                message: Some("done".into()),
            })
            .await;
        Ok(CallToolResult::success(vec![Content::text(
            "progress_sent",
        )]))
    }
}

/// A server with logging capability that emits a log notification during
/// `call_tool("emit_log")`. Used to test the on_log callback path.
#[derive(Clone)]
struct LoggingTestServer;

impl ServerHandler for LoggingTestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_logging()
                .build(),
        )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + MaybeSendFuture + '_
    {
        let tools = vec![Tool::new(
            "emit_log",
            "Emit a log notification then return ok",
            Arc::new(serde_json::Map::new()),
        )];
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    async fn call_tool(
        &self,
        _params: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Push a log notification before returning the result.
        let _ = ctx
            .peer
            .notify_logging_message(LoggingMessageNotificationParam {
                level: LoggingLevel::Info,
                logger: Some("test-logger".into()),
                data: serde_json::json!("e2e log message"),
            })
            .await;
        Ok(CallToolResult::success(vec![Content::text("log_sent")]))
    }
}

/// Spawn a `ProgressTestServer` behind a stateful `StreamableHttpService`.
///
/// Returns the base URL and a `CancellationToken` that stops the server.
async fn spawn_progress_http_server() -> (String, CancellationToken) {
    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(None)
        .with_cancellation_token(ct.child_token());

    let service: StreamableHttpService<ProgressTestServer, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(ProgressTestServer), Default::default(), config);

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

/// Spawn a `LoggingTestServer` behind a stateful `StreamableHttpService`.
///
/// Returns the base URL and a `CancellationToken` that stops the server.
async fn spawn_logging_http_server() -> (String, CancellationToken) {
    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(None)
        .with_cancellation_token(ct.child_token());

    let service: StreamableHttpService<LoggingTestServer, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(LoggingTestServer), Default::default(), config);

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

// ── Nil-field pattern servers ────────────────────────────────────────���─────────

/// A server that sends a progress notification with `total: None` and
/// `message: None` during `call_tool("emit_progress_nil")`.
///
/// Used to verify that the glue nil-guards normalise missing optional fields
/// (`total → 0.0`, `message → ""`) so that `on_progress` callbacks do not
/// crash with nil-concat errors.
#[derive(Clone)]
struct NilPatternProgressServer;

impl ServerHandler for NilPatternProgressServer {
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
            "emit_progress_nil",
            "Emit a progress notification with total=None and message=None",
            Arc::new(serde_json::Map::new()),
        )];
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    async fn call_tool(
        &self,
        _params: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let token = ctx
            .meta
            .get_progress_token()
            .map(|t| t.0.clone())
            .unwrap_or(NumberOrString::String("tok-nil".into()));
        let _ = ctx
            .peer
            .notify_progress(ProgressNotificationParam {
                progress_token: ProgressToken(token),
                progress: 1.0,
                total: None,   // triggers total=0.0 normalisation in glue
                message: None, // triggers message="" normalisation in glue
            })
            .await;
        Ok(CallToolResult::success(vec![Content::text(
            "nil_progress_sent",
        )]))
    }
}

/// A server that sends a log notification with `logger: None` and
/// `data: Value::Null` during `call_tool("emit_log_nil")`.
///
/// Used to verify that the glue nil-guards normalise missing optional fields
/// so that `on_log` callbacks do not crash with nil-concat errors.
#[derive(Clone)]
struct NilPatternLoggingServer;

impl ServerHandler for NilPatternLoggingServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_logging()
                .build(),
        )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + MaybeSendFuture + '_
    {
        let tools = vec![Tool::new(
            "emit_log_nil",
            "Emit a log notification with logger=None and data=Null",
            Arc::new(serde_json::Map::new()),
        )];
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    async fn call_tool(
        &self,
        _params: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let _ = ctx
            .peer
            .notify_logging_message(LoggingMessageNotificationParam {
                level: LoggingLevel::Info,
                logger: None,                  // triggers logger="" normalisation
                data: serde_json::Value::Null, // serialises to "null" string
            })
            .await;
        Ok(CallToolResult::success(vec![Content::text("nil_log_sent")]))
    }
}

/// Spawn a `NilPatternProgressServer` behind a stateful `StreamableHttpService`.
async fn spawn_nil_progress_http_server() -> (String, CancellationToken) {
    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(None)
        .with_cancellation_token(ct.child_token());

    let service: StreamableHttpService<NilPatternProgressServer, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(NilPatternProgressServer), Default::default(), config);

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

/// Spawn a `NilPatternLoggingServer` behind a stateful `StreamableHttpService`.
async fn spawn_nil_logging_http_server() -> (String, CancellationToken) {
    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(None)
        .with_cancellation_token(ct.child_token());

    let service: StreamableHttpService<NilPatternLoggingServer, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(NilPatternLoggingServer), Default::default(), config);

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

// ── Tests: on_progress / on_log / capability gate ─────────────────────────────

/// Case (a): `mcp.on_progress` callback is registered and the envelope is
/// dispatched when the server sends a progress notification during `call_tool`.
///
/// The fixture (`mcp_on_progress_envelope.lua`) registers an `on_progress`
/// handler with the same wrapper pattern that `connect_mcp_servers` uses for
/// `opts.on_progress`, calls `mcp.call("prog", "emit_progress", {})`, and
/// asserts that the callback received an event with `type="progress"`.
#[tokio::test]
async fn on_progress_callback_receives_envelope() {
    let (url, ct) = spawn_progress_http_server().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = url.clone();
    tokio::task::spawn_blocking(move || {
        common::agent_block_cmd()
            .args(["-s", &common::fixture("mcp_on_progress_envelope.lua")])
            .env("MCP_HTTP_URL", &url_clone)
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("CONNECT_HTTP_OK"))
            .stdout(predicate::str::contains("CALL_OK"))
            .stdout(predicate::str::contains("PROGRESS_EV_OK"))
            .stdout(predicate::str::contains("FIXTURE_DONE"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    ct.cancel();
}

/// Case (b): `mcp.on_log` callback is registered and the envelope is dispatched
/// when a server with logging capability sends a log notification.
///
/// The fixture (`mcp_on_log_callback.lua`) registers an `on_log` handler,
/// calls `mcp.call("logserver", "emit_log", {})`, and asserts that the callback
/// received an event with `type="log"`.
#[tokio::test]
async fn on_log_callback_receives_envelope() {
    let (url, ct) = spawn_logging_http_server().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = url.clone();
    tokio::task::spawn_blocking(move || {
        common::agent_block_cmd()
            .args(["-s", &common::fixture("mcp_on_log_callback.lua")])
            .env("MCP_HTTP_URL", &url_clone)
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("CONNECT_HTTP_OK"))
            .stdout(predicate::str::contains("CALL_OK"))
            .stdout(predicate::str::contains("LOG_EV_OK"))
            .stdout(predicate::str::contains("FIXTURE_DONE"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    ct.cancel();
}

/// Case (c): When a server has no logging capability, the `connect_mcp_servers`
/// gate skips `mcp.on_log` registration and the callback is never fired.
///
/// The fixture (`mcp_log_capability_skip.lua`) connects to the `CounterServer`
/// (which declares only `tools`, not `logging`), verifies via `server_info`
/// that `capabilities.logging` is absent, and asserts that the on_log callback
/// is never invoked.
#[tokio::test]
async fn log_capability_absent_skips_on_log_registration() {
    // CounterServer has no logging capability — reuse the existing helper.
    let (url, ct) = spawn_counter_http_server().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = url.clone();
    tokio::task::spawn_blocking(move || {
        common::agent_block_cmd()
            .args(["-s", &common::fixture("mcp_log_capability_skip.lua")])
            .env("MCP_HTTP_URL", &url_clone)
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("CONNECT_HTTP_OK"))
            .stdout(predicate::str::contains("SKIP_OK"))
            .stdout(predicate::str::contains("FIXTURE_DONE"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    ct.cancel();
}

// ── Nil-field regression tests ────────────────────────────────────────────────

/// Nil-field regression (progress): the `on_progress` callback must fire and
/// must NOT emit `"progress handler failed"` when the server sends a progress
/// notification with `total=None` and `message=None`.
///
/// The `NilPatternProgressServer` sends exactly this payload.  The fixture
/// asserts that the callback receives non-nil `total` (normalised to `0`) and
/// non-nil `message` (normalised to `""`), confirming the glue nil-guards in
/// `__mcp_dispatch_progress` (handler.rs) are active.
#[tokio::test]
async fn on_progress_nil_fields_do_not_crash_callback() {
    let (url, ct) = spawn_nil_progress_http_server().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = url.clone();
    tokio::task::spawn_blocking(move || {
        common::agent_block_cmd()
            .args(["-s", &common::fixture("mcp_on_progress_nil_fields.lua")])
            .env("MCP_HTTP_URL", &url_clone)
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("CONNECT_HTTP_OK"))
            .stdout(predicate::str::contains("CALL_OK"))
            .stdout(predicate::str::contains("PROGRESS_EV_OK"))
            // Must NOT see the handler-failed warning that would appear if the
            // callback crashed with a nil-concat error.
            .stdout(predicate::str::contains("progress handler failed").not())
            .stdout(predicate::str::contains("FIXTURE_DONE"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    ct.cancel();
}

/// Nil-field regression (log): the `on_log` callback must fire and must NOT
/// emit `"log handler dispatch failed"` when the server sends a log
/// notification with `logger=None` and `data=Value::Null`.
///
/// The `NilPatternLoggingServer` sends exactly this payload.  The fixture
/// asserts that the callback receives non-nil `logger` (normalised to `""`)
/// and non-nil `data_json` (serialised to the JSON literal `"null"`).
#[tokio::test]
async fn on_log_nil_fields_do_not_crash_callback() {
    let (url, ct) = spawn_nil_logging_http_server().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = url.clone();
    tokio::task::spawn_blocking(move || {
        common::agent_block_cmd()
            .args(["-s", &common::fixture("mcp_on_log_nil_fields.lua")])
            .env("MCP_HTTP_URL", &url_clone)
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stdout(predicate::str::contains("CONNECT_HTTP_OK"))
            .stdout(predicate::str::contains("CALL_OK"))
            .stdout(predicate::str::contains("LOG_EV_OK"))
            // Must NOT see the handler-failed warning.
            .stdout(predicate::str::contains("log handler dispatch failed").not())
            .stdout(predicate::str::contains("FIXTURE_DONE"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    ct.cancel();
}
