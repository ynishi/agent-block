//! E2E test: MCP Resource Subscribe round-trip callback fire verification.
//!
//! Spins up a `StreamableHttpService` (stateful mode) in-process, implementing
//! a `SubscribeTestServer` that sends `notify_resource_updated` immediately upon
//! receiving a `resources/subscribe` request.  The `agent-block` binary runs a
//! Lua fixture that:
//!   1. connects to the server via `mcp.connect_http`
//!   2. registers a `mcp.on_resource_update` callback
//!   3. calls `mcp.subscribe_resource("subsrv", "resource:///test-e2e")`
//!   4. sleeps 300 ms to let the notification arrive
//!   5. asserts that the callback fired with the correct uri
//!
//! This test satisfies the crux `e2e round-trip callback fire verification`
//! must_not_simplify constraint: the test does NOT merely verify that
//! `subscribe_resource` returns `ok=true`; it verifies that the Lua callback
//! actually fires with the correct `ev.uri` value.

mod common;

use predicates::prelude::*;
use rmcp::{
    model::{
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerInfo, SubscribeRequestParams,
    },
    service::{MaybeSendFuture, RequestContext},
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, RoleServer, ServerHandler,
};
use tokio_util::sync::CancellationToken;

// ── Subscribe Test Server ─────────────────────────────────────────────────────

/// An MCP server that:
/// - Advertises `resources` + `subscribe` capability.
/// - On `subscribe(params, ctx)`: spawns a task that immediately calls
///   `ctx.peer.notify_resource_updated(...)` with the subscribed URI, then
///   returns `Ok(())`.
#[derive(Clone)]
struct SubscribeTestServer;

impl ServerHandler for SubscribeTestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_resources_subscribe()
                .build(),
        )
    }

    fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + MaybeSendFuture + '_ {
        let uri = request.uri.clone();
        let peer = context.peer.clone();
        async move {
            // Spawn so subscribe response is sent before the notification arrives.
            tokio::spawn(async move {
                // Small delay to ensure the subscribe RPC response reaches the
                // client before the notification is dispatched (R3 race mitigation).
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let _ = peer
                    .notify_resource_updated(ResourceUpdatedNotificationParam { uri })
                    .await;
            });
            Ok(())
        }
    }
}

// ── HTTP server helper ────────────────────────────────────────────────────────

/// Bind an ephemeral TCP port and mount `SubscribeTestServer` behind a
/// stateful `StreamableHttpService`.  Returns the base URL and a
/// `CancellationToken`.
async fn spawn_subscribe_http_server() -> (String, CancellationToken) {
    let ct = CancellationToken::new();
    // Stateful mode: SSE push notifications require persistent sessions.
    let config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(None)
        .with_cancellation_token(ct.child_token());

    let service: StreamableHttpService<SubscribeTestServer, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(SubscribeTestServer), Default::default(), config);

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

// ── Test ──────────────────────────────────────────────────────────────────────

/// Round-trip: `mcp.subscribe_resource` → server-side `notify_resource_updated`
/// → Lua `on_resource_update` callback fires with correct `ev.uri`.
///
/// This is the crux C9 test: verifies end-to-end dispatch correctness, not
/// merely that `subscribe_resource` returns `ok=true`.
#[tokio::test]
async fn subscribe_resource_callback_fires_with_correct_uri() {
    let (url, ct) = spawn_subscribe_http_server().await;
    // Give the axum listener a moment to enter the accept loop.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url_clone = url.clone();
    tokio::task::spawn_blocking(move || {
        common::agent_block_cmd()
            .args([
                "-s",
                &format!(
                    "{}/tests/fixtures/mcp_on_resource_update_callback.lua",
                    env!("CARGO_MANIFEST_DIR")
                ),
            ])
            .env("MCP_HTTP_URL", &url_clone)
            .env("RUST_LOG", "off")
            .assert()
            .success()
            // subscribe RPC returned ok=true
            .stdout(predicate::str::contains("SUBSCRIBE_OK"))
            // callback fired at least once (crux C9: not just subscribe ok)
            .stdout(predicate::str::contains("RESOURCE_UPDATE_EV_OK"))
            // upvalue/uri check passed inside the callback
            .stdout(predicate::str::contains("UPDATE_HITS=1"))
            // fixture ran to completion
            .stdout(predicate::str::contains("FIXTURE_DONE"));
    })
    .await
    .expect("subprocess assertion task should not panic");

    ct.cancel();
}
