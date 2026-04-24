//! `AgentBlockClientHandler` — custom `ClientHandler` for agent-block MCP clients.
//!
//! Subtask 1: structural skeleton.
//! Subtask 2: `on_progress` wired to `handler_isle` bytecode forwarding.
//! Subtask 3: `create_message` delegates to the registered sampling Lua callback.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mlua_isle::AsyncIsle;
use rmcp::{
    handler::client::ClientHandler,
    model::ProgressNotificationParam,
    service::{NotificationContext, RoleClient},
};

/// Constant name of the Lua global table used to store per-server progress handlers
/// on the handler Isle.
pub(crate) const MCP_PROGRESS_HANDLERS: &str = "__mcp_progress_handlers";

/// Constant name of the Lua dispatcher function called when a progress notification arrives.
const MCP_DISPATCH_PROGRESS: &str = "__mcp_dispatch_progress";

/// Per-server registry of optional Lua callbacks.
///
/// `on_progress` is a boolean marker: `true` means a handler function has been
/// registered on the handler Isle under `__mcp_progress_handlers[server_name]`.
/// The actual bytecode lives on the handler Isle only (not duplicated here).
pub(crate) struct ServerHandlerRegistry {
    /// Whether a Lua on_progress handler is installed on the handler Isle.
    pub(crate) on_progress: bool,
    /// Placeholder for future on_log handler (Subtask 3).
    #[allow(dead_code)]
    pub(crate) on_log: bool,
    /// Placeholder for future on_resource_updated handler (Subtask 3).
    #[allow(dead_code)]
    pub(crate) on_resource_updated: bool,
    /// Placeholder for future sampling callback (Subtask 3).
    #[allow(dead_code)]
    pub(crate) sampling: bool,
}

