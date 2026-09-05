//! `AgentBlockClientHandler` — custom `ClientHandler` for agent-block MCP clients.
//!
//! Subtask 1: structural skeleton.
//! Subtask 2: `on_progress` wired to `handler_isle` bytecode forwarding.
//! Subtask 3: `on_logging_message` log bridge + `create_message` sampling skeleton.
//!
//! # The VM thread never waits, and the host never drives it synchronously
//!
//! Every callback here — the six notifications, sampling, roots, elicitation —
//! ends in *user* Lua, and user Lua is allowed to await: a `knl` session
//! method, `std.sql`, `std.kv`, `std.ts`, `std.task.sleep`. Those are
//! `create_async_function`s, so they yield.
//!
//! A yield only has somewhere to go if every frame between it and the Isle's
//! coroutine is Lua. `AsyncIsle::exec` hands the VM a *Rust* closure, so a
//! callback invoked from inside one is separated from its coroutine by a
//! C-call boundary and the yield dies there — "attempt to yield across a
//! C-call boundary". That is what these paths used to do.
//!
//! So the host calls named Lua functions through
//! [`AsyncIsle::coroutine_call`] instead. That channel carries `&[&str]` in
//! and one `String` out, which is why the dispatchers the host calls are the
//! `_json` wrappers: they do the encode/decode on the Lua side of the
//! boundary, around the user callback rather than through it. The JSON
//! helpers themselves ([`MCP_JSON_ENCODE`], [`MCP_JSON_DECODE`]) are still
//! Rust functions — they are fine, because neither one is on the stack while
//! the user callback runs.
//!
//! Registration (installing dispatchers, storing a handler in a table) runs
//! no user Lua and stays on `exec`.

