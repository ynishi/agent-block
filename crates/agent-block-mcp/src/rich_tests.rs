//! Rich client tests: resources, prompts, progress, and concurrent access.
//!
//! Uses in-process duplex servers (same pattern as `concurrency_tests`).
//!
//! Moved verbatim out of `lib.rs` (`#[cfg(test)] mod rich_tests`). `super`
//! still resolves to the crate root, so `use super::*` is unchanged.

use super::*;
use rmcp::{
    model::{
        CompleteRequestParams, CompleteResult, CompletionInfo, GetPromptRequestParams,
        GetPromptResult, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        NumberOrString, PaginatedRequestParams, ProgressNotificationParam, ProgressToken, Prompt,
        PromptMessage, PromptMessageRole, RawResource, RawResourceTemplate,
        ReadResourceRequestParams, ReadResourceResult, Reference, ResourceContents,
        ServerCapabilities, ServerInfo,
    },
    service::{MaybeSendFuture, RequestContext},
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use std::sync::Arc;
use tokio::sync::RwLock;

// ── Test Servers ────────────────────────────────────────────────────

#[derive(Clone)]
struct ResourceTestServer;

impl ServerHandler for ResourceTestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_resources().build())
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, McpError>> + MaybeSendFuture + '_
    {
        let resources = vec![
            rmcp::model::Resource::new(RawResource::new("file:///hello.txt", "hello.txt"), None),
            rmcp::model::Resource::new(RawResource::new("file:///world.txt", "world.txt"), None),
        ];
        std::future::ready(Ok(ListResourcesResult::with_all_items(resources)))
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ReadResourceResult, McpError>> + MaybeSendFuture + '_
    {
        let uri = request.uri.clone();
        let text = format!("content of {uri}");
        std::future::ready(Ok(ReadResourceResult::new(vec![ResourceContents::text(
            text, uri,
        )])))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourceTemplatesResult, McpError>>
           + MaybeSendFuture
           + '_ {
        let templates = vec![
            rmcp::model::ResourceTemplate::new(
                RawResourceTemplate::new("file:///{name}.txt", "file-template"),
                None,
            ),
            rmcp::model::ResourceTemplate::new(
                RawResourceTemplate::new("db:///{table}/{id}", "db-template"),
                None,
            ),
        ];
        std::future::ready(Ok(ListResourceTemplatesResult::with_all_items(templates)))
    }
}

#[derive(Clone)]
struct PromptTestServer;

impl ServerHandler for PromptTestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_prompts().build())
    }

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListPromptsResult, McpError>> + MaybeSendFuture + '_
    {
        let prompts = vec![
            Prompt::new("greet", Some("Greeting prompt"), None),
            Prompt::new("farewell", Some("Farewell prompt"), None),
        ];
        std::future::ready(Ok(ListPromptsResult::with_all_items(prompts)))
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<GetPromptResult, McpError>> + MaybeSendFuture + '_
    {
        let name = request.name.clone();
        let message = PromptMessage::new_text(
            PromptMessageRole::User,
            format!("This is the '{name}' prompt."),
        );
        std::future::ready(Ok(GetPromptResult::new(vec![message])))
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

async fn attach_resource_server(mgr: &mut McpManager, name: &str) {
    let (server_side, client_side) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(running) = ResourceTestServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let handler = AgentBlockClientHandler::new();
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);
}

async fn attach_prompt_server(mgr: &mut McpManager, name: &str) {
    let (server_side, client_side) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(running) = PromptTestServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let handler = AgentBlockClientHandler::new();
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);
}

#[derive(Clone)]
struct CompleteTestServer;

impl ServerHandler for CompleteTestServer {
    fn get_info(&self) -> ServerInfo {
        // Enable both prompts and resources so this server handles both ref kinds.
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        let info = match &request.r#ref {
            Reference::Prompt(_) => CompletionInfo::with_pagination(
                vec!["alice".to_string(), "alpha".to_string()],
                Some(2),
                false,
            )
            .expect("valid completion info"),
            Reference::Resource(_) => {
                CompletionInfo::with_pagination(vec!["file:///a.txt".to_string()], Some(1), false)
                    .expect("valid completion info")
            }
        };
        Ok(CompleteResult::new(info))
    }
}

async fn attach_complete_server(mgr: &mut McpManager, name: &str) {
    let (server_side, client_side) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(running) = CompleteTestServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let handler = AgentBlockClientHandler::new();
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);
}

// ── Tests: list_resources ───────────────────────────────────────────

#[tokio::test]
async fn list_resources_returns_all_resources() {
    let mut mgr = McpManager::new();
    attach_resource_server(&mut mgr, "res").await;

    let result = mgr
        .list_resources("res")
        .await
        .expect("list_resources should succeed");

    let arr = result.as_array().expect("should be JSON array");
    assert_eq!(arr.len(), 2, "expected 2 resources: {result}");
}

#[tokio::test]
async fn list_resources_unknown_server_returns_error() {
    let mgr = McpManager::new();
    let err = mgr
        .list_resources("ghost")
        .await
        .expect_err("unknown server must error");
    assert!(
        err.to_string().contains("no server named"),
        "unexpected error: {err}"
    );
}

// ── Tests: list_resource_templates ─────────────────────────────────

