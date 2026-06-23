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
        CreateElicitationRequestParams, CreateElicitationResult, CreateMessageRequestParams,
        CreateMessageResult, ElicitationAction, ElicitationCreateRequestMethod, LoggingLevel,
        LoggingMessageNotificationParam, ProgressNotificationParam,
        ResourceUpdatedNotificationParam, Role, SamplingMessage, SamplingMessageContent,
    },
    service::{NotificationContext, RequestContext, RoleClient},
    ErrorData as McpError,
};
use tokio::sync::mpsc;

/// Constant name of the Lua global table used to store per-server sampling handlers
/// on the handler Isle.
pub(crate) const MCP_SAMPLING_HANDLERS: &str = "__mcp_sampling_handlers";

/// Constant name of the Lua dispatcher function called for sampling/createMessage.
const MCP_DISPATCH_SAMPLING: &str = "__mcp_dispatch_sampling";

/// Constant name of the Lua global table used to store per-server roots handlers
/// on the handler Isle.
pub(crate) const MCP_ROOTS_HANDLERS: &str = "__mcp_roots_handlers";

/// Constant name of the Lua dispatcher function called for roots/list requests.
const MCP_DISPATCH_ROOTS: &str = "__mcp_dispatch_roots";

/// Constant name of the Lua global table used to store per-server elicitation handlers
/// on the handler Isle.
pub(crate) const MCP_ELICITATION_HANDLERS: &str = "__mcp_elicitation_handlers";

/// Constant name of the Lua dispatcher function called for elicitation/create requests.
const MCP_DISPATCH_ELICITATION: &str = "__mcp_dispatch_elicitation";

/// Global table that holds user-provided progress callbacks stored by server name
/// on the **main Isle**.
///
/// Written by `mcp.on_progress` (main Isle bridge) so that `on_progress`
/// notifications dispatched via `main_isle.exec` can call the closure with its
/// upvalues intact (no bytecode dump/reload across Lua VMs).
pub const MCP_USER_PROGRESS_CBS: &str = "__mcp_user_progress_cbs";

/// Global table that holds user-provided log callbacks stored by server name
/// on the **main Isle**.
///
/// Same rationale as `MCP_USER_PROGRESS_CBS`.
pub const MCP_USER_LOG_CBS: &str = "__mcp_user_log_cbs";

/// Global table that holds user-provided resource-update callbacks stored by
/// server name on the **main Isle**.
///
/// Same rationale as `MCP_USER_PROGRESS_CBS`.
pub const MCP_USER_RESOURCE_UPDATE_CBS: &str = "__mcp_user_resource_update_cbs";

/// Global table that holds user-provided resources-list-changed callbacks stored
/// by server name on the **main Isle**.
///
/// Same rationale as `MCP_USER_PROGRESS_CBS`.
pub const MCP_USER_RESOURCES_LIST_CHANGED_CBS: &str = "__mcp_user_resources_list_changed_cbs";

/// Global table that holds user-provided tools-list-changed callbacks stored by
/// server name on the **main Isle**.
///
/// Same rationale as `MCP_USER_PROGRESS_CBS`.
pub const MCP_USER_TOOLS_LIST_CHANGED_CBS: &str = "__mcp_user_tools_list_changed_cbs";

/// Global table that holds user-provided prompts-list-changed callbacks stored
/// by server name on the **main Isle**.
///
/// Same rationale as `MCP_USER_PROGRESS_CBS`.
pub const MCP_USER_PROMPTS_LIST_CHANGED_CBS: &str = "__mcp_user_prompts_list_changed_cbs";

/// Capacity of the bounded notification dispatch channel.
///
/// A chatty server emitting progress faster than Lua can consume will fill
/// the channel; notifications beyond this limit are dropped with a warning
/// rather than growing memory without bound.
const NOTIFY_CHANNEL_CAPACITY: usize = 128;

/// Type alias for the event-builder closure used in `NotificationItem`.
type BuildEvFn = Box<dyn FnOnce(&mlua::Lua, &str) -> mlua::Result<mlua::Table> + Send + 'static>;

/// A single notification item routed through the bounded dispatch channel.
///
/// Carries everything the dispatch task needs to call the user Lua callback
/// on the main Isle: the server name, the callback table key, the event builder
/// closure, and a label for log messages.
pub(crate) struct NotificationItem {
    pub(crate) isle: Arc<AsyncIsle>,
    pub(crate) server_name: String,
    pub(crate) cbs_table: &'static str,
    pub(crate) build_ev: BuildEvFn,
    pub(crate) caller: &'static str,
}

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
    /// Whether a Lua on_resource_updated handler is installed.
    pub(crate) on_resource_updated: bool,
    /// Whether a Lua on_resource_list_changed handler is installed.
    pub(crate) on_resource_list_changed: bool,
    /// Whether a Lua on_tool_list_changed handler is installed.
    pub(crate) on_tool_list_changed: bool,
    /// Whether a Lua on_prompt_list_changed handler is installed.
    pub(crate) on_prompt_list_changed: bool,
    /// Whether a Lua sampling callback is installed on the handler Isle.
    pub(crate) sampling: bool,
    /// Whether a Lua roots handler callback is installed on the handler Isle.
    pub(crate) roots: bool,
    /// Whether a Lua elicitation handler callback is installed on the handler Isle.
    pub(crate) elicitation: bool,
    /// Whether to inject `__ab_obs` trace context into `call_tool` arguments
    /// for this server. Opt-in (default: `false`) to avoid leaking agent
    /// identity to untrusted or third-party MCP servers.
    pub(crate) trace_context: bool,
}

