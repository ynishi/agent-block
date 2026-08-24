//! HTTP/SSE transport builder for `McpManager::connect_http`.
//!
//! Provides `connect_http_transport` which performs the full connect+handshake
//! using rmcp's Streamable HTTP transport (rmcp-internal reqwest backend). The
//! `transport-streamable-http-client-reqwest` feature must be enabled in
//! `Cargo.toml` (it is).

use rmcp::{
    service::{RoleClient, RunningService},
    transport::streamable_http_client::StreamableHttpClientTransportConfig,
    ServiceExt,
};

use agent_block_types::error::{BlockError, BlockResult};
use agent_block_types::obs::sanitize_url;

use crate::handler::AgentBlockClientHandler;

/// Perform the MCP initialize handshake over Streamable HTTP transport.
///
/// `opts` may contain:
/// - `auth_header` (string): sent as `Authorization: Bearer <value>`.
///
/// On success returns a connected `RunningService` that can be inserted into
/// `McpManager::servers`.
pub(super) async fn connect_http_transport(
    name: &str,
    url: &str,
    opts: &serde_json::Value,
    handler: AgentBlockClientHandler,
    rpc_timeout: std::time::Duration,
) -> BlockResult<RunningService<RoleClient, AgentBlockClientHandler>> {
    // reqwest is built with `rustls-no-provider`, which panics at Client build
    // when no process-level CryptoProvider is installed. Install ring
    // idempotently; an earlier install by the embedding process (e.g. the CLI
    // installs it for WSS) wins and the failed re-install is ignored.
    static INSTALL_RING_PROVIDER: std::sync::Once = std::sync::Once::new();
    INSTALL_RING_PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });

    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    if let Some(auth) = opts
        .get("auth_header")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        config = config.auth_header(auth);
    }
    // `StreamableHttpClientTransport::from_config` uses rmcp's internal
    // reqwest::Client, which correctly implements StreamableHttpClient.
    let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);

    let safe_url = sanitize_url(url);
    tokio::time::timeout(rpc_timeout, handler.serve(transport))
        .await
        .map_err(|_| {
            tracing::warn!(server = %name, url = %safe_url, timeout = ?rpc_timeout, "mcp http initialize timed out");
            BlockError::Timeout(format!(
                "http connect '{name}' to {safe_url} timed out after {rpc_timeout:?}"
            ))
        })?
        .map_err(|e| {
            tracing::warn!(server = %name, url = %safe_url, error = %e, "mcp http initialize failed");
            BlockError::Mcp(format!("http connect '{name}' to {safe_url}: {e}"))
        })
}