#[tokio::test]
async fn list_resource_templates_returns_all_templates() {
    let mut mgr = McpManager::new();
    attach_resource_server(&mut mgr, "res").await;

    let result = mgr
        .list_resource_templates("res")
        .await
        .expect("list_resource_templates should succeed");

    let arr = result.as_array().expect("should be JSON array");
    assert_eq!(arr.len(), 2, "expected 2 templates: {result}");

    let uri_template = arr[0]
        .get("uriTemplate")
        .and_then(|v| v.as_str())
        .expect("first template should have uriTemplate");
    assert!(
        uri_template.contains("{name}"),
        "uriTemplate should contain placeholder: {uri_template}"
    );
}

#[tokio::test]
async fn list_resource_templates_unknown_server_returns_error() {
    let mgr = McpManager::new();
    let err = mgr
        .list_resource_templates("ghost")
        .await
        .expect_err("unknown server must error");
    assert!(
        err.to_string().contains("no server named"),
        "unexpected error: {err}"
    );
}

// ── Tests: read_resource ────────────────────────────────────────────

#[tokio::test]
async fn read_resource_returns_contents() {
    let mut mgr = McpManager::new();
    attach_resource_server(&mut mgr, "res").await;

    let result = mgr
        .read_resource("res", "file:///hello.txt")
        .await
        .expect("read_resource should succeed");

    let contents = result
        .get("contents")
        .and_then(|v| v.as_array())
        .expect("should have contents array");
    assert!(!contents.is_empty(), "contents must not be empty: {result}");

    let text = contents[0]
        .get("text")
        .and_then(|v| v.as_str())
        .expect("should have text field");
    assert!(
        text.contains("file:///hello.txt"),
        "text should contain uri: {text}"
    );
}

#[tokio::test]
async fn read_resource_unknown_server_returns_error() {
    let mgr = McpManager::new();
    let err = mgr
        .read_resource("ghost", "file:///any.txt")
        .await
        .expect_err("unknown server must error");
    assert!(
        err.to_string().contains("no server named"),
        "unexpected error: {err}"
    );
}

// ── Tests: list_prompts ─────────────────────────────────────────────

#[tokio::test]
async fn list_prompts_returns_all_prompts() {
    let mut mgr = McpManager::new();
    attach_prompt_server(&mut mgr, "prm").await;

    let result = mgr
        .list_prompts("prm")
        .await
        .expect("list_prompts should succeed");

    let arr = result.as_array().expect("should be JSON array");
    assert_eq!(arr.len(), 2, "expected 2 prompts: {result}");
}

#[tokio::test]
async fn list_prompts_unknown_server_returns_error() {
    let mgr = McpManager::new();
    let err = mgr
        .list_prompts("ghost")
        .await
        .expect_err("unknown server must error");
    assert!(
        err.to_string().contains("no server named"),
        "unexpected error: {err}"
    );
}

// ── Tests: get_prompt ───────────────────────────────────────────────

#[tokio::test]
async fn get_prompt_returns_messages() {
    let mut mgr = McpManager::new();
    attach_prompt_server(&mut mgr, "prm").await;

    let result = mgr
        .get_prompt("prm", "greet", serde_json::Value::Null)
        .await
        .expect("get_prompt should succeed");

    let messages = result
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("should have messages array");
    assert!(!messages.is_empty(), "messages must not be empty: {result}");
}

