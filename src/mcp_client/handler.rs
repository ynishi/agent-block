//! `AgentBlockClientHandler` — custom `ClientHandler` for agent-block MCP clients.
//!
//! Subtask 1: structural skeleton.
//! Subtask 2: `on_progress` wired to `handler_isle` bytecode forwarding.
//! Subtask 3: `on_logging_message` log bridge + `create_message` sampling skeleton.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mlua_isle::AsyncIsle;
use rmcp::{
    handler::client::ClientHandler,
    model::{
        CreateMessageRequestParams, CreateMessageResult, LoggingLevel,
        LoggingMessageNotificationParam, ProgressNotificationParam, Role, SamplingMessage,
        SamplingMessageContent,
    },
    service::{NotificationContext, RequestContext, RoleClient},
    ErrorData as McpError,
};

/// Constant name of the Lua global table used to store per-server progress handlers
/// on the handler Isle.
pub(crate) const MCP_PROGRESS_HANDLERS: &str = "__mcp_progress_handlers";

/// Constant name of the Lua global table used to store per-server log handlers.
pub(crate) const MCP_LOG_HANDLERS: &str = "__mcp_log_handlers";

/// Constant name of the Lua global table used to store per-server sampling handlers.
pub(crate) const MCP_SAMPLING_HANDLERS: &str = "__mcp_sampling_handlers";

/// Constant name of the Lua dispatcher function called when a progress notification arrives.
const MCP_DISPATCH_PROGRESS: &str = "__mcp_dispatch_progress";

/// Constant name of the Lua dispatcher function called when a log notification arrives.
const MCP_DISPATCH_LOG: &str = "__mcp_dispatch_log";

/// Constant name of the Lua dispatcher function called for sampling/createMessage.
const MCP_DISPATCH_SAMPLING: &str = "__mcp_dispatch_sampling";

/// Per-server registry of optional Lua callbacks.
///
/// Boolean markers: `true` means a handler function has been registered on the
/// handler Isle under the corresponding table key. The actual bytecode lives on
/// the handler Isle only (not duplicated here).
pub(crate) struct ServerHandlerRegistry {
    /// Whether a Lua on_progress handler is installed on the handler Isle.
    pub(crate) on_progress: bool,
    /// Whether a Lua on_log handler is installed on the handler Isle.
    pub(crate) on_log: bool,
    /// Whether a Lua on_resource_updated handler is installed (placeholder).
    #[allow(dead_code)]
    pub(crate) on_resource_updated: bool,
    /// Whether a Lua sampling callback is installed on the handler Isle.
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
/// The `server_name` field is set per-connection (by `McpManager::connect` /
/// `connect_http`) before `clone()` so that `create_message` can look up the
/// correct sampling handler by server name without needing the `RequestContext`
/// to carry server identity.
///
/// # Subtask evolution
/// - Subtask 1: skeleton — all notification methods are the default no-ops from rmcp.
/// - Subtask 2: `on_progress` wired to `handler_isle` bytecode forwarding.
/// - Subtask 3: `on_logging_message` log bridge + `create_message` sampling skeleton.
#[derive(Clone)]
pub struct AgentBlockClientHandler {
    /// Keyed by server name so a single handler instance can serve multiple servers
    /// when the registry is shared across connections.
    pub(crate) registry: Arc<Mutex<HashMap<String, ServerHandlerRegistry>>>,
    /// Optional handler Isle for Lua callback dispatch.
    /// `None` in unit-test mode (no notification dispatch needed).
    pub(crate) handler_isle: Option<Arc<AsyncIsle>>,
    /// Server name for this connection — set before clone() in connect/connect_http.
    /// `None` for the shared template handler (before per-server clone).
    pub(crate) server_name: Option<String>,
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
            server_name: None,
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
    pub(crate) fn mark_on_progress(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.on_progress = true;
    }

    /// Mark that a Lua on_log handler has been installed on the handler Isle.
    pub(crate) fn mark_on_log(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.on_log = true;
    }