// Sampling / Roots / Logging are deprecated upstream (SEP-2577, protocol revision
// 2026-07-28) but remain part of agent-block's Lua-facing handler surface for the
// deprecation window; suppress the blanket rmcp deprecation warnings file-wide.
#![allow(deprecated)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mlua_isle::AsyncIsle;
use rmcp::{
    handler::client::ClientHandler,
    model::{
        CreateMessageRequestParams, CreateMessageResult, ElicitRequestParams, ElicitResult,
        ElicitationAction, ElicitationCreateRequestMethod, LoggingLevel,
        LoggingMessageNotificationParam, ProgressNotificationParam,
        ResourceUpdatedNotificationParam, Role, SamplingMessage, SamplingMessageContentBlock,
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

/// JSON-returning wrapper around [`MCP_DISPATCH_SAMPLING`].
///
/// The host calls *this* one, never the inner dispatcher, because
/// [`AsyncIsle::coroutine_call`] carries strings in and a string out. See
/// the module-level "Why the host calls the `_json` wrappers" note.
const MCP_DISPATCH_SAMPLING_JSON: &str = "__mcp_dispatch_sampling_json";

/// Constant name of the Lua global table used to store per-server roots handlers
/// on the handler Isle.
pub(crate) const MCP_ROOTS_HANDLERS: &str = "__mcp_roots_handlers";

/// Constant name of the Lua dispatcher function called for roots/list requests.
const MCP_DISPATCH_ROOTS: &str = "__mcp_dispatch_roots";

/// JSON-returning wrapper around [`MCP_DISPATCH_ROOTS`].
const MCP_DISPATCH_ROOTS_JSON: &str = "__mcp_dispatch_roots_json";

/// Constant name of the Lua global table used to store per-server elicitation handlers
/// on the handler Isle.
pub(crate) const MCP_ELICITATION_HANDLERS: &str = "__mcp_elicitation_handlers";

/// Constant name of the Lua dispatcher function called for elicitation/create requests.
const MCP_DISPATCH_ELICITATION: &str = "__mcp_dispatch_elicitation";

/// JSON-returning wrapper around [`MCP_DISPATCH_ELICITATION`].
const MCP_DISPATCH_ELICITATION_JSON: &str = "__mcp_dispatch_elicitation_json";

/// Name of the Rust-backed `value -> json string` helper installed on the
/// **handler Isle** next to the `_json` wrappers.
///
/// It is a C function, but it only runs *after* the user callback has
/// returned, so it never sits between a yield and the coroutine that would
/// catch it.
const MCP_JSON_ENCODE: &str = "__mcp_json_encode";

/// Name of the Rust-backed `json string -> value` helper installed on the
/// **main Isle** next to [`MCP_DISPATCH_NOTIFY`].
///
/// Same reasoning as [`MCP_JSON_ENCODE`], mirrored: it runs *before* the
/// user callback is entered.
const MCP_JSON_DECODE: &str = "__mcp_json_decode";

/// Name of the Lua dispatcher that delivers one notification to a user
/// callback on the **main Isle**.
///
/// Written in Lua rather than as a Rust closure so that a callback which
/// awaits an async battery (`knl` session methods, `std.sql`, `std.kv`,
/// `std.ts`, `std.task.sleep`) yields through pure Lua frames into the
/// coroutine the Isle created for it. A Rust closure invoked through
/// `AsyncIsle::exec` would put a C-call boundary in that path and the yield
/// would fail with "attempt to yield across a C-call boundary".
const MCP_DISPATCH_NOTIFY: &str = "__mcp_dispatch_notify";

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

/// A single notification item routed through the bounded dispatch channel.
///
/// Carries everything the dispatch task needs to call the user Lua callback
/// on the main Isle: the server name, the callback table key, the event as a
/// JSON string, and a label for log messages.
///
/// The event travels as JSON rather than as a closure that builds an
/// `mlua::Table`, because the dispatch task now reaches Lua through
/// [`AsyncIsle::coroutine_call`] — a `&[&str]`-in, `String`-out channel. The
/// table is built on the Lua side, by [`MCP_DISPATCH_NOTIFY`], where it is no
/// longer between the user callback and its coroutine.
pub(crate) struct NotificationItem {
    pub(crate) isle: Arc<AsyncIsle>,
    pub(crate) server_name: String,
    pub(crate) cbs_table: &'static str,
    pub(crate) ev_json: String,
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
        //
        // One item at a time, awaited to completion before the next is taken
        // off the channel: that is the ordering guarantee a server's
        // notifications had under the old sequential `exec` loop, and it is
        // kept here deliberately. What changed is only *how* Lua is entered —
        // `coroutine_call` instead of `exec` — so a callback that awaits an
        // async battery suspends its own coroutine and lets the rest of the
        // VM run, instead of failing to yield.
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let result = item
                    .isle
                    .coroutine_call(
                        MCP_DISPATCH_NOTIFY,
                        &[
                            item.cbs_table,
                            item.server_name.as_str(),
                            item.ev_json.as_str(),
                        ],
                    )
                    .await;
                if let Err(e) = result {
                    tracing::warn!(
                        target: "mcp_client",
                        server = %item.server_name,
                        caller = %item.caller,
                        error = %e,
                        "notification dispatch: main isle coroutine call failed"
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

    // ── JSON wrappers (what the host actually calls) ───────────────────────────
    //
    // `coroutine_call` returns whatever the function returned, stringified by
    // the Isle — and a table stringifies to `table: 0x…`. So the encode has to
    // happen in Lua, after the handler returns and outside the frames it might
    // yield from. `lua_json::lua_to_json` stays the encoder, reached through
    // `__mcp_json_encode`, so the wire shape is byte-for-byte what the previous
    // `exec`-side conversion produced.
    let encode = lua.create_function(|lua, val: LuaValue| {
        let json = crate::lua_json::lua_to_json(lua, val)?;
        serde_json::to_string(&json).map_err(mlua::Error::external)
    })?;
    lua.globals().set(MCP_JSON_ENCODE, encode)?;

    // `nil` -> "" is the "no handler registered" signal the callers already
    // read; anything that is neither nil nor a table is the handler breaking
    // its contract and is raised as a Lua error.
    let wrappers = [
        (
            MCP_DISPATCH_SAMPLING_JSON,
            MCP_DISPATCH_SAMPLING,
            "create_message",
            2usize,
        ),
        (MCP_DISPATCH_ROOTS_JSON, MCP_DISPATCH_ROOTS, "list_roots", 1),
        (
            MCP_DISPATCH_ELICITATION_JSON,
            MCP_DISPATCH_ELICITATION,
            "create_elicitation",
            3,
        ),
    ];
    for (wrapper_name, inner_name, caller, arity) in wrappers {
        let params = match arity {
            1 => "a",
            2 => "a, b",
            _ => "a, b, c",
        };
        let src = format!(
            r#"
            local INNER = "{inner_name}"
            local ENCODE = "{MCP_JSON_ENCODE}"
            return function({params})
                local r = _G[INNER]({params})
                if r == nil then
                    return ""
                end
                if type(r) ~= "table" then
                    error("{caller}: handler must return table or nil, got: " .. type(r))
                end
                return _G[ENCODE](r)
            end
        "#
        );
        let wrapper: LuaFunction = lua
            .load(&src)
            .set_name(format!("@agent_block:{wrapper_name}"))
            .eval()?;
        lua.globals().set(wrapper_name, wrapper)?;
    }

    Ok(())
}

/// Install the notification dispatcher on the **main Isle**.
///
/// Companion to [`install_mcp_dispatcher_on_handler_isle`], for the other
/// half of the surface: the six `on_*` notifications, whose user callbacks
/// live in the `MCP_USER_*_CBS` tables on the main Isle (stored as closures,
/// so their upvalues survive).
///
/// Installs [`MCP_JSON_DECODE`] (Rust) and [`MCP_DISPATCH_NOTIFY`] (Lua). The
/// dispatch task calls the latter through
/// [`AsyncIsle::coroutine_call`]; see the module doc for why it is Lua.
///
/// Must be called during bridge registration, from inside an
/// `AsyncIsle::exec`. Idempotent.
///
/// The caller (`bridge::mcp::register`) runs on both Isles, so the handler
/// Isle gets a copy too. It is inert there — nothing dispatches
/// notifications against the handler Isle — and it is cheaper to leave it
/// than to thread a "which Isle am I" flag through for two globals.
pub fn install_mcp_notify_dispatcher_on_main_isle(lua: &mlua::Lua) -> mlua::Result<()> {
    use mlua::prelude::*;

    let decode = lua.create_function(|lua, s: String| {
        let json: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
        crate::lua_json::json_to_lua(lua, json)
    })?;
    lua.globals().set(MCP_JSON_DECODE, decode)?;

    // A missing table or a missing callback is not an error: the notification
    // simply has nowhere to go (the server was never wired, or the script
    // unregistered). Returning "" matches what the old `exec` closure did.
    //
    // A callback that raises is *not* swallowed here. The error travels out
    // as `IsleError::Lua` and the dispatch task logs it — same warn, one
    // frame further out — and the loop goes on to the next notification.
    let src = format!(
        r#"
        local DECODE = "{MCP_JSON_DECODE}"
        return function(cbs_name, server_name, ev_json)
            local cbs = _G[cbs_name]
            if type(cbs) ~= "table" then
                return ""
            end
            local cb = cbs[server_name]
            if type(cb) ~= "function" then
                return ""
            end
            cb(_G[DECODE](ev_json))
            return ""
        end
    "#
    );
    let dispatch_notify: LuaFunction = lua
        .load(&src)
        .set_name("@agent_block:__mcp_dispatch_notify")
        .eval()?;
    lua.globals().set(MCP_DISPATCH_NOTIFY, dispatch_notify)?;

    Ok(())
}

/// Build the event JSON for the three `*_list_changed` notifications, which
/// carry nothing but their own type and the server they came from.
fn list_changed_ev_json(ev_type: &str, server_name: &str) -> String {
    let mut ev = serde_json::Map::new();
    ev.insert("type".into(), ev_type.into());
    ev.insert("server".into(), server_name.into());
    serde_json::Value::Object(ev).to_string()
}

/// Dispatch a notification to the Lua callback stored under `cbs_table[server_name]`
/// on the provided main Isle.
///
/// The fallback path, used when no bounded dispatch channel has been started
/// (unit-test mode). Shares [`MCP_DISPATCH_NOTIFY`] with the channel path so
/// the two cannot drift: same lookup, same decode, same yield-capable frames.
///
/// Fire-and-forget — the spawned task is detached and an error is logged, not
/// returned. `create_message` and friends are intentionally out of scope: they
/// have a result the caller waits for.
fn isle_dispatch(
    isle: Arc<AsyncIsle>,
    server_name: String,
    cbs_table: &'static str,
    ev_json: String,
    caller: &'static str,
) {
    tokio::spawn(async move {
        let result = isle
            .coroutine_call(
                MCP_DISPATCH_NOTIFY,
                &[cbs_table, server_name.as_str(), ev_json.as_str()],
            )
            .await;
        if let Err(e) = result {
            tracing::warn!(
                target: "mcp_client",
                server = %server_name,
                error = %e,
                "{}: main isle coroutine call failed",
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

            let mut ev = serde_json::Map::new();
            ev.insert("type".into(), "progress".into());
            ev.insert("server".into(), server_name.as_str().into());
            ev.insert("token".into(), token_str.into());
            ev.insert("progress".into(), serde_json::Value::from(progress_f64));
            if let Some(t) = total_opt {
                ev.insert("total".into(), serde_json::Value::from(t));
            }
            if let Some(m) = message_opt {
                ev.insert("message".into(), m.into());
            }
            let ev_json = serde_json::Value::Object(ev).to_string();

            // Route through the bounded channel when available; fall back to the
            // legacy direct-spawn path (unit-test mode, no channel started yet).
            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_PROGRESS_CBS,
                    ev_json,
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
                    ev_json,
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
                    let mut ev = serde_json::Map::new();
                    ev.insert("type".into(), "log".into());
                    ev.insert("server".into(), sn.as_str().into());
                    ev.insert("level".into(), level_str.as_str().into());
                    ev.insert("logger".into(), logger.as_str().into());
                    ev.insert("data".into(), data_str.as_str().into());
                    let ev_json = serde_json::Value::Object(ev).to_string();

                    if let Some(tx) = notify_tx {
                        let item = NotificationItem {
                            isle,
                            server_name: sn,
                            cbs_table: MCP_USER_LOG_CBS,
                            ev_json,
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
                        isle_dispatch(isle, sn, MCP_USER_LOG_CBS, ev_json, "on_logging_message");
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

            let mut ev = serde_json::Map::new();
            ev.insert("type".into(), "resource_update".into());
            ev.insert("server".into(), server_name.as_str().into());
            ev.insert("uri".into(), uri.into());
            let ev_json = serde_json::Value::Object(ev).to_string();

            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_RESOURCE_UPDATE_CBS,
                    ev_json,
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
                    ev_json,
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

            let ev_json = list_changed_ev_json("resources_list_changed", &server_name);

            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_RESOURCES_LIST_CHANGED_CBS,
                    ev_json,
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
                    ev_json,
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

            let ev_json = list_changed_ev_json("tools_list_changed", &server_name);

            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_TOOLS_LIST_CHANGED_CBS,
                    ev_json,
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
                    ev_json,
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

            let ev_json = list_changed_ev_json("prompts_list_changed", &server_name);

            if let Some(tx) = notify_tx {
                let item = NotificationItem {
                    isle: main_isle,
                    server_name,
                    cbs_table: MCP_USER_PROMPTS_LIST_CHANGED_CBS,
                    ev_json,
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
                    ev_json,
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

            // Dispatch to the Lua sampling handler and await its result JSON.
            //
            // `coroutine_call`, not `exec`: the handler is user Lua and may
            // await an async battery, which only works if there is no C-call
            // boundary between it and the coroutine (module doc).
            let result_json = isle
                .coroutine_call(
                    MCP_DISPATCH_SAMPLING_JSON,
                    &[sn.as_str(), params_json.as_str()],
                )
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
                        SamplingMessage::new(role, SamplingMessageContentBlock::text(content_str));
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

            // Dispatch to the Lua roots handler and await its result (see
            // `create_message` for why this is a coroutine call).
            let result_val = isle
                .coroutine_call(MCP_DISPATCH_ROOTS_JSON, &[sn.as_str()])
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
    /// - `Ok(ElicitResult { action: Accept, content: Some(json), .. })` on accept.
    /// - `Ok(ElicitResult { action: Decline, .. })` on decline, cancel-as-decline,
    ///   Url variant, or no handler registered (spec neutral — not an error).
    /// - `Ok(ElicitResult { action: Cancel, .. })` on cancel.
    /// - `Err(McpError::method_not_found)` when no server name is wired or no handler Isle
    ///   is available (mirrors list_roots).
    /// - `Err(McpError::internal_error)` when the handler Isle exec fails or the Lua
    ///   result fails 3-action contract validation.
    fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<ElicitResult, McpError>> + Send + '_ {
        let isle = self.handler_isle.clone();
        let registry = Arc::clone(&self.registry);
        let server_name = self.server_name.clone();

        async move {
            // ── Crux: Form-only dispatch — Url variant never reaches Lua ──────────
            // ElicitRequestParams is #[non_exhaustive]; unknown future variants take
            // the same spec-neutral Decline path as Url.
            let (message, requested_schema) = match request {
                ElicitRequestParams::FormElicitationParams {
                    message,
                    requested_schema,
                    ..
                } => (message, requested_schema),
                _ => {
                    return Ok(ElicitResult::new(ElicitationAction::Decline));
                }
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
                return Ok(ElicitResult::new(ElicitationAction::Decline));
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

            // Dispatch to the Lua elicitation handler and await its result (see
            // `create_message` for why this is a coroutine call).
            let result_val = isle
                .coroutine_call(
                    MCP_DISPATCH_ELICITATION_JSON,
                    &[sn.as_str(), message.as_str(), schema_json.as_str()],
                )
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
                    Ok(ElicitResult::new(ElicitationAction::Decline))
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
                            match content {
                                None => {
                                    tracing::warn!(
                                        target: "mcp_client",
                                        server = %sn,
                                        "create_elicitation: action=accept but content is nil"
                                    );
                                    Err(McpError::internal_error(
                                        "elicitation handler: action=accept but content is nil"
                                            .to_string(),
                                        None,
                                    ))
                                }
                                Some(content) => Ok(ElicitResult::new(ElicitationAction::Accept)
                                    .with_content(content)),
                            }
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
                            Ok(ElicitResult::new(ElicitationAction::Decline))
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
                            Ok(ElicitResult::new(ElicitationAction::Cancel))
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

    /// The boundary this module is built around, pinned from both sides.
    ///
    /// `park()` stands in for any async battery a user callback may reach for
    /// — a `knl` session method, `std.sql`, `std.ts`, `std.task.sleep`. All of
    /// them are `create_async_function`s, all of them yield.
    ///
    /// Through `exec` the callback is called from a Rust frame and the yield
    /// has nowhere to go. Through `coroutine_call` every frame down to the
    /// callback is Lua and the same callback completes. If someone puts an
    /// `exec` back on the notification path, the second half of this test is
    /// what stops them.
    #[tokio::test]
    async fn a_yielding_callback_needs_the_coroutine_path() {
        use mlua::prelude::*;
        use mlua_isle::AsyncIsle;

        let (isle, driver) = AsyncIsle::spawn(|lua: &mlua::Lua| {
            install_mcp_notify_dispatcher_on_main_isle(lua)?;
            let park = lua.create_async_function(|_, ()| async move {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                Ok(())
            })?;
            lua.globals().set("park", park)?;
            lua.load(
                r#"
                __mcp_user_progress_cbs = {}
                hits = 0
                seen_type = nil
                __mcp_user_progress_cbs["srv"] = function(ev)
                    park()
                    hits = hits + 1
                    seen_type = ev.type
                end
            "#,
            )
            .set_name("@yield_boundary_fixture")
            .exec()?;
            Ok(())
        })
        .await
        .expect("spawn the isle");

        // (1) exec — a Rust frame between the callback and the coroutine.
        let via_exec = isle
            .exec(|lua| {
                let cbs: LuaTable = lua
                    .globals()
                    .get(MCP_USER_PROGRESS_CBS)
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("get cbs: {e}")))?;
                let cb: LuaFunction = cbs
                    .get("srv")
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("get cb: {e}")))?;
                let ev = lua
                    .create_table()
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("create ev: {e}")))?;
                cb.call::<()>(ev)
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("call: {e}")))?;
                Ok(String::new())
            })
            .await;
        let err = via_exec.expect_err("a yield inside an exec closure must fail");
        assert!(
            err.to_string().contains("yield"),
            "expected a yield-boundary error, got: {err}"
        );

        // (2) coroutine_call — only Lua frames, so the same callback lands.
        isle.coroutine_call(
            MCP_DISPATCH_NOTIFY,
            &[
                MCP_USER_PROGRESS_CBS,
                "srv",
                r#"{"type":"progress","server":"srv"}"#,
            ],
        )
        .await
        .expect("the callback must run to completion through the coroutine path");

        let hits = isle
            .eval("return tostring(hits) .. ':' .. tostring(seen_type)")
            .await
            .expect("read back");
        assert_eq!(hits, "1:progress");

        driver.shutdown().await.expect("shutdown");
    }

    /// A notification for a server with no registered callback is not an
    /// error — the dispatcher returns the empty string and the dispatch task
    /// moves on.
    #[tokio::test]
    async fn notify_dispatcher_is_silent_when_no_callback_is_registered() {
        use mlua_isle::AsyncIsle;

        let (isle, driver) = AsyncIsle::spawn(|lua: &mlua::Lua| {
            install_mcp_notify_dispatcher_on_main_isle(lua)?;
            Ok(())
        })
        .await
        .expect("spawn the isle");

        // Table absent entirely.
        let out = isle
            .coroutine_call(
                MCP_DISPATCH_NOTIFY,
                &[MCP_USER_LOG_CBS, "ghost", r#"{"type":"log"}"#],
            )
            .await
            .expect("a missing table is not an error");
        assert_eq!(out, "");

        // Table present, server absent.
        isle.exec(|lua| {
            let t = lua
                .create_table()
                .map_err(|e| mlua_isle::IsleError::Lua(format!("create: {e}")))?;
            lua.globals()
                .set(MCP_USER_LOG_CBS, t)
                .map_err(|e| mlua_isle::IsleError::Lua(format!("set: {e}")))?;
            Ok(String::new())
        })
        .await
        .expect("install the table");

        let out = isle
            .coroutine_call(
                MCP_DISPATCH_NOTIFY,
                &[MCP_USER_LOG_CBS, "ghost", r#"{"type":"log"}"#],
            )
            .await
            .expect("a missing callback is not an error");
        assert_eq!(out, "");

        driver.shutdown().await.expect("shutdown");
    }

    /// The `_json` wrapper the host calls returns the encoded handler result,
    /// `""` for "no handler", and raises when the handler breaks its contract.
    #[test]
    fn sampling_json_wrapper_encodes_nil_table_and_rejects_the_rest() {
        let lua = mlua::Lua::new();
        install_mcp_dispatcher_on_handler_isle(&lua).unwrap();

        let wrapper: mlua::Function = lua.globals().get(MCP_DISPATCH_SAMPLING_JSON).unwrap();

        // No handler registered -> "" (what the caller reads as method_not_found).
        let out: String = wrapper.call(("no-srv", "{}")).unwrap();
        assert_eq!(out, "");

        lua.load(
            r#"
            __mcp_sampling_handlers["srv"] = function(sn, params_json)
                return { model = "test-model", content = "hello" }
            end
            __mcp_sampling_handlers["bad"] = function() return "not a table" end
        "#,
        )
        .exec()
        .unwrap();

        let out: String = wrapper.call(("srv", "{}")).unwrap();
        let got: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(got["model"], "test-model");
        assert_eq!(got["content"], "hello");

        let err = wrapper.call::<String>(("bad", "{}")).unwrap_err();
        assert!(
            err.to_string().contains("must return table or nil"),
            "unexpected error: {err}"
        );
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