impl ServerHandlerRegistry {
    fn new() -> Self {
        Self {
            on_progress: false,
            on_log: false,
            on_resource_updated: false,
            on_resource_list_changed: false,
            on_tool_list_changed: false,
            on_prompt_list_changed: false,
            sampling: false,
            roots: false,
            elicitation: false,
            trace_context: false,
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
/// - Subtask 4: progress/log notifications dispatched to main Isle via `exec` so user
///   callbacks run with their upvalues intact (no bytecode dump/reload across VMs).
/// - Subtask 5 (M-3): bounded notification channel replaces per-notification spawns
///   to cap memory growth when a chatty server floods notifications faster than Lua
///   can consume them.
#[derive(Clone)]
pub struct AgentBlockClientHandler {
    /// Keyed by server name so a single handler instance can serve multiple servers
    /// when the registry is shared across connections.
    pub(crate) registry: Arc<Mutex<HashMap<String, ServerHandlerRegistry>>>,
    /// Optional handler Isle for sampling (`create_message`) dispatch via `exec`.
    /// `None` in unit-test mode.
    pub(crate) handler_isle: Option<Arc<AsyncIsle>>,
    /// Optional main Isle for progress/log notification dispatch via `exec`.
    /// User callbacks (`on_progress`, `on_log`) are stored in the main Isle's
    /// globals so upvalues are preserved across calls (no bytecode dump needed).
    /// `None` in unit-test mode.
    pub(crate) main_isle: Option<Arc<AsyncIsle>>,
    /// Server name for this connection — set before clone() in connect/connect_http.
    /// `None` for the shared template handler (before per-server clone).
    pub(crate) server_name: Option<String>,
    /// Bounded sender for the per-handler notification dispatch channel.
    ///
    /// `on_progress` and `on_logging_message` send items here instead of spawning
    /// an unbounded `tokio::spawn` per notification.  A single dispatch task
    /// (started via `start_dispatch_task`) drains the channel and calls
    /// `isle.exec` sequentially, preserving the rmcp-loop-non-blocking property
    /// while capping queue depth at `NOTIFY_CHANNEL_CAPACITY`.
    ///
    /// `mpsc::Sender` is cheap to clone (Arc-backed), so `#[derive(Clone)]`
    /// on the handler just clones the sender end — all handler clones share the
    /// same channel and dispatch task.
    pub(crate) notify_tx: Option<mpsc::Sender<NotificationItem>>,
}

impl AgentBlockClientHandler {
    /// Create a handler with an empty registry (no notification dispatch).
    ///
    /// Used in concurrency tests and contexts where no Isle is available.
    /// Notifications received while `main_isle` is `None` are silently dropped
    /// (no Lua callback can execute without an Isle).
    pub fn new() -> Self {
        Self {
            registry: Arc::new(Mutex::new(HashMap::new())),
            handler_isle: None,
            main_isle: None,
            server_name: None,
            notify_tx: None,
        }
    }

    /// Create and start the bounded notification dispatch task.
    ///
    /// Must be called after `main_isle` is wired.  Idempotent: a second call
    /// replaces the channel (the previous dispatch task drains to completion).
    ///
    /// Returns a clone of the sender so `McpManager::set_main_isle` can store it
    /// back onto the shared template handler.
    pub(crate) fn start_dispatch_task(&mut self) {
        let (tx, mut rx) = mpsc::channel::<NotificationItem>(NOTIFY_CHANNEL_CAPACITY);
        self.notify_tx = Some(tx);
        // Spawn the single dispatch task.  It runs for the lifetime of the channel.
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let sn = item.server_name.clone();
                let result = item
                    .isle
                    .exec(move |lua| {
                        use mlua::prelude::*;
                        let cbs: LuaTable = match lua.globals().get(item.cbs_table) {
                            Ok(t) => t,
                            Err(_) => return Ok(String::new()),
                        };
                        let cb: LuaFunction = match cbs.get(item.server_name.as_str()) {
                            Ok(f) => f,
                            Err(_) => return Ok(String::new()),
                        };
                        let ev = (item.build_ev)(lua, item.server_name.as_str()).map_err(|e| {
                            mlua_isle::IsleError::Lua(format!("{}: build_ev: {e}", item.caller))
                        })?;
                        if let Err(e) = cb.call::<()>(ev) {
                            tracing::warn!(
                                target: "mcp_client",
                                server = %item.server_name,
                                caller = %item.caller,
                                error = %e,
                                "user callback returned error"
                            );
                        }
                        Ok(String::new())
                    })
                    .await;
                if let Err(e) = result {
                    tracing::warn!(
                        target: "mcp_client",
                        server = %sn,
                        error = %e,
                        "notification dispatch: main isle exec failed"
                    );
                }
            }
        });
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
    pub fn mark_on_progress(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.on_progress = true;
    }

    /// Mark that a Lua on_log handler has been installed on the handler Isle.
    pub fn mark_on_log(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.on_log = true;
    }

    /// Mark that a Lua on_resource_updated handler has been installed.
    pub fn mark_on_resource_updated(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.on_resource_updated = true;
    }

    /// Mark that a Lua on_resource_list_changed handler has been installed.
    pub fn mark_on_resource_list_changed(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.on_resource_list_changed = true;
    }

    /// Mark that a Lua on_tool_list_changed handler has been installed.
    pub fn mark_on_tool_list_changed(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.on_tool_list_changed = true;
    }

    /// Mark that a Lua on_prompt_list_changed handler has been installed.
    pub fn mark_on_prompt_list_changed(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.on_prompt_list_changed = true;
    }

    /// Set whether trace context (`__ab_obs`) should be injected into `call_tool`
    /// arguments for the named server.  Defaults to `false` (opt-in).
    pub(crate) fn set_trace_context(&self, server_name: &str, enabled: bool) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.trace_context = enabled;
    }