    /// Mark that a Lua sampling handler has been installed on the handler Isle.
    pub(crate) fn mark_sampling(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.sampling = true;
    }
}

impl Default for AgentBlockClientHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Install all MCP dispatcher tables and functions on the handler Isle.
///
/// Sets up:
/// - `__mcp_progress_handlers` table + `__mcp_dispatch_progress` function
/// - `__mcp_log_handlers` table + `__mcp_dispatch_log` function
/// - `__mcp_sampling_handlers` table + `__mcp_dispatch_sampling` function
///
/// Must be called inside an `AsyncIsle::exec` on the handler Isle during bridge
/// registration.
pub fn install_mcp_dispatcher_on_handler_isle(lua: &mlua::Lua) -> mlua::Result<()> {
    use mlua::prelude::*;

    // ── progress ──────────────────────────────────────────────────────────────
    lua.globals()
        .set(MCP_PROGRESS_HANDLERS, lua.create_table()?)?;

    let progress_src = r#"
        local HANDLERS = "__mcp_progress_handlers"
        return function(server_name, progress_token, progress, total, message)
            local handlers = _G[HANDLERS]
            local h = handlers and handlers[server_name]
            if type(h) ~= "function" then
                return
            end
            h(server_name, progress_token, tonumber(progress), tonumber(total), message)
        end
    "#;
    let dispatch_progress: LuaFunction = lua
        .load(progress_src)
        .set_name("@agent_block:__mcp_dispatch_progress")
        .eval()?;
    lua.globals()
        .set(MCP_DISPATCH_PROGRESS, dispatch_progress)?;

    // ── log ───────────────────────────────────────────────────────────────────
    lua.globals().set(MCP_LOG_HANDLERS, lua.create_table()?)?;

    let log_src = r#"
        local HANDLERS = "__mcp_log_handlers"
        return function(server_name, level, logger, data_json)
            local handlers = _G[HANDLERS]
            local h = handlers and handlers[server_name]
            if type(h) ~= "function" then
                return nil  -- signal: no handler, caller should use tracing fallback
            end
            h(server_name, level, logger, data_json)
            return true
        end
    "#;
    let dispatch_log: LuaFunction = lua
        .load(log_src)
        .set_name("@agent_block:__mcp_dispatch_log")
        .eval()?;
    lua.globals().set(MCP_DISPATCH_LOG, dispatch_log)?;

    // ── sampling ──────────────────────────────────────────────────────────────
    lua.globals()
        .set(MCP_SAMPLING_HANDLERS, lua.create_table()?)?;

    let sampling_src = r#"
        local HANDLERS = "__mcp_sampling_handlers"
        return function(server_name, params_json)
            local handlers = _G[HANDLERS]
            local h = handlers and handlers[server_name]
            if type(h) ~= "function" then
                return nil  -- signal: no handler registered
            end
            return h(server_name, params_json)
        end
    "#;
    let dispatch_sampling: LuaFunction = lua
        .load(sampling_src)
        .set_name("@agent_block:__mcp_dispatch_sampling")
        .eval()?;
    lua.globals()
        .set(MCP_DISPATCH_SAMPLING, dispatch_sampling)?;

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
            let message_str = params.message.unwrap_or_default();

            for server_name in server_names {
                let server_for_task = server_name.clone();
                let token_for_task = token_str.clone();
                let progress_for_task = progress_str.clone();
                let total_for_task = total_str.clone();
                let message_for_task = message_str.clone();
                let isle_ref = Arc::clone(&isle);

                // Spawn each dispatch as a separate task so a slow Lua handler
                // does not block the rmcp notification loop.
                tokio::spawn(async move {
                    let args = [
                        server_for_task.as_str(),
                        token_for_task.as_str(),
                        progress_for_task.as_str(),
                        total_for_task.as_str(),
                        message_for_task.as_str(),
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

    fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let isle = self.handler_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name = self.server_name.clone();

        async move {
            let level = &params.level;
            let logger = params.logger.as_deref().unwrap_or("");
            // Serialize data as JSON string for transport to Lua
            let data_str = match serde_json::to_string(&params.data) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        target: "mcp_client",
                        error = %e,
                        "on_logging_message: failed to serialize data"
                    );
                    return;
                }
            };

            let level_str = match level {
                LoggingLevel::Debug => "debug",
                LoggingLevel::Info | LoggingLevel::Notice => "info",
                LoggingLevel::Warning => "warning",
                LoggingLevel::Error
                | LoggingLevel::Critical
                | LoggingLevel::Alert
                | LoggingLevel::Emergency => "error",
            };

            // Save name string early so we can use it after the optional move.
            let sn_str = server_name.as_deref().unwrap_or("unknown").to_string();

            // Check if a Lua handler is registered for this server
            let has_lua_handler = server_name.as_deref().is_some_and(|sn| {
                registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(sn)
                    .is_some_and(|r| r.on_log)
            });

            if has_lua_handler {
                if let (Some(isle), Some(sn)) = (isle, server_name) {
                    let sn_task = sn.clone();
                    let level_task = level_str.to_string();
                    let logger_task = logger.to_string();
                    let data_task = data_str.clone();

                    tokio::spawn(async move {
                        let args = [
                            sn_task.as_str(),
                            level_task.as_str(),
                            logger_task.as_str(),
                            data_task.as_str(),
                        ];
                        let task = isle.spawn_coroutine_call(MCP_DISPATCH_LOG, &args);
                        if let Err(e) = task.await {
                            tracing::warn!(
                                target: "mcp_client",
                                server = %sn,
                                error = %e,
                                "log handler dispatch failed"
                            );
                        }
                    });
                    return;
                }
            }

            // No Lua handler or no isle — emit directly via tracing to "lua" target
            // so it appears in the same log stream as Lua log.* calls.
            match level {
                LoggingLevel::Debug => {
                    tracing::debug!(
                        target: "lua",
                        script = "mcp_server",
                        server = %sn_str,
                        logger = %logger,
                        "{}",
                        data_str
                    );
                }
                LoggingLevel::Info | LoggingLevel::Notice => {
                    tracing::info!(
                        target: "lua",
                        script = "mcp_server",
                        server = %sn_str,
                        logger = %logger,
                        "{}",
                        data_str
                    );
                }
                LoggingLevel::Warning => {
                    tracing::warn!(
                        target: "lua",
                        script = "mcp_server",
                        server = %sn_str,
                        logger = %logger,
                        "{}",
                        data_str
                    );
                }
                LoggingLevel::Error
                | LoggingLevel::Critical
                | LoggingLevel::Alert
                | LoggingLevel::Emergency => {
                    tracing::error!(
                        target: "lua",
                        script = "mcp_server",
                        server = %sn_str,
                        logger = %logger,
                        "{}",
                        data_str
                    );
                }
            }
        }
    }

    fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateMessageResult, McpError>> + Send + '_ {
        let isle = self.handler_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name = self.server_name.clone();

        async move {
            // If no server_name wired, fall through to method_not_found.
            let sn = match server_name.as_deref() {
                Some(s) => s.to_string(),
                None => {
                    return Err(McpError::method_not_found::<
                        rmcp::model::CreateMessageRequestMethod,
                    >());
                }
            };

            // Check if sampling handler is registered for this server.
            let has_sampling = {
                let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(&sn).is_some_and(|r| r.sampling)
            };

            if !has_sampling {
                return Err(McpError::method_not_found::<
                    rmcp::model::CreateMessageRequestMethod,
                >());
            }

            let isle = match isle {
                Some(i) => i,
                None => {
                    return Err(McpError::method_not_found::<
                        rmcp::model::CreateMessageRequestMethod,
                    >());
                }
            };

            // Serialize params to JSON for Lua dispatch.
            let params_json = match serde_json::to_string(&params) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        target: "mcp_client",
                        server = %sn,
                        error = %e,
                        "create_message: failed to serialize params"
                    );
                    return Err(McpError::internal_error(
                        format!("create_message serialize: {e}"),
                        None,
                    ));
                }
            };

            // Dispatch to Lua sampling handler and await result JSON.
            let sn_task = sn.clone();
            let params_task = params_json.clone();
            let result_json = isle
                .exec(move |lua| {
                    use mlua::prelude::*;
                    let dispatch: LuaFunction =
                        lua.globals().get(MCP_DISPATCH_SAMPLING).map_err(|e| {
                            mlua_isle::IsleError::Lua(format!(
                                "create_message: get dispatcher: {e}"
                            ))
                        })?;
                    let result: LuaValue = dispatch
                        .call((sn_task.as_str(), params_task.as_str()))
                        .map_err(|e| {
                            mlua_isle::IsleError::Lua(format!("create_message: dispatch: {e}"))
                        })?;

                    // Lua handler must return a table or nil.
                    match result {
                        LuaValue::Nil => Ok(String::new()),
                        LuaValue::Table(tbl) => {
                            // Serialize the table to JSON string.
                            let json_val = crate::bridge::lua_to_json(lua, LuaValue::Table(tbl))
                                .map_err(|e| {
                                    mlua_isle::IsleError::Lua(format!(
                                        "create_message: lua_to_json: {e}"
                                    ))
                                })?;
                            serde_json::to_string(&json_val).map_err(|e| {
                                mlua_isle::IsleError::Lua(format!("create_message: to_string: {e}"))
                            })
                        }
                        other => Err(mlua_isle::IsleError::Lua(format!(
                            "create_message: handler must return table or nil, got: {:?}",
                            other.type_name()
                        ))),
                    }
                })
                .await;

            match result_json {
                Err(e) => {
                    tracing::warn!(
                        target: "mcp_client",
                        server = %sn,
                        error = %e,
                        "create_message: handler isle error"
                    );
                    Err(McpError::internal_error(
                        format!("sampling handler: {e}"),
                        None,
                    ))
                }
                Ok(json_str) if json_str.is_empty() => {
                    // Lua returned nil — no handler registered in dispatcher
                    Err(McpError::method_not_found::<
                        rmcp::model::CreateMessageRequestMethod,
                    >())
                }
                Ok(json_str) => {
                    // Parse Lua response into CreateMessageResult fields.
                    let v: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
                        McpError::internal_error(
                            format!("sampling handler result parse: {e}"),
                            None,
                        )
                    })?;

                    let model = v
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let stop_reason = v
                        .get("stop_reason")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let role_str = v
                        .get("role")
                        .and_then(|v| v.as_str())
                        .unwrap_or("assistant");
                    let role = match role_str {
                        "user" => Role::User,
                        _ => Role::Assistant,
                    };
                    let content_str = v
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let message =
                        SamplingMessage::new(role, SamplingMessageContent::text(content_str));
                    let mut result = CreateMessageResult::new(message, model);
                    if let Some(sr) = stop_reason {
                        result = result.with_stop_reason(sr);
                    }
                    Ok(result)
                }
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
    fn new_handler_has_no_server_name() {
        let handler = AgentBlockClientHandler::new();
        assert!(handler.server_name.is_none());
    }

    #[test]
    fn server_name_is_preserved_through_clone() {
        let mut handler = AgentBlockClientHandler::new();
        handler.server_name = Some("srv-a".to_string());
        let cloned = handler.clone();
        assert_eq!(cloned.server_name.as_deref(), Some("srv-a"));
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
    fn mark_on_log_sets_flag() {
        let h = AgentBlockClientHandler::new();
        h.ensure_server("srv");
        h.mark_on_log("srv");
        let guard = h.registry.lock().unwrap();
        assert!(guard.get("srv").unwrap().on_log);
    }

    #[test]
    fn mark_sampling_sets_flag() {
        let h = AgentBlockClientHandler::new();
        h.ensure_server("srv");
        h.mark_sampling("srv");
        let guard = h.registry.lock().unwrap();
        assert!(guard.get("srv").unwrap().sampling);
    }

    #[test]
    fn install_dispatcher_creates_all_globals() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();

        let _: mlua::Table = lua.globals().get(MCP_PROGRESS_HANDLERS).unwrap();
        let _: mlua::Function = lua.globals().get(MCP_DISPATCH_PROGRESS).unwrap();
        let _: mlua::Table = lua.globals().get(MCP_LOG_HANDLERS).unwrap();
        let _: mlua::Function = lua.globals().get(MCP_DISPATCH_LOG).unwrap();
        let _: mlua::Table = lua.globals().get(MCP_SAMPLING_HANDLERS).unwrap();
        let _: mlua::Function = lua.globals().get(MCP_DISPATCH_SAMPLING).unwrap();
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

    #[test]
    fn log_dispatcher_returns_nil_when_no_handler() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();
        let dispatch: mlua::Function = lua.globals().get(MCP_DISPATCH_LOG).unwrap();
        // No handler registered — should return nil (not error).
        let result: mlua::Value = dispatch
            .call(("no-srv", "info", "logger", r#""some message""#))
            .unwrap();
        assert!(
            matches!(result, mlua::Value::Nil),
            "expected nil when no handler"
        );
    }

    #[test]
    fn log_dispatcher_calls_registered_handler() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();

        lua.load(
            r#"
            local called = {}
            __mcp_log_handlers["srv"] = function(sn, level, logger, data)
                called.sn = sn
                called.level = level
                called.logger = logger
                called.data = data
            end
            __mcp_dispatch_log("srv", "warn", "mylogger", '"hello"')
            assert(called.sn == "srv")
            assert(called.level == "warn")
            assert(called.logger == "mylogger")
            assert(called.data == '"hello"')
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn sampling_dispatcher_returns_nil_when_no_handler() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();
        let dispatch: mlua::Function = lua.globals().get(MCP_DISPATCH_SAMPLING).unwrap();
        let result: mlua::Value = dispatch.call(("no-srv", "{}")).unwrap();
        assert!(
            matches!(result, mlua::Value::Nil),
            "expected nil when no handler"
        );
    }

    #[test]
    fn sampling_dispatcher_calls_registered_handler() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();

        lua.load(
            r#"
            __mcp_sampling_handlers["srv"] = function(sn, params_json)
                return { model = "test-model", stop_reason = "endTurn",
                         role = "assistant", content = "hello" }
            end
            local result = __mcp_dispatch_sampling("srv", "{}")
            assert(type(result) == "table")
            assert(result.model == "test-model")
            assert(result.content == "hello")
        "#,
        )
        .exec()
        .unwrap();
    }
}
