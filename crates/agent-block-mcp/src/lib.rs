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
//! # Child environment
//!
//! Stdio servers are spawned as child processes and inherit the host's
//! environment, minus the host's *own* credential variables
//! ([`agent_block_types::creds::OWN_CREDENTIAL_ENV_VARS`]), which
//! [`McpManager::connect`] removes — the same set `sh.exec` strips. A server
//! that legitimately needs one of those keys must be handed it explicitly via
//! the `env` argument (`mcp.connect(name, cmd, args, { env = {...} })` from
//! Lua), which is applied after the removals.
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
#[cfg(feature = "mcp-http")]
pub(crate) mod http;
pub mod lua_json;

use std::collections::HashMap;
use std::path::Path;
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

use agent_block_types::creds::OWN_CREDENTIAL_ENV_VARS;
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
    ///
    /// `cwd`: if `Some`, the spawned subprocess inherits this as its current
    /// working directory; if `None`, the subprocess inherits the parent
    /// process's CWD. Callers driven through `agent-block-core` typically
    /// pass `BlockConfig.project_root` so the MCP server sees the same
    /// project root as the Lua script (matters for servers that rely on
    /// path-based discovery such as `git rev-parse --show-toplevel`).
    ///
    /// `env`: variables explicitly injected into the child, applied **after**
    /// the host's own credentials ([`OWN_CREDENTIAL_ENV_VARS`]) are removed.
    /// Ordinary variables are still inherited from the parent — this is an
    /// explicit-injection escape hatch, not an allowlist, and it is the only
    /// way to hand a stripped variable to a server that genuinely needs it.
    pub async fn connect(
        &mut self,
        name: &str,
        command: &str,
        args: &[String],
        trace_context: bool,
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> BlockResult<()> {
        let mut cmd = Command::new(command);
        cmd.args(args).stderr(Stdio::inherit());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // Strip the host's own credentials first, then apply caller-supplied
        // injections so an explicit `env` entry can re-provide a stripped var.
        for var in OWN_CREDENTIAL_ENV_VARS {
            cmd.env_remove(var);
        }
        for (key, value) in env {
            cmd.env(key, value);
        }
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
    ///
    /// Only available when the `mcp-http` feature is enabled (on by default).
    #[cfg(feature = "mcp-http")]
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
    /// Uses `send_request(ClientRequest::PingRequest(...))` — rmcp has no
    /// dedicated client-side `Peer::ping()` method.  Latency is measured with
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
    // resources/subscribe is legacy-only under protocol 2026-07-28; kept for the
    // deprecation window (migration target: Peer::listen / Subscription).
    #[allow(deprecated)]
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
    // resources/unsubscribe is legacy-only under protocol 2026-07-28; kept for the
    // deprecation window (migration target: cancel the Subscription handle).
    #[allow(deprecated)]
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
            ArgumentInfo::new(arg_name.to_string(), arg_value.to_string()),
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
            let notification = CancelledNotification::new(CancelledNotificationParam::new(
                Some(NumberOrString::Number(id)),
                Some("cancelled".to_owned()),
            ));
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

// Unit tests live in sibling files to keep this module focused on
// production code. They remain in-crate (not `tests/*.rs` integration
// tests) because they exercise crate-internal surface such as
// `handler.registry`, `handler.server_name`, and the `mark_*` helpers.
// Each module stays a direct child of the crate root, so `use super::*`
// inside the moved files resolves identically to the former inline form.
#[cfg(test)]
mod concurrency_tests;
#[cfg(test)]
mod rich_tests;
#[cfg(test)]
mod tests;
