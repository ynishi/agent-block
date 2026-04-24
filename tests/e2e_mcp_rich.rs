//! E2E tests for MCP rich client (binary-spawn variant).
//!
//! In-process tests for resources/prompts/progress live in
//! `src/mcp_client/mod.rs::rich_tests` because the crate is binary-only
//! (no lib target) and in-process duplex servers require direct access to
//! private McpManager internals.
//!
//! This file contains binary-spawn tests that exercise the CLI surface.

mod common;

/// Placeholder: future HTTP e2e tests will spawn an in-process HTTP MCP server
/// and test `connect_http` end-to-end via the `mcp.connect_http` Lua bridge.
/// For now this module intentionally has no test cases that require network.
#[cfg(test)]
mod http_e2e {
    // Reserved for HTTP transport integration tests.
}