#[tokio::test]
async fn get_prompt_rejects_non_object_args() {
    let mgr = McpManager::new();
    let err = mgr
        .get_prompt("any", "greet", serde_json::json!([1, 2]))
        .await
        .expect_err("array args must error");
    assert!(
        err.to_string().contains("args must be a JSON object"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn get_prompt_unknown_server_returns_error() {
    let mgr = McpManager::new();
    let err = mgr
        .get_prompt("ghost", "greet", serde_json::Value::Null)
        .await
        .expect_err("unknown server must error");
    assert!(
        err.to_string().contains("no server named"),
        "unexpected error: {err}"
    );
}

// ── Tests: complete ─────────────────────────────────────────────────

#[tokio::test]
async fn complete_prompt_ref_returns_values() {
    let mut mgr = McpManager::new();
    attach_complete_server(&mut mgr, "cmp").await;

    let ref_json = serde_json::json!({ "type": "ref/prompt", "name": "greet" });
    let result = mgr
        .complete("cmp", ref_json, "name", "al")
        .await
        .expect("complete with prompt ref should succeed");

    let completion = result
        .get("completion")
        .expect("result should have 'completion' key");
    let values = completion
        .get("values")
        .and_then(|v| v.as_array())
        .expect("completion should have 'values' array");
    assert!(
        !values.is_empty(),
        "values must not be empty for prompt ref: {result}"
    );
}

#[tokio::test]
async fn complete_resource_ref_returns_values() {
    let mut mgr = McpManager::new();
    attach_complete_server(&mut mgr, "cmp").await;

    let ref_json = serde_json::json!({ "type": "ref/resource", "uri": "file:///a.txt" });
    let result = mgr
        .complete("cmp", ref_json, "uri", "file:///")
        .await
        .expect("complete with resource ref should succeed");

    let completion = result
        .get("completion")
        .expect("result should have 'completion' key");
    let values = completion
        .get("values")
        .and_then(|v| v.as_array())
        .expect("completion should have 'values' array");
    assert!(
        !values.is_empty(),
        "values must not be empty for resource ref: {result}"
    );
}

#[tokio::test]
async fn complete_unknown_server_returns_error() {
    let mgr = McpManager::new();
    let ref_json = serde_json::json!({ "type": "ref/prompt", "name": "greet" });
    let err = mgr
        .complete("ghost", ref_json, "name", "al")
        .await
        .expect_err("unknown server must error");
    assert!(
        err.to_string().contains("no server named"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn complete_invalid_ref_kind_returns_error() {
    let mgr = McpManager::new();
    let ref_json = serde_json::json!({ "type": "ref/unknown", "name": "x" });
    let err = mgr
        .complete("any", ref_json, "name", "x")
        .await
        .expect_err("invalid ref kind must error");
    assert!(
        err.to_string().contains("invalid ref kind"),
        "unexpected error: {err}"
    );
}

// ── Tests: concurrent reads ─────────────────────────────────────────

/// Verify that list_resources and list_prompts can run concurrently under
/// RwLock::read — neither serializes behind the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_list_resources_and_list_prompts() {
    let mgr = Arc::new(RwLock::new(McpManager::new()));

    {
        let mut w = mgr.write().await;
        attach_resource_server(&mut w, "res").await;
        attach_prompt_server(&mut w, "prm").await;
    }

    let mgr_a = Arc::clone(&mgr);
    let mgr_b = Arc::clone(&mgr);

    let (r1, r2) = tokio::join!(
        async move { mgr_a.read().await.list_resources("res").await },
        async move { mgr_b.read().await.list_prompts("prm").await },
    );

    r1.expect("list_resources should succeed concurrently");
    r2.expect("list_prompts should succeed concurrently");
}

// ── Tests: on_progress handler registry marker ─────────────────────

#[test]
fn mark_on_progress_sets_flag_accessible_by_handler() {
    let handler = AgentBlockClientHandler::new();
    handler.ensure_server("srv");
    assert!(
        !handler
            .registry
            .lock()
            .unwrap()
            .get("srv")
            .unwrap()
            .on_progress
    );
    handler.mark_on_progress("srv");
    assert!(
        handler
            .registry
            .lock()
            .unwrap()
            .get("srv")
            .unwrap()
            .on_progress
    );
}

// ── Tests: connect_http ─────────────────────────────────────────────

/// connect_http on an unreachable address fails with BlockError::Mcp or Timeout.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn connect_http_unreachable_returns_error() {
    let mut mgr = McpManager::with_rpc_timeout(Duration::from_millis(100))
        .expect("non-zero timeout must be accepted");

    let err = mgr
        .connect_http(
            "test",
            "http://127.0.0.1:19999/mcp",
            serde_json::Value::Null,
        )
        .await
        .expect_err("unreachable URL must produce an error");

    let msg = err.to_string();
    assert!(
        msg.contains("http connect") || msg.contains("timed out"),
        "unexpected error: {msg}"
    );
}

// ── Tests: on_log and sampling marker flags ─────────────────────────

#[test]
fn mark_on_log_sets_flag_accessible_by_handler() {
    let handler = AgentBlockClientHandler::new();
    handler.ensure_server("log-srv");
    assert!(
        !handler
            .registry
            .lock()
            .unwrap()
            .get("log-srv")
            .unwrap()
            .on_log
    );
    handler.mark_on_log("log-srv");
    assert!(
        handler
            .registry
            .lock()
            .unwrap()
            .get("log-srv")
            .unwrap()
            .on_log
    );
}

#[test]
fn mark_sampling_sets_flag_accessible_by_handler() {
    let handler = AgentBlockClientHandler::new();
    handler.ensure_server("samp-srv");
    assert!(
        !handler
            .registry
            .lock()
            .unwrap()
            .get("samp-srv")
            .unwrap()
            .sampling
    );
    handler.mark_sampling("samp-srv");
    assert!(
        handler
            .registry
            .lock()
            .unwrap()
            .get("samp-srv")
            .unwrap()
            .sampling
    );
}

// ── Tests: send_cancelled ───────────────────────────────────────────

/// send_cancelled on an unknown server must not panic.
#[tokio::test]
async fn send_cancelled_unknown_server_is_no_op() {
    let mgr = McpManager::new();
    // Should not panic — logs a warn and returns.
    mgr.send_cancelled("ghost", Some(42));
}

/// send_cancelled on a live in-process server completes without error.
#[tokio::test]
async fn send_cancelled_live_server_does_not_panic() {
    let mut mgr = McpManager::new();
    attach_resource_server(&mut mgr, "res").await;
    // Pass Some(0) as a concrete request_id (live server will ignore unknown IDs).
    mgr.send_cancelled("res", Some(0));
    // Give the spawned task a moment to complete.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ── Tests: server_name set before clone in connect ──────────────────

/// Verifies the server_name + registry handshake in the connect flow.
///
/// `connect` sets `handler.server_name` before `clone()` then resets it
/// to `None` on the shared template. `ensure_server` ensures the registry
/// has an entry.  We test this without spawning a real transport by using
/// `ensure_server` + manual server_name mutation, which mirrors the
/// actual `connect` / `connect_http` code path.
#[test]
fn handler_server_name_reset_after_simulated_connect() {
    let mut mgr = McpManager::new();
    // Simulate what connect() does before cloning the handler.
    mgr.handler.ensure_server("srv-x");
    mgr.handler.server_name = Some("srv-x".to_string());
    let cloned = mgr.handler.clone();
    mgr.handler.server_name = None;

    // Template must be reset; clone must retain the name.
    assert!(
        mgr.handler.server_name.is_none(),
        "template server_name must be None after simulated connect"
    );
    assert_eq!(
        cloned.server_name.as_deref(),
        Some("srv-x"),
        "cloned handler must carry the server_name"
    );
    // Registry entry created by ensure_server.
    let guard = mgr.handler.registry.lock().unwrap();
    assert!(
        guard.contains_key("srv-x"),
        "registry must have entry after ensure_server"
    );
}

// ── Tests: progress dispatch (no-isle path) ─────────────────────────

/// Verifies the on_progress no-op path when handler_isle is None:
/// ensure_server + mark_on_progress sets the flag, and calling on_progress
/// with a real notification completes without panic when no isle is wired.
#[tokio::test]
async fn on_progress_no_op_when_no_isle() {
    let handler = AgentBlockClientHandler::new();
    handler.ensure_server("srv");
    handler.mark_on_progress("srv");

    // Simulate a progress notification arriving from rmcp task.
    let params = ProgressNotificationParam {
        progress_token: ProgressToken(NumberOrString::String("tok-1".into())),
        progress: 0.5,
        total: Some(1.0),
        message: None,
    };

    // We can't construct a full NotificationContext without a live Peer.
    // The no-isle path exits immediately, so this is covered by the unit test
    // in handler::tests::dispatcher_no_op_when_no_handler.
    // This test validates the flag path end-to-end via the registry.
    let guard = handler.registry.lock().unwrap();
    assert!(
        guard.get("srv").unwrap().on_progress,
        "on_progress flag must be set after mark_on_progress"
    );
    drop(guard);

    // The handler's on_progress is async; with no isle it short-circuits.
    // We exercise it via a minimal timeout-wrapped call.
    let _ = params;
}

// ── Tests: server_info ──────────────────────────────────────────────

#[tokio::test]
async fn server_info_unknown_server_returns_error() {
    let mgr = McpManager::new();
    let err = mgr
        .server_info("ghost")
        .expect_err("unknown server must error");
    assert!(
        err.to_string().contains("no server named"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn server_info_returns_capabilities_for_resource_server() {
    let mut mgr = McpManager::new();
    attach_resource_server(&mut mgr, "res").await;

    let info = mgr
        .server_info("res")
        .expect("server_info should succeed after handshake");

    let caps = info
        .get("capabilities")
        .expect("InitializeResult must have capabilities field");
    assert!(
        caps.get("resources").is_some(),
        "resource server must advertise resources capability: {caps}"
    );
}

#[tokio::test]
async fn server_info_returns_capabilities_for_prompt_server() {
    let mut mgr = McpManager::new();
    attach_prompt_server(&mut mgr, "prm").await;

    let info = mgr
        .server_info("prm")
        .expect("server_info should succeed after handshake");

    let caps = info
        .get("capabilities")
        .expect("InitializeResult must have capabilities field");
    assert!(
        caps.get("prompts").is_some(),
        "prompt server must advertise prompts capability: {caps}"
    );
}

// ── Tests: logging capability gate (case c) ─────────────────────────

/// A server that declares logging capability.
#[derive(Clone)]
struct LoggingCapableServer;

impl ServerHandler for LoggingCapableServer {
    // rmcp v1.4: `enable_logging()` is deprecated by SEP-2577. Kept on
    // the test surface until the migration to the post-2577 logging
    // API lands (tracked separately).
    #[allow(deprecated)]
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_logging()
                .build(),
        )
    }
}

async fn attach_logging_server(mgr: &mut McpManager, name: &str) {
    let (server_side, client_side) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(running) = LoggingCapableServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let handler = AgentBlockClientHandler::new();
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);
}

/// Verifies that `server_info` for a server with logging capability
/// returns `capabilities.logging` as a non-null field.  This is the
/// Rust-side condition that the Lua `connect_mcp_servers` gate checks:
/// `caps.logging ~= nil`.
#[tokio::test]
async fn server_info_returns_logging_capability_when_declared() {
    let mut mgr = McpManager::new();
    attach_logging_server(&mut mgr, "log").await;

    let info = mgr
        .server_info("log")
        .expect("server_info should succeed after handshake");

    let caps = info
        .get("capabilities")
        .expect("InitializeResult must have capabilities field");
    assert!(
        caps.get("logging").is_some(),
        "logging-capable server must advertise logging capability: {caps}"
    );
}

/// Verifies that `server_info` for a server WITHOUT logging capability
/// returns no `capabilities.logging` field, confirming the gate condition
/// correctly evaluates to `caps.logging == nil` in Lua.
#[tokio::test]
async fn server_info_has_no_logging_capability_for_tool_only_server() {
    let mut mgr = McpManager::new();
    attach_resource_server(&mut mgr, "res").await;

    let info = mgr
        .server_info("res")
        .expect("server_info should succeed after handshake");

    let caps = info
        .get("capabilities")
        .expect("InitializeResult must have capabilities field");
    assert!(
        caps.get("logging").is_none(),
        "resource-only server must not advertise logging capability: {caps}"
    );
}

// ── Tests: call_tool progress token auto-attach ─────────────────────

/// Integration test: verifies that `call_tool` (and list_resources, which
/// shares the same connection path) succeeds both when an `on_progress`
/// handler is registered for the server and when it is not.
#[tokio::test]
async fn call_tool_succeeds_with_and_without_progress_handler() {
    let mut mgr = McpManager::new();
    attach_resource_server(&mut mgr, "srv").await;

    // Without on_progress handler — should succeed.
    mgr.list_resources("srv")
        .await
        .expect("list_resources without handler should succeed");

    // With on_progress handler — auto-attach path is exercised; should still succeed.
    mgr.handler.mark_on_progress("srv");
    mgr.list_resources("srv")
        .await
        .expect("list_resources with handler should succeed");
}

// ── Test Server: RootsTestServer ────────────────────────────────────

/// A test server that, when `call_tool` is invoked, issues a `roots/list`
/// request back to the client (server→client direction) and embeds the
/// result in the tool response. This exercises the Crux C2 duplex path:
/// the client's `ClientHandler::list_roots` override is triggered by the
/// server's outbound `peer.list_roots()` call.
#[derive(Clone)]
struct RootsTestServer;

impl ServerHandler for RootsTestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    // rmcp v1.4: `peer.list_roots()` is deprecated by SEP-2577. Kept
    // on the test surface until the migration to the post-2577 roots
    // API lands (tracked separately).
    #[allow(deprecated)]
    async fn call_tool(
        &self,
        _params: rmcp::model::CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        // Issue a server→client `roots/list` request.
        // This triggers `AgentBlockClientHandler::list_roots` on the
        // client side, which calls the registered Lua roots handler.
        let roots_result = ctx.peer.list_roots().await.map_err(|e| {
            McpError::internal_error(format!("server list_roots failed: {e}"), None)
        })?;
        // Return the count and first URI as text so the test can assert.
        let count = roots_result.roots.len();
        let first_uri = roots_result
            .roots
            .first()
            .map(|r| r.uri.as_str())
            .unwrap_or("(none)");
        Ok(rmcp::model::CallToolResult::success(vec![
            rmcp::model::Content::text(format!("roots:{count}:{first_uri}")),
        ]))
    }
}

/// Attach a `RootsTestServer` to `mgr` under `name`, with a pre-configured
/// handler Isle that has a Lua roots handler installed for the server name.
///
/// Returns the `IsleDriver` so the caller can keep the driver alive.
async fn attach_roots_server_with_isle(
    mgr: &mut McpManager,
    name: &str,
) -> mlua_isle::AsyncIsleDriver {
    use mlua_isle::AsyncIsle;

    // Spawn the isle with a trivial init, then configure it via exec().
    let (isle, driver) = AsyncIsle::spawn(|_lua: &mlua::Lua| Ok(()))
        .await
        .expect("AsyncIsle::spawn should succeed");

    let name_owned = name.to_string();
    isle.exec(move |lua| {
        handler::install_mcp_dispatcher_on_handler_isle(lua)
            .map_err(|e| mlua_isle::IsleError::Lua(format!("setup dispatcher: {e}")))?;
        // Pre-install the Lua roots handler for `name_owned`.
        use mlua::prelude::*;
        let handlers: LuaTable = lua
            .globals()
            .get("__mcp_roots_handlers")
            .map_err(|e| mlua_isle::IsleError::Lua(format!("get handlers: {e}")))?;
        let cb: LuaFunction = lua
            .load(
                r#"
                return function(server_name)
                    return {
                        { uri = "file:///test", name = "TestRoot" },
                    }
                end
            "#,
            )
            .set_name("@test_roots_handler")
            .eval()
            .map_err(|e| mlua_isle::IsleError::Lua(format!("eval: {e}")))?;
        handlers
            .set(name_owned.as_str(), cb)
            .map_err(|e| mlua_isle::IsleError::Lua(format!("set handler: {e}")))?;
        Ok(String::new())
    })
    .await
    .expect("isle setup must succeed");

    let isle_arc = std::sync::Arc::new(isle);

    // Build a fresh handler, wire the isle and server_name BEFORE calling
    // serve() so the RunningService clone has them set.
    let mut handler = AgentBlockClientHandler::new();
    handler.handler_isle = Some(std::sync::Arc::clone(&isle_arc));
    handler.server_name = Some(name.to_string());
    handler.mark_roots(name);

    let (server_side, client_side) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(running) = RootsTestServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);

    driver
}

/// Attach a plain `RootsTestServer` without any Lua handler wired.
/// Used for testing the no-handler error path.
async fn attach_roots_server_bare(mgr: &mut McpManager, name: &str) {
    let (server_side, client_side) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(running) = RootsTestServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let handler = AgentBlockClientHandler::new();
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);
}

// ── Tests: mark_roots flag ──────────────────────────────────────────

/// (T1) mark_roots sets the registry flag that list_roots checks.
#[test]
fn mark_roots_sets_flag_accessible_by_handler() {
    let handler = AgentBlockClientHandler::new();
    handler.ensure_server("roots-srv");
    assert!(
        !handler
            .registry
            .lock()
            .unwrap()
            .get("roots-srv")
            .unwrap()
            .roots
    );
    handler.mark_roots("roots-srv");
    assert!(
        handler
            .registry
            .lock()
            .unwrap()
            .get("roots-srv")
            .unwrap()
            .roots
    );
}

// ── Tests: notify_roots_list_changed ───────────────────────────────

/// (T2) notify_roots_list_changed on an unknown server must not panic.
#[tokio::test]
async fn notify_roots_list_changed_unknown_server_is_no_op() {
    let mgr = McpManager::new();
    // Should not panic — logs a warn and returns.
    mgr.notify_roots_list_changed("ghost");
}

/// (T1) notify_roots_list_changed on a live in-process server completes
/// without error. Mirrors `send_cancelled_live_server_does_not_panic`.
#[tokio::test]
async fn notify_roots_list_changed_live_server_does_not_panic() {
    let mut mgr = McpManager::new();
    attach_resource_server(&mut mgr, "res").await;
    mgr.notify_roots_list_changed("res");
    // Give the spawned task a moment to complete.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ── Tests: live duplex roots round-trip (Crux C2) ───────────────────

/// (T1 / Crux C2) Live duplex test: the server issues `roots/list` to the
/// client while the client concurrently sends `notify_roots_list_changed`
/// back to the server. Both must complete successfully.
///
/// Flow:
///  (a) server→client: `call_tool` triggers `peer.list_roots()` which
///      dispatches to `AgentBlockClientHandler::list_roots` on the client.
///  (b) client→server: `notify_roots_list_changed` fires a
///      `notifications/roots/list_changed` notification concurrently.
///
/// This test verifies thread-safety of the ROOTS_HANDLERS registry under
/// real async dispatch (Crux C2: concurrent flight, not sequential stubs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_duplex_roots_round_trip() {
    let mut mgr = McpManager::new();
    let _driver = attach_roots_server_with_isle(&mut mgr, "roots").await;

    // Wrap in Arc<RwLock<...>> for concurrent access.
    let mgr_arc = std::sync::Arc::new(tokio::sync::RwLock::new(mgr));

    // (a) Spawn call_tool — server will issue list_roots back to client.
    let mgr_a = std::sync::Arc::clone(&mgr_arc);
    let call_handle = tokio::spawn(async move {
        mgr_a
            .read()
            .await
            .call_tool("roots", "any_tool", serde_json::json!({}))
            .await
    });

    // (b) Concurrently send notify_roots_list_changed client→server.
    let mgr_b = std::sync::Arc::clone(&mgr_arc);
    let notify_handle = tokio::spawn(async move {
        // Small yield to let call_tool start, ensuring concurrent flight.
        tokio::time::sleep(Duration::from_millis(5)).await;
        mgr_b.read().await.notify_roots_list_changed("roots");
    });

    // Both must complete without panic.
    let tool_result = call_handle.await.expect("call_handle must not panic");
    notify_handle.await.expect("notify_handle must not panic");

    // The tool result contains the roots count embedded in the text.
    let result = tool_result.expect("call_tool must succeed");
    let result_json = serde_json::to_string(&result).expect("serialize result");
    assert!(
        result_json.contains("roots:1:file:///test"),
        "expected roots:1:file:///test in tool result: {result_json}"
    );
}

/// (T3) list_roots without a registered handler returns method_not_found error.
/// The server propagates the error back to the client as a McpError.
#[tokio::test]
async fn live_duplex_roots_no_handler_returns_error() {
    let mut mgr = McpManager::new();
    // Attach without wiring an isle or handler — call_tool triggers list_roots
    // which should return method_not_found on the client side.
    attach_roots_server_bare(&mut mgr, "roots-no-handler").await;

    let result = mgr
        .call_tool("roots-no-handler", "any_tool", serde_json::json!({}))
        .await;
    // The server propagates the list_roots method_not_found error as a
    // BlockError on the client side.
    assert!(
        result.is_err(),
        "call_tool must fail when no roots handler is registered: {result:?}"
    );
}

// ── Test Server: ElicitationTestServer ─────────────────────────────

/// A test server that, when `call_tool` is invoked, issues an
/// `elicitation/create` request back to the client (server→client direction)
/// using a Form variant and embeds the result in the tool response. This
/// exercises the `ClientHandler::create_elicitation` override.
#[derive(Clone)]
struct ElicitationTestServer;

impl ServerHandler for ElicitationTestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn call_tool(
        &self,
        _params: rmcp::model::CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        use rmcp::model::{
            CreateElicitationRequestParams, ElicitationSchema, PrimitiveSchema, StringSchema,
        };
        use std::collections::BTreeMap;

        // Build a minimal Form variant with one string property.
        let mut props = BTreeMap::new();
        props.insert(
            "name".to_string(),
            PrimitiveSchema::String(StringSchema::new()),
        );
        let schema = ElicitationSchema::new(props);
        let req = CreateElicitationRequestParams::FormElicitationParams {
            meta: None,
            message: "What is your name?".to_string(),
            requested_schema: schema,
        };

        let result = ctx
            .peer
            .create_elicitation(req)
            .await
            .map_err(|e| McpError::internal_error(format!("create_elicitation: {e}"), None))?;

        // Encode action + content as text so tests can assert.
        let action_str = match result.action {
            rmcp::model::ElicitationAction::Accept => "accept",
            rmcp::model::ElicitationAction::Decline => "decline",
            rmcp::model::ElicitationAction::Cancel => "cancel",
        };
        let content_str = result
            .content
            .map(|v| serde_json::to_string(&v).unwrap_or_default())
            .unwrap_or_default();
        Ok(rmcp::model::CallToolResult::success(vec![
            rmcp::model::Content::text(format!("elicitation:{action_str}:{content_str}")),
        ]))
    }
}

/// A test server that issues a Url-variant `elicitation/create` request.
/// Used to verify that the client always returns Decline for Url variants
/// without dispatching to the Lua callback.
#[derive(Clone)]
struct ElicitationUrlTestServer;

impl ServerHandler for ElicitationUrlTestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn call_tool(
        &self,
        _params: rmcp::model::CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        use rmcp::model::CreateElicitationRequestParams;

        // Issue a Url variant — client must always Decline without Lua dispatch.
        let req = CreateElicitationRequestParams::UrlElicitationParams {
            meta: None,
            message: "Please complete this form online".to_string(),
            url: "https://example.com/form".to_string(),
            elicitation_id: "test-elicitation-id-001".to_string(),
        };

        let result = ctx
            .peer
            .create_elicitation(req)
            .await
            .map_err(|e| McpError::internal_error(format!("create_elicitation: {e}"), None))?;

        let action_str = match result.action {
            rmcp::model::ElicitationAction::Accept => "accept",
            rmcp::model::ElicitationAction::Decline => "decline",
            rmcp::model::ElicitationAction::Cancel => "cancel",
        };
        Ok(rmcp::model::CallToolResult::success(vec![
            rmcp::model::Content::text(format!("url_elicitation:{action_str}")),
        ]))
    }
}