    /// Return whether trace context injection is enabled for the named server.
    pub fn trace_context_enabled(&self, server_name: &str) -> bool {
        let guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(server_name).is_some_and(|r| r.trace_context)
    }

    /// Mark that a Lua sampling handler has been installed on the handler Isle.
    pub fn mark_sampling(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.sampling = true;
    }

    /// Mark that a Lua roots handler has been installed on the handler Isle.
    ///
    /// # Arguments
    /// - `server_name` — the server for which the roots handler was registered.
    ///
    /// # Side effects
    /// Creates a registry entry for the server if one does not yet exist, then
    /// sets `roots = true` so that `list_roots` requests are dispatched to the
    /// Lua callback rather than returning `method_not_found`.
    pub fn mark_roots(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.roots = true;
    }

    /// Mark that a Lua elicitation handler has been installed on the handler Isle.
    ///
    /// # Arguments
    /// - `server_name` — the server for which the elicitation handler was registered.
    ///
    /// # Side effects
    /// Creates a registry entry for the server if one does not yet exist, then
    /// sets `elicitation = true` so that `create_elicitation` requests are dispatched
    /// to the Lua callback rather than returning `Decline` (no-handler path).
    pub fn mark_elicitation(&self, server_name: &str) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(server_name.to_string())
            .or_insert_with(ServerHandlerRegistry::new);
        entry.elicitation = true;
    }
}

impl Default for AgentBlockClientHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Install MCP dispatcher tables and functions on the handler Isle.
///
/// Sets up:
/// - `__mcp_sampling_handlers` table + `__mcp_dispatch_sampling` function
///
/// Progress and log notifications are now dispatched directly to the main Isle
/// via `main_isle.exec` in `AgentBlockClientHandler::on_progress` /
/// `on_logging_message`, so the handler Isle no longer needs those dispatcher
/// globals.
///
/// Must be called inside an `AsyncIsle::exec` on the handler Isle during bridge
/// registration.
pub fn install_mcp_dispatcher_on_handler_isle(lua: &mlua::Lua) -> mlua::Result<()> {
    use mlua::prelude::*;

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

    // ── roots ──────────────────────────────────────────────────────────────────
    lua.globals().set(MCP_ROOTS_HANDLERS, lua.create_table()?)?;

    let roots_src = r#"
        local HANDLERS = "__mcp_roots_handlers"
        return function(server_name)
            local handlers = _G[HANDLERS]
            local h = handlers and handlers[server_name]
            if type(h) ~= "function" then
                return nil  -- signal: no handler registered
            end
            return h(server_name)
        end
    "#;
    let dispatch_roots: LuaFunction = lua
        .load(roots_src)
        .set_name("@agent_block:__mcp_dispatch_roots")
        .eval()?;
    lua.globals().set(MCP_DISPATCH_ROOTS, dispatch_roots)?;

    // ── elicitation ───────────────────────────────────────────────────────────
    lua.globals()
        .set(MCP_ELICITATION_HANDLERS, lua.create_table()?)?;

    let elicitation_src = r#"
        local HANDLERS = "__mcp_elicitation_handlers"
        return function(server_name, message, schema_json)
            local handlers = _G[HANDLERS]
            local h = handlers and handlers[server_name]
            if type(h) ~= "function" then
                return nil  -- signal: no handler registered → Decline
            end
            return h(server_name, message, schema_json)
        end
    "#;
    let dispatch_elicitation: LuaFunction = lua
        .load(elicitation_src)
        .set_name("@agent_block:__mcp_dispatch_elicitation")
        .eval()?;
    lua.globals()
        .set(MCP_DISPATCH_ELICITATION, dispatch_elicitation)?;

    Ok(())
}

/// Dispatch a notification to the Lua callback stored under `cbs_table[server_name]`
/// on the provided main Isle.
///
/// This helper encapsulates the common "look up cb in globals table → build ev →
/// spawn → isle.exec → pcall → log error" pattern shared by `on_progress` and
/// `on_logging_message`. Extracting it here mechanically prevents the H-1-style
/// divergence where independently-edited methods drift apart.
///
/// `build_ev` receives the Lua state and the server name (already moved into the
/// closure) and must return the event table to pass to the callback. The callback
/// is invoked with pcall semantics: a Lua error inside the callback is logged at
/// warn level but does not propagate into the main Isle runtime.
///
/// `create_message` is intentionally kept out of scope — it has a different
/// shape (it returns a value rather than being fire-and-forget).
fn isle_dispatch<F>(
    isle: Arc<AsyncIsle>,
    server_name: String,
    cbs_table: &'static str,
    build_ev: F,
    caller: &'static str,
) where
    F: FnOnce(&mlua::Lua, &str) -> mlua::Result<mlua::Table> + Send + 'static,
{
    tokio::spawn(async move {
        let sn = server_name.clone();
        let result = isle
            .exec(move |lua| {
                use mlua::prelude::*;
                // Look up the per-server callback table on the main Isle.
                let cbs: LuaTable = match lua.globals().get(cbs_table) {
                    Ok(t) => t,
                    Err(_) => return Ok(String::new()), // table not yet initialised
                };
                let cb: LuaFunction = match cbs.get(server_name.as_str()) {
                    Ok(f) => f,
                    Err(_) => return Ok(String::new()), // no handler for this server
                };
                // Build the event table and invoke the user callback.
                // pcall semantics: absorb errors so a user callback crash
                // does not propagate into the main Isle runtime.
                let ev = build_ev(lua, server_name.as_str())
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("{caller}: build_ev: {e}")))?;
                if let Err(e) = cb.call::<()>(ev) {
                    tracing::warn!(
                        target: "mcp_client",
                        server = %server_name,
                        caller = %caller,
                        error = %e,
                        "user callback returned error"
                    );
                }
                Ok(String::new())
            })
            .await;
        if let Err(e) = result {
            tracing::warn!(
                target: "mcp_client",
                server = %sn,
                error = %e,
                "{}: main isle exec failed",
                caller
            );
        }
    });
}

