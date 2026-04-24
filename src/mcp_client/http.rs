//! HTTP/SSE transport builder for `McpManager::connect_http`.
//!
//! Provides `connect_http_transport` which performs the full connect+handshake
//! using rmcp's Streamable HTTP transport (reqwest backend, rmcp 1.4 internal
//! reqwest). The `transport-streamable-http-client-reqwest` feature must be
//! enabled in `Cargo.toml` (it is).

use rmcp::{
    service::{RoleClient, RunningService},
    transport::streamable_http_client::StreamableHttpClientTransportConfig,
    ServiceExt,
};

use crate::error::{BlockError, BlockResult};
use crate::mcp_client::handler::AgentBlockClientHandler;

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
    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    if let Some(auth) = opts
        .get("auth_header")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        config = config.auth_header(auth);
    }
    // `StreamableHttpClientTransport::from_config` uses rmcp's internal
    // reqwest::Client (0.13), which correctly implements StreamableHttpClient.
    let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);

    tokio::time::timeout(rpc_timeout, handler.serve(transport))
        .await
        .map_err(|_| {
            tracing::warn!(server = %name, url = %url, timeout = ?rpc_timeout, "mcp http initialize timed out");
            BlockError::Timeout(format!(
                "http connect '{name}' to {url} timed out after {rpc_timeout:?}"
            ))
        })?
        .map_err(|e| {
            tracing::warn!(server = %name, url = %url, error = %e, "mcp http initialize failed");
            BlockError::Mcp(format!("http connect '{name}' to {url}: {e}"))
        })
}