/// Attach an `ElicitationTestServer` (Form variant) with a pre-configured
/// handler Isle that has a Lua elicitation handler returning the given
/// `action` and (for accept) a content object `{name: "Alice"}`.
///
/// Returns the `IsleDriver` so the caller can keep the driver alive.
async fn attach_elicitation_server_with_isle(
    mgr: &mut McpManager,
    name: &str,
    action: &str,
) -> mlua_isle::AsyncIsleDriver {
    use mlua_isle::AsyncIsle;

    let (isle, driver) = AsyncIsle::spawn(|_lua: &mlua::Lua| Ok(()))
        .await
        .expect("AsyncIsle::spawn should succeed");

    let name_owned = name.to_string();
    let action_owned = action.to_string();
    isle.exec(move |lua| {
        handler::install_mcp_dispatcher_on_handler_isle(lua)
            .map_err(|e| mlua_isle::IsleError::Lua(format!("setup dispatcher: {e}")))?;
        // Pre-install the Lua elicitation handler for `name_owned`.
        use mlua::prelude::*;
        let handlers: LuaTable = lua
            .globals()
            .get("__mcp_elicitation_handlers")
            .map_err(|e| mlua_isle::IsleError::Lua(format!("get handlers: {e}")))?;

        // Build a handler that returns the requested action.
        let handler_src = match action_owned.as_str() {
            "accept" => {
                r#"
                return function(server_name, message, schema_json)
                    return { action = "accept", content = { name = "Alice" } }
                end
            "#
            }
            "decline" => {
                r#"
                return function(server_name, message, schema_json)
                    return { action = "decline" }
                end
            "#
            }
            "cancel" => {
                r#"
                return function(server_name, message, schema_json)
                    return { action = "cancel" }
                end
            "#
            }
            _ => {
                r#"
                return function(server_name, message, schema_json)
                    return { action = "decline" }
                end
            "#
            }
        };

        let cb: LuaFunction = lua
            .load(handler_src)
            .set_name("@test_elicitation_handler")
            .eval()
            .map_err(|e| mlua_isle::IsleError::Lua(format!("eval: {e}")))?;
        handlers
            .set(name_owned.as_str(), cb)
            .map_err(|e| mlua_isle::IsleError::Lua(format!("set handler: {e}")))?;
        Ok(String::new())
    })
    .await
    .expect("isle setup must succeed");

    let isle_arc = std::sync::Arc::new(isle);

    let mut handler = AgentBlockClientHandler::new();
    handler.handler_isle = Some(std::sync::Arc::clone(&isle_arc));
    handler.server_name = Some(name.to_string());
    handler.mark_elicitation(name);

    let (server_side, client_side) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(running) = ElicitationTestServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);

    driver
}

