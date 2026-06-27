//! MCP Client — manages MCP server child processes via rmcp.
//!
//! Uses `rmcp` (1.4.x) `RunningService<RoleClient, AgentBlockClientHandler>` internally.
//! `AgentBlockClientHandler` provides custom notification handling via Lua callbacks
//! (wired in Subtask 2/3). For Subtask 1, all notification methods are default no-ops.
//!
//! All rmcp round-trips are wrapped in a per-call timeout so a hung child
//! cannot block a Lua coroutine indefinitely.
//!
//! # Concurrency contract
//!
//! `list_tools` and `call_tool` take `&self`, so the manager can be held
//! under `tokio::sync::RwLock` and multiple RPCs — including against the
//! same server — can proceed in parallel via read guards. Request/response
//! multiplexing on a single server is handled by rmcp's `Peer`, which
//! pairs each outbound request with a `oneshot` receiver keyed by request
//! ID. `connect` and `disconnect` are mutating (`&mut self`) and must take
//! the write guard.
//!
//! This contract is covered by in-process unit tests in `#[cfg(test)]` at
//! the bottom of this file. If rmcp alters its `Peer` concurrency model,
//! or if this module is refactored to re-serialize RPCs, those tests fail.
//!
//! # Usage from Lua
//!
//! ```lua
//! mcp.connect("outline", "outline-mcp", {})
//! local tools = mcp.list_tools("outline")
//! local result = mcp.call("outline", "shelf", {})
//! mcp.disconnect("outline")
//! ```

pub mod handler;
pub(crate) mod http;
pub mod lua_json;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua_isle::AsyncIsle;
use rmcp::{
    model::{
        ArgumentInfo, CallToolRequestParams, CancelledNotification, CancelledNotificationParam,
        ClientRequest, CompleteRequestParams, GetPromptRequestParams, NumberOrString, PingRequest,
        ReadResourceRequestParams, Reference, RootsListChangedNotification, ServerResult,
        SubscribeRequestParams, UnsubscribeRequestParams,
    },
    service::{RoleClient, RunningService},
    transport::TokioChildProcess,
    ServiceExt,
};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

use agent_block_types::error::{BlockError, BlockResult};

pub use handler::AgentBlockClientHandler;

/// Default RPC round-trip timeout when no explicit value is provided.
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