impl ServerHandlerRegistry {
    fn new() -> Self {
        Self {
            on_progress: false,
            on_log: false,
            on_resource_updated: false,
            sampling: false,
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
/// - Subtask 2: `on_progress` wired to `handler_isle` bytecode forwarding.
/// - Subtask 3: `create_message` delegates to the registered sampling Lua callback.
#[derive(Clone)]
pub struct AgentBlockClientHandler {
    /// Keyed by server name so a single handler instance can serve multiple servers
    /// when the registry is shared across connections.
    pub(crate) registry: Arc<Mutex<HashMap<String, ServerHandlerRegistry>>>,
    /// Optional handler Isle for Lua callback dispatch.
    /// `None` in unit-test mode (no notification dispatch needed).
    pub(crate) handler_isle: Option<Arc<AsyncIsle>>,
}

impl AgentBlockClientHandler {
    /// Create a handler with an empty registry (no notification dispatch).
    ///
    /// Used in concurrency tests and contexts where no `handler_isle` is available.
    /// Notifications received while `handler_isle` is `None` are silently dropped
    /// (no Lua callback can execute without an Isle).
    pub fn new() -> Self {
        Self {
            registry: Arc::new(Mutex::new(HashMap::new())),
            handler_isle: None,
        }
    }

    /// Ensure a `ServerHandlerRegistry` entry exists for `server_name`.
    ///
    /// Called by `McpManager::connect` / `connect_http` so that
    /// the Lua bridge can register callbacks for the server at any point after
    /// the connection is established.
    pub(crate) fn ensure_server(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
    }

    /// Mark that a Lua on_progress handler has been installed on the handler Isle
    /// for the given server.
    ///
    /// Called by `bridge::mcp::register` after forwarding the bytecode to the
    /// handler Isle. This lets `on_progress` dispatch know which servers have
    /// active Lua callbacks registered.
    pub(crate) fn mark_on_progress(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.on_progress = true;
    }
}

impl Default for AgentBlockClientHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Install `__mcp_progress_handlers` table and `__mcp_dispatch_progress` function
/// on the handler Isle.
///
/// Must be called inside an `AsyncIsle::exec` on the handler Isle during bridge
/// registration (analogous to `install_bus_dispatcher_on_handler_isle`).
pub fn install_mcp_dispatcher_on_handler_isle(lua: &mlua::Lua) -> mlua::Result<()> {
    use mlua::prelude::*;

    lua.globals()
        .set(MCP_PROGRESS_HANDLERS, lua.create_table()?)?;

    // Pure-Lua dispatcher so user handlers can yield across bridge calls.
    let src = r#"
        local HANDLERS = "__mcp_progress_handlers"
        return function(server_name, progress_token, progress, total)
            local handlers = _G[HANDLERS]
            local h = handlers and handlers[server_name]
            if type(h) ~= "function" then
                return
            end
            h(server_name, progress_token, tonumber(progress), tonumber(total))
        end
    "#;
    let dispatch: LuaFunction = lua
        .load(src)
        .set_name("@agent_block:__mcp_dispatch_progress")
        .eval()?;
    lua.globals().set(MCP_DISPATCH_PROGRESS, dispatch)?;
    Ok(())
}

impl ClientHandler for AgentBlockClientHandler {
    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        // Clone Arc refs BEFORE the async block to avoid holding the Mutex
        // guard across any await (await-holding-lock anti-pattern).
        let isle = self.handler_isle.clone();
        let registry = Arc::clone(&self.registry);

        async move {
            let isle = match isle {
                Some(i) => i,
                None => return, // no isle configured — drop notification
            };

            // Collect server names that have an on_progress handler registered.
            // rmcp does not provide server identity in NotificationContext for
            // client notifications, so we dispatch to all registered handlers
            // and let the Lua side filter by token if needed.
            let server_names: Vec<String> = {
                let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .iter()
                    .filter_map(|(name, reg)| {
                        if reg.on_progress {
                            Some(name.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            };
            // guard is dropped here — no await held

            let token_str = match &params.progress_token.0 {
                rmcp::model::NumberOrString::Number(n) => n.to_string(),
                rmcp::model::NumberOrString::String(s) => s.to_string(),
            };
            let progress_str = params.progress.to_string();
            let total_str = params
                .total
                .map(|t| t.to_string())
                .unwrap_or_else(|| "0".to_string());

            for server_name in server_names {
                let server_for_task = server_name.clone();
                let token_for_task = token_str.clone();
                let progress_for_task = progress_str.clone();
                let total_for_task = total_str.clone();
                let isle_ref = Arc::clone(&isle);

                // Spawn each dispatch as a separate task so a slow Lua handler
                // does not block the rmcp notification loop.
                tokio::spawn(async move {
                    let args = [
                        server_for_task.as_str(),
                        token_for_task.as_str(),
                        progress_for_task.as_str(),
                        total_for_task.as_str(),
                    ];
                    let task = isle_ref.spawn_coroutine_call(MCP_DISPATCH_PROGRESS, &args);
                    if let Err(e) = task.await {
                        tracing::warn!(
                            target: "mcp_client",
                            server = %server_name,
                            error = %e,
                            "progress handler failed"
                        );
                    }
                });
            }
        }
    }
}

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

    #[test]
    fn mark_on_progress_sets_flag() {
        let h = AgentBlockClientHandler::new();
        h.ensure_server("srv");
        h.mark_on_progress("srv");
        let guard = h.registry.lock().unwrap();
        assert!(guard.get("srv").unwrap().on_progress);
    }

    #[test]
    fn install_dispatcher_creates_globals() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();
        let tbl: mlua::Table = lua.globals().get(MCP_PROGRESS_HANDLERS).unwrap();
        assert_eq!(tbl.raw_len(), 0, "empty table expected");
        let _fn: mlua::Function = lua.globals().get(MCP_DISPATCH_PROGRESS).unwrap();
    }

    #[test]
    fn dispatcher_calls_handler_with_progress() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();

        lua.load(
            r#"
            local results = {}
            __mcp_progress_handlers["my-srv"] = function(srv, tok, prog, total)
                results[#results+1] = { srv=srv, tok=tok, prog=prog, total=total }
            end
            __mcp_dispatch_progress("my-srv", "tok-1", "0.5", "1.0")
            assert(#results == 1)
            assert(results[1].srv == "my-srv")
            assert(results[1].tok == "tok-1")
            assert(math.abs(results[1].prog - 0.5) < 0.001)
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn dispatcher_no_op_when_no_handler() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();
        // Should not error when no handler is registered.
        let dispatch: mlua::Function = lua.globals().get(MCP_DISPATCH_PROGRESS).unwrap();
        dispatch
            .call::<()>(("no-srv", "tok", "0.5", "1.0"))
            .unwrap();
    }
}