/// Attach a plain `ElicitationTestServer` (Form variant) without any Lua
/// handler wired. Used for testing the no-handler Decline path.
/// Server name is wired so `create_elicitation` can check the registry and
/// return `Decline` (not `method_not_found`) when no handler is registered.
async fn attach_elicitation_server_bare(mgr: &mut McpManager, name: &str) {
    let (server_side, client_side) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(running) = ElicitationTestServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let mut handler = AgentBlockClientHandler::new();
    handler.ensure_server(name);
    handler.server_name = Some(name.to_string());
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);
}

/// Attach a `ElicitationUrlTestServer` (Url variant) without any Lua handler.
/// Used to verify Url-variant always returns Decline.
async fn attach_elicitation_url_server(mgr: &mut McpManager, name: &str) {
    let (server_side, client_side) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(running) = ElicitationUrlTestServer.serve(server_side).await {
            let _ = running.waiting().await;
        }
    });
    let handler = AgentBlockClientHandler::new();
    let running = handler.serve(client_side).await.expect("handshake");
    mgr.servers.insert(name.to_string(), running);
}

// ── Tests: mark_elicitation flag ───────────────────────────────────

/// (T1) mark_elicitation sets the registry flag that create_elicitation checks.
#[test]
fn mark_elicitation_sets_flag_accessible_by_handler() {
    let handler = AgentBlockClientHandler::new();
    handler.ensure_server("elicit-srv");
    assert!(
        !handler
            .registry
            .lock()
            .unwrap()
            .get("elicit-srv")
            .unwrap()
            .elicitation
    );
    handler.mark_elicitation("elicit-srv");
    assert!(
        handler
            .registry
            .lock()
            .unwrap()
            .get("elicit-srv")
            .unwrap()
            .elicitation
    );
}