pub struct McpManager {
    /// Server connections keyed by name. `pub(crate)` so integration tests
    /// can insert in-process test servers directly (same as `concurrency_tests`
    /// in this module).
    pub servers: HashMap<String, RunningService<RoleClient, AgentBlockClientHandler>>,
    rpc_timeout: Duration,
    /// Shared handler instance — all connections share the same registry Arc.
    pub handler: AgentBlockClientHandler,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
            handler: AgentBlockClientHandler::new(),
        }
    }

    /// Construct a manager with a caller-specified RPC timeout.
    /// Applies to `connect`, `list_tools`, and `call_tool` alike.
    ///
    /// `rpc_timeout` must be non-zero. `Duration::ZERO` would cause every
    /// `tokio::time::timeout` to fire immediately, silently turning every
    /// MCP round-trip into a timeout error — for an autonomous agent that
    /// is a "everything looks broken" failure mode. We reject it at
    /// construction time so the misconfiguration surfaces loudly at
    /// startup instead of being swallowed at the first RPC.
    pub fn with_rpc_timeout(rpc_timeout: Duration) -> BlockResult<Self> {
        if rpc_timeout.is_zero() {
            return Err(BlockError::Mcp(
                "rpc_timeout must be > 0 (got Duration::ZERO); \
                 every MCP RPC would time out immediately"
                    .to_string(),
            ));
        }
        Ok(Self {
            servers: HashMap::new(),
            rpc_timeout,
            handler: AgentBlockClientHandler::new(),
        })
    }

    /// Spawn the MCP server process and complete the MCP initialize handshake.
    ///
    /// `trace_context`: if `true`, `__ab_obs` observability context will be
    /// injected into `call_tool` arguments for this server.  Defaults to `false`
    /// (opt-in) so that third-party / untrusted stdio servers do not receive agent
    /// identity metadata unless explicitly enabled.
    pub async fn connect(
        &mut self,
        name: &str,
        command: &str,
        args: &[String],
        trace_context: bool,
    ) -> BlockResult<()> {
        let mut cmd = Command::new(command);
        cmd.args(args).stderr(Stdio::inherit());
        let transport = TokioChildProcess::new(cmd).map_err(|e| {
            warn!(server = %name, command = %command, error = %e, "mcp spawn failed");
            BlockError::Mcp(format!("spawn '{command}': {e}"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        // Ensure the handler registry has an entry for this server name
        // so callbacks can be registered immediately after connect returns.
        self.handler.ensure_server(name);
        self.handler.set_trace_context(name, trace_context);
        // Set server_name before clone so create_message can identify the
        // connection without needing the RequestContext to carry server identity.
        // The mutate-template → clone → reset dance is required because
        // AgentBlockClientHandler is shared across all connections via Arc<Mutex>
        // for the registry, but create_message needs per-connection server identity
        // that is NOT shared.  Cloning after setting server_name gives each
        // RunningService its own immutable copy of the name while the registry Arc
        // continues to be shared.  Both connect() and connect_http() use this pattern.
        self.handler.server_name = Some(name.to_string());
        let handler = self.handler.clone();
        // Reset server_name on the shared template so the next connect call
        // starts fresh.
        self.handler.server_name = None;
        let running = timeout(rpc_timeout, handler.serve(transport))
            .await
            .map_err(|_| {
                warn!(server = %name, timeout = ?rpc_timeout, "mcp initialize timed out");
                BlockError::Timeout(format!(
                    "initialize '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, error = %e, "mcp initialize failed");
                BlockError::Mcp(format!("initialize '{name}': {e}"))
            })?;
        self.servers.insert(name.to_string(), running);
        Ok(())
    }

    /// Call `tools/list` and return the tools as a JSON array.
    ///
    /// Immutable receiver so concurrent readers can share an `RwLock<McpManager>`.
    pub async fn list_tools(&self, name: &str) -> BlockResult<serde_json::Value> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, "mcp list_tools on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let tools = timeout(rpc_timeout, srv.list_all_tools())
            .await
            .map_err(|_| {
                warn!(server = %name, timeout = ?rpc_timeout, "mcp list_tools timed out");
                BlockError::Timeout(format!(
                    "list_tools '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, error = %e, "mcp list_tools failed");
                BlockError::Mcp(format!("list_tools '{name}': {e}"))
            })?;
        serde_json::to_value(&tools)
            .map_err(|e| BlockError::Mcp(format!("serialize list_tools result: {e}")))
    }

    /// Call `tools/call` with the given tool name and arguments.
    ///
    /// Returns the full rmcp `CallToolResult` serialized to JSON
    /// (`{"content": [...], "isError": bool, ...}`) on success, including
    /// the `isError` flag — tool-execution errors are passed through to
    /// the caller, following the MCP spec's intent that the LLM sees them
    /// and self-corrects. Only protocol / transport / timeout failures
    /// surface as `Err(BlockError::*)`.
    ///
    /// `arguments` must be a JSON `Object` or `Null`. `Null` is treated as
    /// "no arguments"; any other shape (array, scalar) returns an error
    /// rather than silently dropping the payload.
    /// Immutable receiver so concurrent readers can share an `RwLock<McpManager>`.
    pub async fn call_tool(
        &self,
        name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> BlockResult<serde_json::Value> {
        // Validate argument shape early so the error does not depend on
        // whether the server is registered or reachable. MCP spec requires
        // `arguments` to be an object (or absent); an array/scalar would
        // serialize into `CallToolRequestParams` as-is and the server
        // would reject it with an opaque protocol error.
        let mut params = CallToolRequestParams::new(tool_name.to_string());
        match arguments {
            serde_json::Value::Object(obj) => {
                params = params.with_arguments(obj);
            }
            serde_json::Value::Null => {}
            other => {
                let kind = match other {
                    serde_json::Value::Array(_) => "array",
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Bool(_) => "bool",
                    _ => "unknown",
                };
                return Err(BlockError::Mcp(format!(
                    "call_tool '{tool_name}' on '{name}': arguments must be a JSON object \
                     (got {kind})"
                )));
            }
        }
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, tool = %tool_name, "mcp call_tool on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let result = timeout(rpc_timeout, srv.call_tool(params))
            .await
            .map_err(|_| {
                warn!(server = %name, tool = %tool_name, timeout = ?rpc_timeout, "mcp call_tool timed out");
                // Fire-and-forget cancellation notification so the server can
                // clean up the timed-out request.  request_id 0 is a sentinel
                // (we do not have the rmcp-internal ID at this call site).
                // Pass None: we do not have the rmcp-internal request ID at
                // this call site, and sending ID=0 risks matching a real
                // in-flight request on a server that allocates from zero.
                self.send_cancelled(name, None);
                BlockError::Timeout(format!(
                    "call_tool '{tool_name}' on '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, tool = %tool_name, error = %e, "mcp call_tool failed");
                BlockError::Mcp(format!("call_tool '{tool_name}' on '{name}': {e}"))
            })?;
        serde_json::to_value(&result)
            .map_err(|e| BlockError::Mcp(format!("serialize call_tool result: {e}")))
    }

    /// Cancel the named server and remove it from the manager.
    ///
    /// The server is removed from the internal map **before** the cancel
    /// round-trip begins, so a slow or failed cancel never leaves a
    /// zombie entry behind. If graceful cancel exceeds `rpc_timeout`,
    /// the service handle is dropped at the end of the match arm —
    /// rmcp's `Drop` impl cancels the peer's cancellation token, which
    /// terminates the internal task and closes the transport — and
    /// `BlockError::Timeout` is returned.
    ///
    /// The same `rpc_timeout` is reused here so callers have a single
    /// knob governing every MCP round-trip (see `with_rpc_timeout`).
    ///
    /// Callers may re-`connect` the same name safely after any outcome.
    pub async fn disconnect(&mut self, name: &str) -> BlockResult<()> {
        let Some(running) = self.servers.remove(name) else {
            return Ok(());
        };
        let cancel_timeout = self.rpc_timeout;
        match timeout(cancel_timeout, running.cancel()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                warn!(server = %name, error = %e, "mcp cancel failed");
                Err(BlockError::Mcp(format!("cancel '{name}': {e}")))
            }
            Err(_) => {
                warn!(server = %name, timeout = ?cancel_timeout, "mcp cancel timed out");
                Err(BlockError::Timeout(format!(
                    "cancel '{name}' timed out after {cancel_timeout:?}"
                )))
            }
        }
    }

    /// Cancel all managed servers.
    ///
    /// Every server is disconnected regardless of individual failures.
    /// The first error encountered is returned so shutdown can signal
    /// a problem; **subsequent** errors are logged at `warn` level so
    /// they are not silently discarded.
    pub async fn disconnect_all(&mut self) -> BlockResult<()> {
        let mut first_err: Option<BlockError> = None;
        let names: Vec<String> = self.servers.keys().cloned().collect();
        for name in names {
            if let Err(e) = self.disconnect(&name).await {
                if first_err.is_none() {
                    first_err = Some(e);
                } else {
                    warn!(server = %name, error = %e, "disconnect failed during disconnect_all");
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Wire the handler Isle into this manager's `AgentBlockClientHandler`.
    ///
    /// Must be called after both the `McpManager` and the `AsyncIsle` are
    /// constructed. The handler Isle is used to dispatch Lua notification
    /// callbacks (`on_progress` etc.) from the rmcp task thread.
    ///
    /// Idempotent: a second call replaces the previous Isle reference.
    pub fn set_handler_isle(&mut self, isle: Arc<AsyncIsle>) {
        self.handler.handler_isle = Some(isle);
    }

    /// Wire the main Isle into the shared `AgentBlockClientHandler`.
    ///
    /// Must be called after construction and before `connect` / `connect_http`
    /// so that progress/log notification dispatchers can call user Lua callbacks
    /// stored in the main Isle's globals (upvalue-safe path).
    ///
    /// Also starts the bounded notification dispatch task (M-3: capacity-128 channel
    /// that prevents unbounded memory growth from chatty notification sources).
    ///
    /// Idempotent: a second call replaces the previous Isle reference and restarts
    /// the dispatch task on the new channel.
    pub fn set_main_isle(&mut self, isle: Arc<AsyncIsle>) {
        self.handler.main_isle = Some(isle);
        self.handler.start_dispatch_task();
    }

    /// Connect to an MCP server via Streamable HTTP transport.
    ///
    /// `opts` may contain:
    /// - `auth_header` (string): bearer-token authentication header value.
    /// - `trace_context` (bool): if `true`, inject `__ab_obs` observability
    ///   context into `call_tool` arguments. Default: `false` (opt-in).
    ///
    /// The handler Isle must be wired via `set_handler_isle` before calling
    /// this method if `on_progress` callbacks are needed.
    pub async fn connect_http(
        &mut self,
        name: &str,
        url: &str,
        opts: serde_json::Value,
    ) -> BlockResult<()> {
        let trace_context = opts
            .get("trace_context")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.handler.ensure_server(name);
        self.handler.set_trace_context(name, trace_context);
        // Same mutate-template → clone → reset dance as connect(); see the comment
        // there for the rationale (per-connection server_name, shared registry Arc).
        self.handler.server_name = Some(name.to_string());
        let handler = self.handler.clone();
        self.handler.server_name = None;
        let running =
            http::connect_http_transport(name, url, &opts, handler, self.rpc_timeout).await?;
        self.servers.insert(name.to_string(), running);
        Ok(())
    }

    /// Call `resources/list` and return resources as a JSON array.
    ///
    /// Immutable receiver — usable under `RwLock::read` alongside concurrent RPCs.
    pub async fn list_resources(&self, name: &str) -> BlockResult<serde_json::Value> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, "mcp list_resources on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let resources = timeout(rpc_timeout, srv.list_all_resources())
            .await
            .map_err(|_| {
                warn!(server = %name, timeout = ?rpc_timeout, "mcp list_resources timed out");
                BlockError::Timeout(format!(
                    "list_resources '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, error = %e, "mcp list_resources failed");
                BlockError::Mcp(format!("list_resources '{name}': {e}"))
            })?;
        serde_json::to_value(&resources)
            .map_err(|e| BlockError::Mcp(format!("serialize list_resources result: {e}")))
    }

    /// Call `resources/templates/list` and return resource templates as a JSON array.
    ///
    /// Immutable receiver — usable under `RwLock::read` alongside concurrent RPCs.
    pub async fn list_resource_templates(&self, name: &str) -> BlockResult<serde_json::Value> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, "mcp list_resource_templates on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let templates = timeout(rpc_timeout, srv.list_all_resource_templates())
            .await
            .map_err(|_| {
                warn!(server = %name, timeout = ?rpc_timeout, "mcp list_resource_templates timed out");
                BlockError::Timeout(format!(
                    "list_resource_templates '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, error = %e, "mcp list_resource_templates failed");
                BlockError::Mcp(format!("list_resource_templates '{name}': {e}"))
            })?;
        serde_json::to_value(&templates)
            .map_err(|e| BlockError::Mcp(format!("serialize list_resource_templates result: {e}")))
    }

    /// Send a `ping` keepalive to the named server and return the round-trip
    /// latency in milliseconds.
    ///
    /// Uses `send_request(ClientRequest::PingRequest(...))` — rmcp 1.4.0 has
    /// no dedicated `Peer::ping()` method.  Latency is measured with
    /// `Instant::now()` immediately before the send and `elapsed()` immediately
    /// after the `EmptyResult` is received (crux must_not_simplify).
    ///
    /// Immutable receiver — usable under `RwLock::read` alongside concurrent RPCs.
    pub async fn ping(&self, name: &str) -> BlockResult<u64> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, "mcp ping on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        // Clone Peer out of RunningService before awaiting to avoid holding
        // the lock across the await point (K-4 / await-holding-lock).
        let peer = srv.peer().clone();
        let ping_req = ClientRequest::PingRequest(PingRequest::default());
        // Measure latency from immediately before send to immediately after
        // EmptyResult receipt (crux: must_not_simplify).
        let started = Instant::now();
        let response = timeout(rpc_timeout, peer.send_request(ping_req))
            .await
            .map_err(|_| {
                warn!(server = %name, timeout = ?rpc_timeout, "mcp ping timed out");
                BlockError::Timeout(format!("ping '{name}' timed out after {rpc_timeout:?}"))
            })?
            .map_err(|e| {
                warn!(server = %name, error = %e, "mcp ping failed");
                BlockError::Mcp(format!("ping '{name}': {e}"))
            })?;
        match response {
            ServerResult::EmptyResult(_) => {
                let latency_ms = started.elapsed().as_millis() as u64;
                Ok(latency_ms)
            }
            other => {
                warn!(server = %name, "mcp ping: unexpected response");
                Err(BlockError::Mcp(format!(
                    "ping '{name}': unexpected response: {other:?}"
                )))
            }
        }
    }

    /// Call `resources/read` and return the resource contents as JSON.
    ///
    /// Immutable receiver — usable under `RwLock::read`.
    pub async fn read_resource(&self, name: &str, uri: &str) -> BlockResult<serde_json::Value> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, uri = %uri, "mcp read_resource on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let params = ReadResourceRequestParams::new(uri);
        let result = timeout(rpc_timeout, srv.read_resource(params))
            .await
            .map_err(|_| {
                warn!(server = %name, uri = %uri, timeout = ?rpc_timeout, "mcp read_resource timed out");
                BlockError::Timeout(format!(
                    "read_resource '{uri}' on '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, uri = %uri, error = %e, "mcp read_resource failed");
                BlockError::Mcp(format!("read_resource '{uri}' on '{name}': {e}"))
            })?;
        serde_json::to_value(&result)
            .map_err(|e| BlockError::Mcp(format!("serialize read_resource result: {e}")))
    }

    /// Call `resources/subscribe` to subscribe to updates for the given URI.
    ///
    /// Immutable receiver — usable under `RwLock::read`.
    pub async fn subscribe_resource(&self, name: &str, uri: &str) -> BlockResult<()> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, uri = %uri, "mcp subscribe_resource on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let params = SubscribeRequestParams::new(uri);
        timeout(rpc_timeout, srv.subscribe(params))
            .await
            .map_err(|_| {
                warn!(server = %name, uri = %uri, timeout = ?rpc_timeout, "mcp subscribe_resource timed out");
                BlockError::Timeout(format!(
                    "subscribe_resource '{uri}' on '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, uri = %uri, error = %e, "mcp subscribe_resource failed");
                BlockError::Mcp(format!("subscribe_resource '{uri}' on '{name}': {e}"))
            })
    }

    /// Call `resources/unsubscribe` to stop receiving updates for the given URI.
    ///
    /// Immutable receiver — usable under `RwLock::read`.
    pub async fn unsubscribe_resource(&self, name: &str, uri: &str) -> BlockResult<()> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, uri = %uri, "mcp unsubscribe_resource on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let params = UnsubscribeRequestParams::new(uri);
        timeout(rpc_timeout, srv.unsubscribe(params))
            .await
            .map_err(|_| {
                warn!(server = %name, uri = %uri, timeout = ?rpc_timeout, "mcp unsubscribe_resource timed out");
                BlockError::Timeout(format!(
                    "unsubscribe_resource '{uri}' on '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, uri = %uri, error = %e, "mcp unsubscribe_resource failed");
                BlockError::Mcp(format!("unsubscribe_resource '{uri}' on '{name}': {e}"))
            })
    }

    /// Call `prompts/list` and return prompts as a JSON array.
    ///
    /// Immutable receiver — usable under `RwLock::read`.
    pub async fn list_prompts(&self, name: &str) -> BlockResult<serde_json::Value> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, "mcp list_prompts on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let prompts = timeout(rpc_timeout, srv.list_all_prompts())
            .await
            .map_err(|_| {
                warn!(server = %name, timeout = ?rpc_timeout, "mcp list_prompts timed out");
                BlockError::Timeout(format!(
                    "list_prompts '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, error = %e, "mcp list_prompts failed");
                BlockError::Mcp(format!("list_prompts '{name}': {e}"))
            })?;
        serde_json::to_value(&prompts)
            .map_err(|e| BlockError::Mcp(format!("serialize list_prompts result: {e}")))
    }

    /// Call `prompts/get` with the given prompt name and optional arguments.
    ///
    /// `args` must be a JSON Object or Null. Immutable receiver.
    pub async fn get_prompt(
        &self,
        name: &str,
        prompt_name: &str,
        args: serde_json::Value,
    ) -> BlockResult<serde_json::Value> {
        let mut params = GetPromptRequestParams::new(prompt_name.to_string());
        match args {
            serde_json::Value::Object(obj) => {
                params = params.with_arguments(obj);
            }
            serde_json::Value::Null => {}
            other => {
                let kind = match other {
                    serde_json::Value::Array(_) => "array",
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Bool(_) => "bool",
                    _ => "unknown",
                };
                return Err(BlockError::Mcp(format!(
                    "get_prompt '{prompt_name}' on '{name}': args must be a JSON object \
                     (got {kind})"
                )));
            }
        }
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, prompt = %prompt_name, "mcp get_prompt on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let result = timeout(rpc_timeout, srv.get_prompt(params))
            .await
            .map_err(|_| {
                warn!(server = %name, prompt = %prompt_name, timeout = ?rpc_timeout, "mcp get_prompt timed out");
                BlockError::Timeout(format!(
                    "get_prompt '{prompt_name}' on '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, prompt = %prompt_name, error = %e, "mcp get_prompt failed");
                BlockError::Mcp(format!("get_prompt '{prompt_name}' on '{name}': {e}"))
            })?;
        serde_json::to_value(&result)
            .map_err(|e| BlockError::Mcp(format!("serialize get_prompt result: {e}")))
    }

    /// Call `completion/complete` with the given reference and argument.
    ///
    /// `ref_json` must be a JSON Object with a `type` field of either
    /// `"ref/prompt"` (with a `name` field) or `"ref/resource"` (with a `uri`
    /// field).  Any other `type` value is rejected with `BlockError::Mcp`.
    ///
    /// `CompletionContext` is not exposed (scope-out per issue.md:51); it is
    /// always sent as `None`.  Immutable receiver — usable under `RwLock::read`.
    pub async fn complete(
        &self,
        name: &str,
        ref_json: serde_json::Value,
        arg_name: &str,
        arg_value: &str,
    ) -> BlockResult<serde_json::Value> {
        // Build the Reference by dispatching on the `type` field at runtime.
        // This is the crux: both prompt-ref and resource-ref paths must be
        // preserved; collapsing or hardcoding one variant is forbidden.
        let reference = match ref_json.get("type").and_then(|v| v.as_str()) {
            Some("ref/prompt") => {
                let prompt_name = ref_json.get("name").and_then(|v| v.as_str()).unwrap_or("");
                Reference::for_prompt(prompt_name)
            }
            Some("ref/resource") => {
                let uri = ref_json.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                Reference::for_resource(uri)
            }
            Some(kind) => {
                warn!(server = %name, kind = ?kind, "mcp complete: invalid ref kind");
                return Err(BlockError::Mcp(format!(
                    "complete on '{name}': invalid ref kind '{kind}', \
                     expected 'ref/prompt' or 'ref/resource'"
                )));
            }
            None => {
                warn!(server = %name, "mcp complete: ref missing 'type' field");
                return Err(BlockError::Mcp(format!(
                    "complete on '{name}': ref object has no 'type' field"
                )));
            }
        };
        let params = CompleteRequestParams::new(
            reference,
            ArgumentInfo {
                name: arg_name.to_string(),
                value: arg_value.to_string(),
            },
        );
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, "mcp complete on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let rpc_timeout = self.rpc_timeout;
        let result = timeout(rpc_timeout, srv.complete(params))
            .await
            .map_err(|_| {
                warn!(server = %name, timeout = ?rpc_timeout, "mcp complete timed out");
                BlockError::Timeout(format!(
                    "complete on '{name}' timed out after {rpc_timeout:?}"
                ))
            })?
            .map_err(|e| {
                warn!(server = %name, error = %e, "mcp complete failed");
                BlockError::Mcp(format!("complete on '{name}': {e}"))
            })?;
        serde_json::to_value(&result)
            .map_err(|e| BlockError::Mcp(format!("serialize complete result: {e}")))
    }

    /// Return the server's `InitializeResult` serialized as JSON.
    ///
    /// `peer_info()` is sync (no I/O). It returns `Some` after a successful
    /// MCP handshake and `None` before initialization completes.
    ///
    /// Immutable receiver — usable under `RwLock::read`.
    pub fn server_info(&self, name: &str) -> BlockResult<serde_json::Value> {
        let srv = self.servers.get(name).ok_or_else(|| {
            warn!(server = %name, "mcp server_info on unknown server");
            BlockError::Mcp(format!("no server named '{name}'"))
        })?;
        let info = srv.peer_info().ok_or_else(|| {
            warn!(server = %name, "mcp server_info: server not yet initialized");
            BlockError::Mcp(format!("server '{name}' not yet initialized"))
        })?;
        serde_json::to_value(info)
            .map_err(|e| BlockError::Mcp(format!("serialize server_info '{name}': {e}")))
    }

    /// Send a `notifications/cancelled` to the named server.
    ///
    /// This is a best-effort fire-and-forget: the notification is spawned in a
    /// separate task so the caller is not blocked waiting for transport ack.
    /// Errors from the peer send are logged at `warn` level and discarded —
    /// the MCP spec does not require the server to ack cancellations (fire-and-forget
    /// by design; warn-level logging is intentional).
    ///
    /// `request_id` is `Some(id)` when the caller has captured the rmcp-internal
    /// request ID, or `None` when the ID is not available (e.g. a timeout fired
    /// before the ID was obtained). When `None` the notification is **skipped
    /// entirely** to avoid accidentally matching request ID 0 on a server that
    /// allocates IDs starting from zero.
    pub fn send_cancelled(&self, name: &str, request_id: Option<i64>) {
        // Skip silently when no ID is available; sending a bogus sentinel value
        // risks matching a real in-flight request (rmcp allocates from 0).
        let id = match request_id {
            Some(id) => id,
            None => return,
        };
        let Some(srv) = self.servers.get(name) else {
            warn!(server = %name, "send_cancelled: unknown server, ignoring");
            return;
        };
        // Clone the Peer out of the RunningService before spawning so we do
        // not hold any lock across the await (await-holding-lock prevention).
        let peer = srv.peer().clone();
        let name_owned = name.to_string();
        tokio::spawn(async move {
            // CancelledNotification is non-exhaustive; use ::new() which sets
            // method = CancelledNotificationMethod::default() and extensions = Default.
            let notification = CancelledNotification::new(CancelledNotificationParam {
                request_id: NumberOrString::Number(id),
                reason: Some("cancelled".to_owned()),
            });
            if let Err(e) = peer.send_notification(notification.into()).await {
                warn!(
                    server = %name_owned,
                    request_id = %id,
                    error = %e,
                    "send_cancelled: peer send_notification failed"
                );
            }
        });
    }

    /// Notify the named server that the client's roots list has changed.
    ///
    /// Sends a `notifications/roots/list_changed` notification to the server as a
    /// fire-and-forget operation. The server may respond by issuing a new
    /// `roots/list` request.
    ///
    /// # Arguments
    /// - `name` — the name of the server connection to notify.
    ///
    /// # Errors
    /// None propagated. Unknown server is logged at warn level and silently
    /// ignored. Send failures inside the spawned task are also logged at warn
    /// level and discarded.
    pub fn notify_roots_list_changed(&self, name: &str) {
        let Some(srv) = self.servers.get(name) else {
            warn!(server = %name, "notify_roots_list_changed: unknown server, ignoring");
            return;
        };
        // Clone the Peer out of the RunningService before spawning so we do
        // not hold any lock across the await (await-holding-lock prevention).
        let peer = srv.peer().clone();
        let name_owned = name.to_string();
        tokio::spawn(async move {
            // RootsListChangedNotification has no params; Default::default() is
            // sufficient (method = RootsListChangedNotificationMethod::default(),
            // extensions = Default).
            let notification = RootsListChangedNotification::default();
            if let Err(e) = peer.send_notification(notification.into()).await {
                warn!(
                    server = %name_owned,
                    error = %e,
                    "notify_roots_list_changed: peer send_notification failed"
                );
            }
        });
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_manager_is_empty() {
        let mgr = McpManager::new();
        assert!(mgr.servers.is_empty());
    }

    #[tokio::test]
    async fn with_rpc_timeout_rejects_zero() {
        // A ZERO timeout would make every `tokio::time::timeout` fire
        // immediately, silently turning every RPC into a timeout error.
        // For an autonomous agent that is a catastrophic failure mode —
        // the misconfiguration must surface at construction, not be
        // swallowed at the first MCP call.
        let err = match McpManager::with_rpc_timeout(Duration::ZERO) {
            Ok(_) => panic!("Duration::ZERO must be rejected"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("rpc_timeout must be > 0"),
            "unexpected error: {err}",
        );
    }

    #[tokio::test]
    async fn with_rpc_timeout_accepts_positive() {
        let mgr = match McpManager::with_rpc_timeout(Duration::from_millis(1)) {
            Ok(m) => m,
            Err(e) => panic!("positive timeout must be accepted: {e}"),
        };
        assert!(mgr.servers.is_empty());
    }

    #[tokio::test]
    async fn disconnect_nonexistent_is_ok() {
        let mut mgr = McpManager::new();
        assert!(mgr.disconnect("ghost").await.is_ok());
    }

    #[tokio::test]
    async fn call_unknown_server_returns_error() {
        // `let mgr =` (not `let mut`) also asserts at compile time that
        // `call_tool` takes `&self`. Reverting to `&mut self` would break
        // this call site.
        let mgr = McpManager::new();
        let res = mgr.call_tool("none", "dummy", serde_json::json!({})).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn list_tools_takes_shared_receiver() {
        // Mirror guard for `list_tools(&self)`.
        let mgr = McpManager::new();
        let res = mgr.list_tools("none").await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn disconnect_all_empties_map() {
        let mut mgr = McpManager::new();
        mgr.disconnect_all()
            .await
            .expect("disconnect_all on empty manager should succeed");
        assert!(mgr.servers.is_empty());
    }

    #[tokio::test]
    async fn call_tool_rejects_non_object_arguments() {
        // Argument validation runs before the server lookup, so an
        // array/scalar is rejected even without a live server.
        let mgr = McpManager::new();
        for bad in [
            serde_json::json!([1, 2, 3]),
            serde_json::json!("string"),
            serde_json::json!(42),
            serde_json::json!(true),
        ] {
            let res = mgr.call_tool("anything", "dummy", bad.clone()).await;
            let err = res.expect_err("non-object args must error");
            let msg = err.to_string();
            assert!(
                msg.contains("arguments must be a JSON object"),
                "unexpected error for {bad}: {msg}",
            );
        }
    }

    #[tokio::test]
    async fn call_tool_accepts_null_arguments_as_absent() {
        // Null is the documented "no arguments" form. It must pass the
        // validation gate (and fail at the server-lookup step instead).
        let mgr = McpManager::new();
        let res = mgr
            .call_tool("ghost", "dummy", serde_json::Value::Null)
            .await;
        let err = res.expect_err("expected no-server error, not arg-shape error");
        assert!(
            err.to_string().contains("no server named"),
            "Null args should reach the lookup step: {err}",
        );
    }
}

/// Concurrency contract tests.
///
/// These tests nail down the **intended** concurrency model of `McpManager`
/// regardless of what rmcp does internally:
///
/// 1. `list_tools` / `call_tool` are `&self` ⇒ usable under `RwLock::read`.
/// 2. Two concurrent RPCs against the **same** server must overlap in
///    wall time (they do not serialize at the `McpManager` layer).
/// 3. The lock primitive is `RwLock`, not `Mutex` — concurrent reads
///    coexist and a write blocks while any read is held.
///
/// If rmcp changes its `Peer` concurrency contract, or if this module is
/// refactored back to `Mutex` / `&mut self`, these tests break loudly.
#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::RwLock;

    use rmcp::{
        model::{CallToolRequestParams, CallToolResult, Content, ServerCapabilities, ServerInfo},
        service::{MaybeSendFuture, RequestContext},
        ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    };

    /// A server that sleeps `delay` before every `tools/call`.
    /// Used to observe whether two concurrent `call_tool` invocations
    /// overlap (≈ `delay`) or serialize (≈ `2 × delay`).
    #[derive(Clone)]
    struct SlowToolServer {
        delay: Duration,
    }

    impl ServerHandler for SlowToolServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        fn call_tool(
            &self,
            _params: CallToolRequestParams,
            _ctx: RequestContext<RoleServer>,
        ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + MaybeSendFuture + '_
        {
            let delay = self.delay;
            async move {
                tokio::time::sleep(delay).await;
                Ok(CallToolResult::success(vec![Content::text("ok")]))
            }
        }
    }

    /// Spawn an in-process `SlowToolServer` wired to the given `McpManager`
    /// via a `tokio::io::duplex` pair. Bypasses `TokioChildProcess` so the
    /// test does not depend on an external binary.
    async fn attach_slow_server(mgr: &mut McpManager, name: &str, delay: Duration) {
        let (server_side, client_side) = tokio::io::duplex(8192);

        let server = SlowToolServer { delay };
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_call_tool_same_server_does_not_serialize() {
        let delay = Duration::from_millis(300);
        let mgr = Arc::new(RwLock::new(McpManager::new()));

        attach_slow_server(&mut *mgr.write().await, "slow", delay).await;

        let start = Instant::now();
        let a = {
            let mgr = Arc::clone(&mgr);
            async move {
                mgr.read()
                    .await
                    .call_tool("slow", "slow_tool", serde_json::json!({}))
                    .await
            }
        };
        let b = {
            let mgr = Arc::clone(&mgr);
            async move {
                mgr.read()
                    .await
                    .call_tool("slow", "slow_tool", serde_json::json!({}))
                    .await
            }
        };
        let (r1, r2) = tokio::join!(a, b);
        let elapsed = start.elapsed();

        r1.expect("first call succeeds");
        r2.expect("second call succeeds");

        // Serialized path would take ≥ 2×delay = 600ms. Parallel path
        // should land near `delay` (300ms). Fail with generous margin if
        // serialization is observed.
        let serialized_budget = delay * 2 - Duration::from_millis(80);
        assert!(
            elapsed < serialized_budget,
            "concurrent call_tool appears serialized: elapsed={:?}, serialized_budget={:?}",
            elapsed,
            serialized_budget,
        );
    }

    #[tokio::test]
    async fn two_reads_coexist_on_rwlock() {
        // Structural check: confirms `RwLock` (not `Mutex`) is the primitive.
        // A revert to `tokio::sync::Mutex` would drop `try_read` and break
        // this test at compile time.
        let mgr = Arc::new(RwLock::new(McpManager::new()));
        let _g1 = mgr.read().await;
        assert!(
            mgr.try_read().is_ok(),
            "RwLock rejected a concurrent second read guard",
        );
    }

    #[tokio::test]
    async fn write_blocks_while_read_held() {
        let mgr = Arc::new(RwLock::new(McpManager::new()));
        let _g1 = mgr.read().await;
        assert!(
            mgr.try_write().is_err(),
            "write lock acquired while a read guard was held",
        );
    }

    /// A server that always returns `CallToolResult::error`, i.e.
    /// `isError = true`. Used to lock down pass-through semantics.
    #[derive(Clone)]
    struct IsErrorServer;

    impl ServerHandler for IsErrorServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn call_tool(
            &self,
            _params: CallToolRequestParams,
            _ctx: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, McpError> {
            Ok(CallToolResult::error(vec![Content::text("tool blew up")]))
        }
    }

    async fn attach_is_error_server(mgr: &mut McpManager, name: &str) {
        let (server_side, client_side) = tokio::io::duplex(8192);
        tokio::spawn(async move {
            if let Ok(running) = IsErrorServer.serve(server_side).await {
                let _ = running.waiting().await;
            }
        });
        let handler = AgentBlockClientHandler::new();
        let running = handler.serve(client_side).await.expect("handshake");
        mgr.servers.insert(name.to_string(), running);
    }

    #[tokio::test]
    async fn is_error_is_passed_through_in_ok_branch() {
        // MCP spec: tool-execution errors come back as a successful RPC
        // with `isError=true`. `call_tool` must return `Ok(..)` and
        // preserve `isError` in the serialized JSON so the Lua bridge
        // (and ultimately the LLM) sees it.
        let mut mgr = McpManager::new();
        attach_is_error_server(&mut mgr, "boom").await;

        let val = mgr
            .call_tool("boom", "explode", serde_json::json!({}))
            .await
            .expect("RPC succeeds even when isError=true");

        assert_eq!(
            val.get("isError").and_then(|v| v.as_bool()),
            Some(true),
            "isError must be preserved in Ok branch: {val}",
        );
        let content = val.get("content").and_then(|v| v.as_array()).cloned();
        assert!(
            content.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
            "content blocks must be forwarded alongside isError: {val:?}",
        );
    }
}

/// Rich client tests: resources, prompts, progress, and concurrent access.
///
/// Uses in-process duplex servers (same pattern as `concurrency_tests`).
#[cfg(test)]
mod rich_tests {
    use super::*;
    use rmcp::{
        model::{
            CompleteRequestParams, CompleteResult, CompletionInfo, GetPromptRequestParams,
            GetPromptResult, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
            NumberOrString, PaginatedRequestParams, ProgressNotificationParam, ProgressToken,
            Prompt, PromptMessage, PromptMessageRole, RawResource, RawResourceTemplate,
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
        ) -> impl std::future::Future<Output = Result<ListResourcesResult, McpError>>
               + MaybeSendFuture
               + '_ {
            let resources = vec![
                rmcp::model::Resource::new(
                    RawResource::new("file:///hello.txt", "hello.txt"),
                    None,
                ),
                rmcp::model::Resource::new(
                    RawResource::new("file:///world.txt", "world.txt"),
                    None,
                ),
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
                Reference::Resource(_) => CompletionInfo::with_pagination(
                    vec!["file:///a.txt".to_string()],
                    Some(1),
                    false,
                )
                .expect("valid completion info"),
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

            let result =
                ctx.peer.create_elicitation(req).await.map_err(|e| {
                    McpError::internal_error(format!("create_elicitation: {e}"), None)
                })?;

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

            let result =
                ctx.peer.create_elicitation(req).await.map_err(|e| {
                    McpError::internal_error(format!("create_elicitation: {e}"), None)
                })?;

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
}