impl ClientHandler for AgentBlockClientHandler {
    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        // Clone Arc refs and server_name BEFORE the async block to avoid holding
        // the Mutex guard across any await (await-holding-lock anti-pattern).
        let main_isle = self.main_isle.clone();
        let registry = Arc::clone(&self.registry);
        // Clone server_name here (before async move) so the originating server
        // identity is available inside the future without capturing &self.
        let server_name_opt = self.server_name.clone();
        // Clone the notification channel sender (cheap: mpsc::Sender is Arc-backed).
        let notify_tx = self.notify_tx.clone();

        async move {
            let main_isle = match main_isle {
                Some(i) => i,
                None => return, // no Isle configured — drop notification
            };

            // Mirror on_logging_message: dispatch only for the originating server.
            // The registry-wide fan-out that was here previously was a bug: every
            // server with on_progress=true would receive every other server's
            // notification, causing bogus ev.server attributions and callback
            // over-counting proportional to N_servers.
            let server_name = match server_name_opt {
                Some(s) => s,
                None => return, // no server identity — cannot route notification
            };
            let has_cb = {
                let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(&server_name).is_some_and(|r| r.on_progress)
            };
            // guard is dropped here — no await held
            if !has_cb {
                return;
            }

            let token_str = match &params.progress_token.0 {
                rmcp::model::NumberOrString::Number(n) => n.to_string(),
                rmcp::model::NumberOrString::String(s) => s.to_string(),
            };
            let progress_f64: f64 = params.progress;
            let total_opt: Option<f64> = params.total;
            let message_opt: Option<String> = params.message;

            // Route through the bounded channel when available; fall back to the
            // legacy direct-spawn path (unit-test mode, no channel started yet).
            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_PROGRESS_CBS,
                    build_ev: Box::new(move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "progress")?;
                        ev.set("server", server_for_task)?;
                        ev.set("token", token_str.as_str())?;
                        ev.set("progress", progress_f64)?;
                        if let Some(t) = total_opt {
                            ev.set("total", t)?;
                        }
                        if let Some(ref m) = message_opt {
                            ev.set("message", m.as_str())?;
                        }
                        Ok(ev)
                    }),
                    caller: "on_progress",
                };
                if let Err(e) = tx.try_send(item) {
                    // Channel full: drop this notification and warn.
                    tracing::warn!(
                        target: "mcp_client",
                        error = %e,
                        "on_progress: notification channel full, dropping notification \
                         (server is emitting faster than Lua can consume)"
                    );
                }
            } else {
                // Fallback: legacy unbounded spawn (unit-test mode / no channel).
                isle_dispatch(
                    main_isle,
                    server_name,
                    MCP_USER_PROGRESS_CBS,
                    move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "progress")?;
                        ev.set("server", server_for_task)?;
                        ev.set("token", token_str.as_str())?;
                        ev.set("progress", progress_f64)?;
                        if let Some(t) = total_opt {
                            ev.set("total", t)?;
                        }
                        if let Some(ref m) = message_opt {
                            ev.set("message", m.as_str())?;
                        }
                        Ok(ev)
                    },
                    "on_progress",
                );
            }
        }
    }

    fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let main_isle = self.main_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name = self.server_name.clone();
        let notify_tx = self.notify_tx.clone();

        async move {
            let level = &params.level;
            let logger = params.logger.as_deref().unwrap_or("").to_string();
            // Serialize data as JSON string for Lua.
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
            }
            .to_string();

            // Save name string early so we can use it after the optional move.
            let sn_str = server_name.as_deref().unwrap_or("unknown").to_string();

            // Check if a Lua handler is registered for this server.
            let has_lua_handler = server_name.as_deref().is_some_and(|sn| {
                registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(sn)
                    .is_some_and(|r| r.on_log)
            });

            if has_lua_handler {
                if let (Some(isle), Some(sn)) = (main_isle, server_name) {
                    let level_task = level_str.clone();
                    let logger_task = logger.clone();
                    let data_task = data_str.clone();

                    if let Some(tx) = notify_tx {
                        let item = NotificationItem {
                            isle,
                            server_name: sn,
                            cbs_table: MCP_USER_LOG_CBS,
                            build_ev: Box::new(move |lua, server_for_task| {
                                let ev = lua.create_table()?;
                                ev.set("type", "log")?;
                                ev.set("server", server_for_task)?;
                                ev.set("level", level_task.as_str())?;
                                ev.set("logger", logger_task.as_str())?;
                                ev.set("data", data_task.as_str())?;
                                Ok(ev)
                            }),
                            caller: "on_logging_message",
                        };
                        if let Err(e) = tx.try_send(item) {
                            tracing::warn!(
                                target: "mcp_client",
                                error = %e,
                                "on_logging_message: notification channel full, dropping notification"
                            );
                        }
                    } else {
                        // Fallback: legacy unbounded spawn (unit-test mode / no channel).
                        isle_dispatch(
                            isle,
                            sn,
                            MCP_USER_LOG_CBS,
                            move |lua, server_for_task| {
                                let ev = lua.create_table()?;
                                ev.set("type", "log")?;
                                ev.set("server", server_for_task)?;
                                ev.set("level", level_task.as_str())?;
                                ev.set("logger", logger_task.as_str())?;
                                ev.set("data", data_task.as_str())?;
                                Ok(ev)
                            },
                            "on_logging_message",
                        );
                    }
                    return;
                }
            }

            // No Lua handler or no Isle — emit directly via tracing to "lua" target
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

    fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let main_isle = self.main_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name_opt = self.server_name.clone();
        let notify_tx = self.notify_tx.clone();

        async move {
            let main_isle = match main_isle {
                Some(i) => i,
                None => return,
            };
            let server_name = match server_name_opt {
                Some(s) => s,
                None => return,
            };
            let has_cb = {
                let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .get(&server_name)
                    .is_some_and(|r| r.on_resource_updated)
                // guard dropped here — no await held (K-4)
            };
            if !has_cb {
                return;
            }

            let uri = params.uri.clone();

            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_RESOURCE_UPDATE_CBS,
                    build_ev: Box::new(move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "resource_update")?;
                        ev.set("server", server_for_task)?;
                        ev.set("uri", uri.as_str())?;
                        Ok(ev)
                    }),
                    caller: "on_resource_updated",
                };
                if let Err(e) = tx.try_send(item) {
                    tracing::warn!(
                        target: "mcp_client",
                        error = %e,
                        "on_resource_updated: notification channel full, dropping notification \
                         (server is emitting faster than Lua can consume)"
                    );
                }
            } else {
                isle_dispatch(
                    main_isle,
                    server_name,
                    MCP_USER_RESOURCE_UPDATE_CBS,
                    move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "resource_update")?;
                        ev.set("server", server_for_task)?;
                        ev.set("uri", uri.as_str())?;
                        Ok(ev)
                    },
                    "on_resource_updated",
                );
            }
        }
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let main_isle = self.main_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name_opt = self.server_name.clone();
        let notify_tx = self.notify_tx.clone();

        async move {
            let main_isle = match main_isle {
                Some(i) => i,
                None => return,
            };
            let server_name = match server_name_opt {
                Some(s) => s,
                None => return,
            };
            let has_cb = {
                let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .get(&server_name)
                    .is_some_and(|r| r.on_resource_list_changed)
                // guard dropped here — no await held (K-4)
            };
            if !has_cb {
                return;
            }

            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_RESOURCES_LIST_CHANGED_CBS,
                    build_ev: Box::new(move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "resources_list_changed")?;
                        ev.set("server", server_for_task)?;
                        Ok(ev)
                    }),
                    caller: "on_resource_list_changed",
                };
                if let Err(e) = tx.try_send(item) {
                    tracing::warn!(
                        target: "mcp_client",
                        error = %e,
                        "on_resource_list_changed: notification channel full, dropping notification"
                    );
                }
            } else {
                isle_dispatch(
                    main_isle,
                    server_name,
                    MCP_USER_RESOURCES_LIST_CHANGED_CBS,
                    move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "resources_list_changed")?;
                        ev.set("server", server_for_task)?;
                        Ok(ev)
                    },
                    "on_resource_list_changed",
                );
            }
        }
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let main_isle = self.main_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name_opt = self.server_name.clone();
        let notify_tx = self.notify_tx.clone();

        async move {
            let main_isle = match main_isle {
                Some(i) => i,
                None => return,
            };
            let server_name = match server_name_opt {
                Some(s) => s,
                None => return,
            };
            let has_cb = {
                let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .get(&server_name)
                    .is_some_and(|r| r.on_tool_list_changed)
                // guard dropped here — no await held (K-4)
            };
            if !has_cb {
                return;
            }

            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_TOOLS_LIST_CHANGED_CBS,
                    build_ev: Box::new(move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "tools_list_changed")?;
                        ev.set("server", server_for_task)?;
                        Ok(ev)
                    }),
                    caller: "on_tool_list_changed",
                };
                if let Err(e) = tx.try_send(item) {
                    tracing::warn!(
                        target: "mcp_client",
                        error = %e,
                        "on_tool_list_changed: notification channel full, dropping notification"
                    );
                }
            } else {
                isle_dispatch(
                    main_isle,
                    server_name,
                    MCP_USER_TOOLS_LIST_CHANGED_CBS,
                    move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "tools_list_changed")?;
                        ev.set("server", server_for_task)?;
                        Ok(ev)
                    },
                    "on_tool_list_changed",
                );
            }
        }
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let main_isle = self.main_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name_opt = self.server_name.clone();
        let notify_tx = self.notify_tx.clone();

        async move {
            let main_isle = match main_isle {
                Some(i) => i,
                None => return,
            };
            let server_name = match server_name_opt {
                Some(s) => s,
                None => return,
            };
            let has_cb = {
                let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .get(&server_name)
                    .is_some_and(|r| r.on_prompt_list_changed)
                // guard dropped here — no await held (K-4)
            };
            if !has_cb {
                return;
            }

            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_PROMPTS_LIST_CHANGED_CBS,
                    build_ev: Box::new(move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "prompts_list_changed")?;
                        ev.set("server", server_for_task)?;
                        Ok(ev)
                    }),
                    caller: "on_prompt_list_changed",
                };
                if let Err(e) = tx.try_send(item) {
                    tracing::warn!(
                        target: "mcp_client",
                        error = %e,
                        "on_prompt_list_changed: notification channel full, dropping notification"
                    );
                }
            } else {
                isle_dispatch(
                    main_isle,
                    server_name,
                    MCP_USER_PROMPTS_LIST_CHANGED_CBS,
                    move |lua, server_for_task| {
                        let ev = lua.create_table()?;
                        ev.set("type", "prompts_list_changed")?;
                        ev.set("server", server_for_task)?;
                        Ok(ev)
                    },
                    "on_prompt_list_changed",
                );
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
                            let json_val = crate::lua_json::lua_to_json(lua, LuaValue::Table(tbl))
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

    /// Handle an inbound `roots/list` request that arrives from the MCP server.
    ///
    /// The server sends `roots/list` to ask the client which filesystem roots are
    /// available. This is a **server→client** request; the implementation looks up
    /// the Lua callback registered via `mcp.set_roots_handler` and returns its
    /// result.
    ///
    /// # Returns
    /// - `Ok(ListRootsResult)` containing the roots the Lua handler returned.
    /// - `Err(McpError::method_not_found)` when no server name is wired, no roots
    ///   handler is registered, or no handler Isle is available.
    /// - `Err(McpError::internal_error)` when the handler Isle exec fails or the
    ///   Lua result cannot be parsed.
    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::ListRootsResult, McpError>> + Send + '_
    {
        let isle = self.handler_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name = self.server_name.clone();

        async move {
            // If no server_name wired, fall through to method_not_found.
            let sn = match server_name.as_deref() {
                Some(s) => s.to_string(),
                None => {
                    return Err(McpError::method_not_found::<
                        rmcp::model::ListRootsRequestMethod,
                    >());
                }
            };

            // Check if roots handler is registered for this server.
            let has_roots = {
                let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(&sn).is_some_and(|r| r.roots)
            };

            if !has_roots {
                return Err(McpError::method_not_found::<
                    rmcp::model::ListRootsRequestMethod,
                >());
            }

            let isle = match isle {
                Some(i) => i,
                None => {
                    return Err(McpError::method_not_found::<
                        rmcp::model::ListRootsRequestMethod,
                    >());
                }
            };

            // Dispatch to Lua roots handler and await result.
            let sn_task = sn.clone();
            let result_val = isle
                .exec(move |lua| {
                    use mlua::prelude::*;
                    let dispatch: LuaFunction =
                        lua.globals().get(MCP_DISPATCH_ROOTS).map_err(|e| {
                            mlua_isle::IsleError::Lua(format!("list_roots: get dispatcher: {e}"))
                        })?;
                    let result: LuaValue = dispatch.call(sn_task.as_str()).map_err(|e| {
                        mlua_isle::IsleError::Lua(format!("list_roots: dispatch: {e}"))
                    })?;

                    // Lua handler must return a table or nil.
                    match result {
                        LuaValue::Nil => Ok(String::new()),
                        LuaValue::Table(tbl) => {
                            // Serialize the table to JSON string.
                            let json_val = crate::lua_json::lua_to_json(lua, LuaValue::Table(tbl))
                                .map_err(|e| {
                                    mlua_isle::IsleError::Lua(format!(
                                        "list_roots: lua_to_json: {e}"
                                    ))
                                })?;
                            serde_json::to_string(&json_val).map_err(|e| {
                                mlua_isle::IsleError::Lua(format!("list_roots: to_string: {e}"))
                            })
                        }
                        other => Err(mlua_isle::IsleError::Lua(format!(
                            "list_roots: handler must return table or nil, got: {:?}",
                            other.type_name()
                        ))),
                    }
                })
                .await;

            match result_val {
                Err(e) => {
                    tracing::warn!(
                        target: "mcp_client",
                        server = %sn,
                        error = %e,
                        "list_roots: handler isle error"
                    );
                    Err(McpError::internal_error(
                        format!("roots handler: {e}"),
                        None,
                    ))
                }
                Ok(json_str) if json_str.is_empty() => {
                    // Lua returned nil — no handler registered in dispatcher
                    Err(McpError::method_not_found::<
                        rmcp::model::ListRootsRequestMethod,
                    >())
                }
                Ok(json_str) => {
                    // Parse Lua response into Vec<Root>.
                    let v: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
                        McpError::internal_error(format!("roots handler result parse: {e}"), None)
                    })?;

                    // The Lua handler returns an array of {uri, name} tables.
                    let entries = v.as_array().ok_or_else(|| {
                        McpError::internal_error(
                            "roots handler result parse: expected array".to_string(),
                            None,
                        )
                    })?;

                    let mut roots = Vec::with_capacity(entries.len());
                    for entry in entries {
                        let uri = entry
                            .get("uri")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = entry
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string);
                        let root = if let Some(n) = name {
                            rmcp::model::Root::new(uri).with_name(n)
                        } else {
                            rmcp::model::Root::new(uri)
                        };
                        roots.push(root);
                    }
                    Ok(rmcp::model::ListRootsResult::new(roots))
                }
            }
        }
    }

    /// Handle an inbound `elicitation/create` request that arrives from the MCP server.
    ///
    /// The server sends `elicitation/create` to ask the client to gather user input.
    /// This is a **server→client** request. Form variant is dispatched to the Lua
    /// callback registered via `mcp.set_elicitation_handler`; Url variant is always
    /// declined without reaching the Lua layer (crux Form-only dispatch constraint).
    ///
    /// # Returns
    /// - `Ok(CreateElicitationResult { action: Accept, content: Some(json), .. })` on accept.
    /// - `Ok(CreateElicitationResult { action: Decline, .. })` on decline, cancel-as-decline,
    ///   Url variant, or no handler registered (spec neutral — not an error).
    /// - `Ok(CreateElicitationResult { action: Cancel, .. })` on cancel.
    /// - `Err(McpError::method_not_found)` when no server name is wired or no handler Isle
    ///   is available (mirrors list_roots).
    /// - `Err(McpError::internal_error)` when the handler Isle exec fails or the Lua
    ///   result fails 3-action contract validation.
    fn create_elicitation(
        &self,
        request: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateElicitationResult, McpError>> + Send + '_
    {
        let isle = self.handler_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name = self.server_name.clone();

        async move {
            // ── Crux: Form-only dispatch — Url variant never reaches Lua ──────────
            let (message, requested_schema) = match request {
                CreateElicitationRequestParams::UrlElicitationParams { .. } => {
                    return Ok(CreateElicitationResult {
                        action: ElicitationAction::Decline,
                        content: None,
                        meta: None,
                    });
                }
                CreateElicitationRequestParams::FormElicitationParams {
                    message,
                    requested_schema,
                    ..
                } => (message, requested_schema),
            };

            // If no server_name wired, fall through to method_not_found.
            let sn = match server_name.as_deref() {
                Some(s) => s.to_string(),
                None => {
                    return Err(McpError::method_not_found::<ElicitationCreateRequestMethod>());
                }
            };

            // Check if elicitation handler is registered for this server.
            let has_elicitation = {
                let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(&sn).is_some_and(|r| r.elicitation)
            };

            if !has_elicitation {
                // No handler registered — spec neutral Decline (not an error).
                return Ok(CreateElicitationResult {
                    action: ElicitationAction::Decline,
                    content: None,
                    meta: None,
                });
            }

            let isle = match isle {
                Some(i) => i,
                None => {
                    return Err(McpError::method_not_found::<ElicitationCreateRequestMethod>());
                }
            };

            // Serialize schema for Lua (crux schema-to-Lua conversion).
            let schema_json = serde_json::to_string(&requested_schema).map_err(|e| {
                McpError::internal_error(format!("create_elicitation: schema serialize: {e}"), None)
            })?;

            // Dispatch to Lua elicitation handler and await result.
            let sn_task = sn.clone();
            let message_task = message.clone();
            let result_val = isle
                .exec(move |lua| {
                    use mlua::prelude::*;
                    let dispatch: LuaFunction =
                        lua.globals().get(MCP_DISPATCH_ELICITATION).map_err(|e| {
                            mlua_isle::IsleError::Lua(format!(
                                "create_elicitation: get dispatcher: {e}"
                            ))
                        })?;
                    let result: LuaValue = dispatch
                        .call((
                            sn_task.as_str(),
                            message_task.as_str(),
                            schema_json.as_str(),
                        ))
                        .map_err(|e| {
                            mlua_isle::IsleError::Lua(format!("create_elicitation: dispatch: {e}"))
                        })?;

                    // Lua handler must return a table or nil.
                    match result {
                        LuaValue::Nil => Ok(String::new()),
                        LuaValue::Table(tbl) => {
                            // Serialize the table to JSON string.
                            let json_val = crate::lua_json::lua_to_json(lua, LuaValue::Table(tbl))
                                .map_err(|e| {
                                    mlua_isle::IsleError::Lua(format!(
                                        "create_elicitation: lua_to_json: {e}"
                                    ))
                                })?;
                            serde_json::to_string(&json_val).map_err(|e| {
                                mlua_isle::IsleError::Lua(format!(
                                    "create_elicitation: to_string: {e}"
                                ))
                            })
                        }
                        other => Err(mlua_isle::IsleError::Lua(format!(
                            "create_elicitation: handler must return table or nil, got: {:?}",
                            other.type_name()
                        ))),
                    }
                })
                .await;

            match result_val {
                Err(e) => {
                    tracing::warn!(
                        target: "mcp_client",
                        server = %sn,
                        error = %e,
                        "create_elicitation: handler isle error"
                    );
                    Err(McpError::internal_error(
                        format!("elicitation handler: {e}"),
                        None,
                    ))
                }
                Ok(json_str) if json_str.is_empty() => {
                    // Lua returned nil — no handler registered in dispatcher → Decline.
                    Ok(CreateElicitationResult {
                        action: ElicitationAction::Decline,
                        content: None,
                        meta: None,
                    })
                }
                Ok(json_str) => {
                    // ── Crux: 3-action response contract validation ────────────────
                    let v: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
                        McpError::internal_error(
                            format!("elicitation handler result parse: {e}"),
                            None,
                        )
                    })?;

                    let action_str = v
                        .get("action")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            McpError::internal_error(
                                "elicitation handler result: missing or non-string 'action' field"
                                    .to_string(),
                                None,
                            )
                        })?;

                    let content = v.get("content").cloned();

                    match action_str {
                        "accept" => {
                            if content.is_none() {
                                tracing::warn!(
                                    target: "mcp_client",
                                    server = %sn,
                                    "create_elicitation: action=accept but content is nil"
                                );
                                return Err(McpError::internal_error(
                                    "elicitation handler: action=accept but content is nil"
                                        .to_string(),
                                    None,
                                ));
                            }
                            Ok(CreateElicitationResult {
                                action: ElicitationAction::Accept,
                                content,
                                meta: None,
                            })
                        }
                        "decline" => {
                            if content.is_some() {
                                tracing::warn!(
                                    target: "mcp_client",
                                    server = %sn,
                                    "create_elicitation: action=decline but content is non-nil"
                                );
                                return Err(McpError::internal_error(
                                    "elicitation handler: action=decline but content is non-nil"
                                        .to_string(),
                                    None,
                                ));
                            }
                            Ok(CreateElicitationResult {
                                action: ElicitationAction::Decline,
                                content: None,
                                meta: None,
                            })
                        }
                        "cancel" => {
                            if content.is_some() {
                                tracing::warn!(
                                    target: "mcp_client",
                                    server = %sn,
                                    "create_elicitation: action=cancel but content is non-nil"
                                );
                                return Err(McpError::internal_error(
                                    "elicitation handler: action=cancel but content is non-nil"
                                        .to_string(),
                                    None,
                                ));
                            }
                            Ok(CreateElicitationResult {
                                action: ElicitationAction::Cancel,
                                content: None,
                                meta: None,
                            })
                        }
                        other => {
                            tracing::warn!(
                                target: "mcp_client",
                                server = %sn,
                                action = %other,
                                "create_elicitation: unknown action"
                            );
                            Err(McpError::internal_error(
                                format!("elicitation handler: unknown action: {other}"),
                                None,
                            ))
                        }
                    }
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
    fn mark_on_resource_updated_sets_flag() {
        let h = AgentBlockClientHandler::new();
        h.ensure_server("srv");
        h.mark_on_resource_updated("srv");
        let guard = h.registry.lock().unwrap();
        assert!(guard.get("srv").unwrap().on_resource_updated);
    }

    #[test]
    fn mark_on_resource_list_changed_sets_flag() {
        let h = AgentBlockClientHandler::new();
        h.ensure_server("srv");
        h.mark_on_resource_list_changed("srv");
        let guard = h.registry.lock().unwrap();
        assert!(guard.get("srv").unwrap().on_resource_list_changed);
    }

    #[test]
    fn mark_on_tool_list_changed_sets_flag() {
        let h = AgentBlockClientHandler::new();
        h.ensure_server("srv");
        h.mark_on_tool_list_changed("srv");
        let guard = h.registry.lock().unwrap();
        assert!(guard.get("srv").unwrap().on_tool_list_changed);
    }

    #[test]
    fn mark_on_prompt_list_changed_sets_flag() {
        let h = AgentBlockClientHandler::new();
        h.ensure_server("srv");
        h.mark_on_prompt_list_changed("srv");
        let guard = h.registry.lock().unwrap();
        assert!(guard.get("srv").unwrap().on_prompt_list_changed);
    }

    /// Verify that `install_mcp_dispatcher_on_handler_isle` now only installs the
    /// sampling dispatcher (progress/log dispatchers were removed in favour of
    /// main-Isle-direct exec).
    #[test]
    fn install_dispatcher_creates_sampling_globals() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();

        let _: mlua::Table = lua.globals().get(MCP_SAMPLING_HANDLERS).unwrap();
        let _: mlua::Function = lua.globals().get(MCP_DISPATCH_SAMPLING).unwrap();

        // Progress/log dispatcher globals are no longer installed on the handler
        // Isle — they live on the main Isle (via MCP_USER_PROGRESS_CBS /
        // MCP_USER_LOG_CBS) instead.
        let progress_handlers: mlua::Value = lua.globals().get("__mcp_progress_handlers").unwrap();
        assert!(
            matches!(progress_handlers, mlua::Value::Nil),
            "__mcp_progress_handlers must not be installed on handler Isle"
        );
        let log_handlers: mlua::Value = lua.globals().get("__mcp_log_handlers").unwrap();
        assert!(
            matches!(log_handlers, mlua::Value::Nil),
            "__mcp_log_handlers must not be installed on handler Isle"
        );
    }

    /// Verify that user-callback storage tables for progress/log are NOT created
    /// on the handler Isle (they now live on the main Isle).
    #[test]
    fn handler_isle_has_no_user_callback_tables() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();

        let progress_cbs: mlua::Value = lua.globals().get(MCP_USER_PROGRESS_CBS).unwrap();
        assert!(
            matches!(progress_cbs, mlua::Value::Nil),
            "__mcp_user_progress_cbs must not be on handler Isle"
        );
        let log_cbs: mlua::Value = lua.globals().get(MCP_USER_LOG_CBS).unwrap();
        assert!(
            matches!(log_cbs, mlua::Value::Nil),
            "__mcp_user_log_cbs must not be on handler Isle"
        );
    }

    /// Verify that user callbacks stored in `__mcp_user_progress_cbs` on the main
    /// Isle can capture upvalues (the root cause of the original bug).
    #[tokio::test]
    async fn main_isle_progress_cb_preserves_upvalue() {
        use mlua_isle::AsyncIsle;

        let (isle, driver) = AsyncIsle::spawn(|_lua: &mlua::Lua| Ok(()))
            .await
            .expect("AsyncIsle::spawn should succeed");

        // Initialise the callback table and register a closure that captures
        // a local counter — mirroring what `mcp.on_progress` does on main Isle.
        isle.exec(|lua| {
            lua.load(
                r#"
                __mcp_user_progress_cbs = {}
                local hits = 0
                __mcp_user_progress_cbs["test-srv"] = function(ev)
                    hits = hits + 1
                end
                _G.get_hits = function() return hits end
            "#,
            )
            .exec()
            .map_err(|e| mlua_isle::IsleError::Lua(format!("setup: {e}")))?;
            Ok(String::new())
        })
        .await
        .expect("setup exec");

        // Simulate three on_progress dispatches (as on_progress handler does).
        for _ in 0..3 {
            isle.exec(|lua| {
                use mlua::prelude::*;
                let cbs: LuaTable = lua
                    .globals()
                    .get(MCP_USER_PROGRESS_CBS)
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("get cbs: {e}")))?;
                let cb: LuaFunction = cbs
                    .get("test-srv")
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("get cb: {e}")))?;
                let ev = lua
                    .create_table()
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("create ev: {e}")))?;
                let _ = cb.call::<()>(ev);
                Ok(String::new())
            })
            .await
            .expect("dispatch exec");
        }

        // Verify the upvalue was incremented 3 times.
        let hits_str = isle
            .exec(|lua| {
                use mlua::prelude::*;
                let get_hits: LuaFunction = lua
                    .globals()
                    .get("get_hits")
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("get_hits: {e}")))?;
                let n: i64 = get_hits
                    .call(())
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("call get_hits: {e}")))?;
                Ok(n.to_string())
            })
            .await
            .expect("read hits exec");
        let hits: i64 = hits_str.parse().expect("hits must be integer");
        assert_eq!(hits, 3, "upvalue counter must reach 3");

        driver.shutdown().await.expect("shutdown");
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