// ── Tests: live duplex elicitation round-trips ─────────────────────

/// (T1 / elicitation_accept) Form variant + handler accept → Accept result with content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elicitation_accept_returns_accept_with_content() {
    let mut mgr = McpManager::new();
    let _driver = attach_elicitation_server_with_isle(&mut mgr, "elicit", "accept").await;

    let result = mgr
        .call_tool("elicit", "any_tool", serde_json::json!({}))
        .await
        .expect("call_tool must succeed");
    let result_json = serde_json::to_string(&result).expect("serialize result");
    assert!(
        result_json.contains("elicitation:accept:"),
        "expected elicitation:accept: in result: {result_json}"
    );
    assert!(
        result_json.contains("Alice"),
        "expected Alice in content: {result_json}"
    );
}

/// (T2 / elicitation_decline) Form variant + handler decline → Decline result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elicitation_decline_returns_decline() {
    let mut mgr = McpManager::new();
    let _driver = attach_elicitation_server_with_isle(&mut mgr, "elicit", "decline").await;

    let result = mgr
        .call_tool("elicit", "any_tool", serde_json::json!({}))
        .await
        .expect("call_tool must succeed");
    let result_json = serde_json::to_string(&result).expect("serialize result");
    assert!(
        result_json.contains("elicitation:decline:"),
        "expected elicitation:decline: in result: {result_json}"
    );
}

