//! `AgentBlockClientHandler` — custom `ClientHandler` for agent-block MCP clients.
//!
//! Subtask 1: structural skeleton only.
//! All notification methods keep the default no-op behaviour inherited from rmcp.
//! Subtask 2 will wire up progress / log callback dispatch.
//! Subtask 3 will wire up sampling/createMessage.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rmcp::handler::client::ClientHandler;

/// Phantom type placeholder for a Lua callback proxy.
///
/// Subtask 2 replaces this with a concrete type carrying bytecode + handler_isle.
pub struct LuaHandlerProxy;

/// Per-server registry of optional Lua callbacks.
///
/// Fields are `Option<Arc<LuaHandlerProxy>>` so the handler can hold a shared,
/// nullable reference to each callback type. `Arc` lets the notification task
/// clone the proxy without acquiring the registry mutex for the whole callback
/// execution. Subtask 2/3 populate the concrete proxy implementation.
///
/// Fields are declared as `pub(crate)` forward-declarations for Subtask 2/3.
/// `#[allow(dead_code)]` suppresses the placeholder warning until they are wired up.
#[allow(dead_code)]
pub(crate) struct ServerHandlerRegistry {
    pub(crate) on_progress: Option<Arc<LuaHandlerProxy>>,
    pub(crate) on_log: Option<Arc<LuaHandlerProxy>>,
    pub(crate) on_resource_updated: Option<Arc<LuaHandlerProxy>>,
    pub(crate) sampling: Option<Arc<LuaHandlerProxy>>,
}

impl ServerHandlerRegistry {
    fn new() -> Self {
        Self {
            on_progress: None,
            on_log: None,
            on_resource_updated: None,
            sampling: None,
        }
    }
}

/// Custom MCP client handler that holds per-server Lua callback registries.
///
/// `AgentBlockClientHandler` is cloned into each `RunningService<RoleClient, _>`.
/// The inner `Arc<Mutex<…>>` lets all clones share the same registry map so that
/// a callback registered via the Lua bridge after `connect` is immediately visible
/// to the handler running on the rmcp task.
///
/// # Subtask evolution
/// - Subtask 1: skeleton — all notification methods are the default no-ops from rmcp.
/// - Subtask 2: `on_progress` / `on_log` wired to `handler_isle` bytecode forwarding.
/// - Subtask 3: `create_message` delegates to the registered sampling Lua callback.
#[derive(Clone)]
pub struct AgentBlockClientHandler {
    /// Keyed by server name so a single handler instance can serve multiple servers
    /// when the registry is shared across connections.
    pub(crate) registry: Arc<Mutex<HashMap<String, ServerHandlerRegistry>>>,
}

impl AgentBlockClientHandler {
    /// Create a handler with an empty registry.
    ///
    /// Call this once per `McpManager` instance (shared across all servers managed
    /// by that instance). Subtask 2 will add a `handler_isle: Arc<AsyncIsle>`
    /// parameter for bytecode forwarding.
    pub fn new() -> Self {
        Self {
            registry: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Ensure a `ServerHandlerRegistry` entry exists for `server_name`.
    ///
    /// Called by `McpManager::connect` / `connect_http` (Subtask 2) so that
    /// the Lua bridge can register callbacks for the server at any point after
    /// the connection is established.
    pub(crate) fn ensure_server(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
    }
}

impl Default for AgentBlockClientHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// All notification methods intentionally keep the rmcp default no-op
/// implementations for Subtask 1. Override stubs will be added in Subtask 2/3.
impl ClientHandler for AgentBlockClientHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_handler_has_empty_registry() {
        let handler = AgentBlockClientHandler::new();
        let guard = handler.registry.lock().unwrap();
        assert!(guard.is_empty());
    }

    #[test]
    fn ensure_server_creates_entry() {
        let handler = AgentBlockClientHandler::new();
        handler.ensure_server("my-server");
        let guard = handler.registry.lock().unwrap();
        assert!(guard.contains_key("my-server"));
    }

    #[test]
    fn ensure_server_idempotent() {
        let handler = AgentBlockClientHandler::new();
        handler.ensure_server("srv");
        handler.ensure_server("srv");
        let guard = handler.registry.lock().unwrap();
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn clone_shares_registry() {
        let h1 = AgentBlockClientHandler::new();
        let h2 = h1.clone();
        h1.ensure_server("alpha");
        let guard = h2.registry.lock().unwrap();
        assert!(guard.contains_key("alpha"), "clone must share registry Arc");
    }
}