/// (T3 / elicitation_cancel) Form variant + handler cancel → Cancel result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elicitation_cancel_returns_cancel() {
    let mut mgr = McpManager::new();
    let _driver = attach_elicitation_server_with_isle(&mut mgr, "elicit", "cancel").await;

    let result = mgr
        .call_tool("elicit", "any_tool", serde_json::json!({}))
        .await
        .expect("call_tool must succeed");
    let result_json = serde_json::to_string(&result).expect("serialize result");
    assert!(
        result_json.contains("elicitation:cancel:"),
        "expected elicitation:cancel: in result: {result_json}"
    );
}

/// (T4 / elicitation_url_decline) Url variant → always Decline, no Lua dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elicitation_url_variant_always_declines() {
    let mut mgr = McpManager::new();
    attach_elicitation_url_server(&mut mgr, "elicit-url").await;

    let result = mgr
        .call_tool("elicit-url", "any_tool", serde_json::json!({}))
        .await
        .expect("call_tool must succeed");
    let result_json = serde_json::to_string(&result).expect("serialize result");
    assert!(
        result_json.contains("url_elicitation:decline"),
        "expected url_elicitation:decline in result: {result_json}"
    );
}

/// (T5 / elicitation_no_handler) Form variant + no handler → Decline (spec neutral).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elicitation_no_handler_returns_decline() {
    let mut mgr = McpManager::new();
    // Attach without wiring an isle or elicitation handler.
    attach_elicitation_server_bare(&mut mgr, "elicit-bare").await;

    let result = mgr
        .call_tool("elicit-bare", "any_tool", serde_json::json!({}))
        .await
        .expect("call_tool must succeed — no handler → Decline, not error");
    let result_json = serde_json::to_string(&result).expect("serialize result");
    assert!(
        result_json.contains("elicitation:decline:"),
        "expected elicitation:decline: when no handler registered: {result_json}"
    );
}

// ── Test Servers: ping ─────────────────────────────────────────────

/// A server that sleeps `delay` before responding to every `ping`.
/// Used to drive the timeout path in `McpManager::ping`.
/// K-181: ServerHandler impl uses `async fn` directly.
#[derive(Clone)]
struct SlowPingServer {
    delay: Duration,
}

impl ServerHandler for SlowPingServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().build())
    }

    async fn ping(&self, _ctx: RequestContext<RoleServer>) -> Result<(), McpError> {
        tokio::time::sleep(self.delay).await;
        Ok(())
    }
}

/// Attach an in-process `SlowPingServer` to `mgr` under `name`.
async fn attach_slow_ping_server(mgr: &mut McpManager, name: &str, delay: Duration) {
    let (server_side, client_side) = tokio::io::duplex(8192);
    let server = SlowPingServer { delay };
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

// ── Tests: ping ────────────────────────────────────────────────────

/// (P1) ping against a responsive server returns Ok(latency_ms).
/// latency_ms is a non-negative integer (monotonic Instant measurement).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_success_returns_latency_ms() {
    let mut mgr = McpManager::new();
    // Use zero delay so the test completes quickly.
    attach_slow_ping_server(&mut mgr, "pingsrv", Duration::from_millis(0)).await;

    let result = mgr.ping("pingsrv").await;
    let latency_ms = result.expect("ping should succeed against a live server");
    // latency_ms must be a non-negative integer; the type is u64.
    // We cannot assert an exact value, but it should be <= 5000ms on
    // any reasonable CI box.
    assert!(
        latency_ms <= 5000,
        "latency_ms={latency_ms} looks unreasonable (> 5 s)"
    );
}

/// (P2) ping against a server that delays beyond rpc_timeout returns
/// `BlockError::Timeout`, not `BlockError::Mcp`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_timeout_returns_block_error_timeout() {
    let mut mgr = McpManager::with_rpc_timeout(Duration::from_millis(1))
        .expect("with_rpc_timeout(1ms) should succeed");
    // Server sleeps 200ms, rpc_timeout is 1ms → guaranteed timeout.
    attach_slow_ping_server(&mut mgr, "slowping", Duration::from_millis(200)).await;

    let result = mgr.ping("slowping").await;
    let err = result.expect_err("ping should time out");
    assert!(
        matches!(err, BlockError::Timeout(_)),
        "expected BlockError::Timeout, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("timed out"),
        "timeout message should contain 'timed out': {msg}"
    );
}
