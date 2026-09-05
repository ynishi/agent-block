//! Host — the thin Rust shell that wires up Lua VM, Mesh, HTTP, and MCP.
//!
//! # Responsibilities
//!
//! 1. Spawn an mlua-isle `AsyncIsle` (dedicated Lua VM thread with coroutine support)
//! 2. Optionally connect to agent-mesh relay
//! 3. Initialize the MCP manager for stdio-based MCP server connections
//! 4. Inject all Lua stdlib bridges (`mesh.*`, `http.*`, `sh.*`, `tool.*`, `log.*`, `mcp.*`)
//! 5. Execute the user-provided Lua script via `coroutine_eval` (async-aware)
//! 6. Graceful shutdown (Isle + MCP servers + mesh)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};

use mlua_isle::{AsyncIsle, AsyncIsleDriver};
use tracing::{info, info_span, warn};

use crate::bridge;
use crate::bus::{Event, EventBus, Handler};
use agent_block_mcp::McpManager;
use agent_block_types::error::{BlockError, BlockResult};
use tokio_util::sync::CancellationToken;

/// Embedded Lua sources for the blocks that expose a tool surface.
///
/// Baked into the binary at compile time so `cargo install` works without any
/// extra file distribution. The `require` name on the left is independent of
/// the path on the right: `blocks/` is laid out by role
/// (`agent/` runtime, `tools/`, `lib/`) while callers keep writing
/// `require("compile_loop")`.
const EMBEDDED_BLOCKS: &[(&str, &str)] = &[
    ("agent", include_str!("../blocks/agent/init.lua")),
    (
        "compile_loop",
        include_str!("../blocks/tools/compile_loop/init.lua"),
    ),
    (
        "coding_agent",
        include_str!("../blocks/tools/coding_agent/init.lua"),
    ),
];

/// Embedded Lua support libraries — `require`-able like [`EMBEDDED_BLOCKS`]
/// but not part of the block surface reported by [`inspect_tools`].
///
/// These are required by other modules rather than registered as tools:
/// `llm_proto` is the provider-neutral LLM wire format, `session` persists a
/// messages array through `std.kv`, and `lshape` is a schema validator.
/// Listing them as tools would be misleading.
const EMBEDDED_LIBS: &[(&str, &str)] = &[
    ("session", include_str!("../blocks/lib/session/init.lua")),
    (
        "tool_loop",
        include_str!("../blocks/lib/tool_loop/init.lua"),
    ),
    (
        "llm_proto",
        include_str!("../blocks/lib/llm_proto/init.lua"),
    ),
    (
        "llm_proto.openai",
        include_str!("../blocks/lib/llm_proto/openai.lua"),
    ),
    (
        "llm_proto.anthropic",
        include_str!("../blocks/lib/llm_proto/anthropic.lua"),
    ),
    ("lshape", include_str!("../blocks/lib/lshape/init.lua")),
    ("lshape.t", include_str!("../blocks/lib/lshape/t.lua")),
    (
        "lshape.check",
        include_str!("../blocks/lib/lshape/check.lua"),
    ),
    (
        "lshape.reflect",
        include_str!("../blocks/lib/lshape/reflect.lua"),
    ),
    (
        "lshape.luacats",
        include_str!("../blocks/lib/lshape/luacats.lua"),
    ),
    (
        "mcp_tools",
        include_str!("../blocks/lib/mcp_tools/init.lua"),
    ),
    ("knl", include_str!("../blocks/lib/knl/init.lua")),
    (
        "knl_adapter",
        include_str!("../blocks/lib/knl_adapter/init.lua"),
    ),
    ("policy", include_str!("../blocks/lib/policy/init.lua")),
];

/// Embedded default agent invoker used by [`ScriptSource::DefaultAgent`].
///
/// Runs the StdPkg `agent` module with `_PROMPT` / `_CONTEXT` injected and
/// emits the result on the EventBus. The emit kind is `"_"` — a neutral
/// label with no SDK-side meaning. The result is intended to be received
/// via [`BlockConfig::host_handler`] (the kind-agnostic single sink); the
/// literal label is irrelevant to SDK consumers.
const DEFAULT_AGENT_INVOKER: &str = r#"
local agent = require("agent")
local r = agent.run({
    prompt = _PROMPT,
    system = _CONTEXT,
})
bus.emit("_", r)
"#;

/// How the Lua script source for `run()` is supplied.
///
/// `Path` matches the CLI form (`agent-block -s <path>`), reading from
/// the filesystem at start. `Inline` lets SDK consumers pass a script
/// they hold in memory (compile-time `include_str!`, dynamically built
/// string, etc.) without writing it to a tempfile. `DefaultAgent` uses
/// an embedded invoker that runs the StdPkg `agent` module with the
/// caller-supplied prompt/context and emits the result via
/// `bus.emit("agent_result", ...)`.
#[derive(Debug, Clone)]
pub enum ScriptSource {
    /// Read the script from a filesystem path at start.
    Path(PathBuf),
    /// Use the supplied source code directly.
    Inline {
        /// Lua source code.
        source: String,
        /// Display name used in tracing, error messages, and the Lua
        /// `_SCRIPT_NAME` global (e.g. `"agent_invoker.lua"`).
        name: String,
    },
    /// Use the embedded default agent invoker. `prompt` / `context`
    /// are forwarded as `_PROMPT` / `_CONTEXT` Lua globals and the
    /// agent result is emitted on the EventBus under a neutral label
    /// (`"_"`). SDK consumers should pair this with
    /// [`BlockConfig::host_handler`] (the kind-agnostic single sink)
    /// and `auto_serve_bus = true`. The emit-kind is intentionally
    /// meaningless; consumers that need string-keyed routing should
    /// supply [`ScriptSource::Inline`] with their own invoker.
    DefaultAgent,
}

/// How a string payload (prompt / system context) is supplied.
///
/// `Inline` is the literal string variant (CLI `--prompt` / `--context`).
/// `File` reads the contents from disk at `run()` start (CLI
/// `--prompt-file` / `--context-file`).
#[derive(Debug, Clone)]
pub enum PromptSource {
    /// Literal string.
    Inline(String),
    /// Filesystem path; contents are read at `run()` start.
    File(PathBuf),
}

/// How the Ed25519 mesh identity secret key is supplied.
///
/// `Inline` is a 64-hex literal. `Env` reads the named environment
/// variable at `run()` start (CLI default uses
/// `AGENT_BLOCK_MESH_SECRET_KEY`). Absence of any `SecretKeySource`
/// (i.e. `BlockConfig.secret_key = None`) causes a random keypair to
/// be generated, matching the prior behavior.
#[derive(Debug, Clone)]
pub enum SecretKeySource {
    /// 64-character hex literal.
    Inline(String),
    /// Environment variable name to read at start.
    Env(String),
}

/// Async handler invoked when the LLM (or a Lua call to
/// `tool.call(name, ...)`) targets a Rust-implemented tool supplied via
/// [`BlockConfig::host_tools`].
///
/// `input` arrives as a `serde_json::Value` (converted from Lua before
/// the handler is invoked). The returned value is converted back to a
/// Lua value and delivered to the caller. Errors are propagated as
/// `LuaError::external` (visible inside the script) and as `BlockError`
/// on the Rust side.
#[async_trait::async_trait]
pub trait ToolHandler: Send + Sync + 'static {
    async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value, BlockError>;
}

/// Declarative spec for a Rust-implemented tool injected into the Lua
/// tool registry before the user script runs. The resulting entry is
/// indistinguishable from a Lua-defined tool from the script's view:
/// `tool.call("<name>", input)`, `agent.run({ ... })` tool dispatch,
/// and `tool.schema()` enumeration all work uniformly.
#[derive(Clone)]
pub struct HostToolSpec {
    /// Tool name. Becomes the routing key in `_TOOL_REGISTRY` and the
    /// `name` field exposed by `tool.schema()` (Anthropic tool spec).
    pub name: String,
    /// Free-form description shown to the LLM. Becomes the
    /// `description` field of the Anthropic tool spec.
    pub description: String,
    /// Input schema (Anthropic-compatible JSON Schema object).
    pub input_schema: serde_json::Value,
    /// Optional group label for [`agent.run`'s `tool_groups`] filter
    /// and for [`BlockConfig::tool_policy`] (planned).
    pub group: Option<String>,
    /// Rust callback dispatched on every invocation.
    pub handler: Arc<dyn ToolHandler>,
}

impl std::fmt::Debug for HostToolSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostToolSpec")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .field("group", &self.group)
            .field("handler", &"<dyn ToolHandler>")
            .finish()
    }
}

/// Snapshot of a tool that a given [`BlockConfig`] will (statically)
/// expose to the LLM. Produced by [`inspect_tools`] without running
/// the script. MCP server tools are *not* included because they are
/// only known after the MCP `initialize` handshake completes; callers
/// that need that view should run the script and call `tool.schema()`
/// from Lua.
#[derive(Debug, Clone)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub group: Option<String>,
    pub source: ToolSource,
}

/// Origin of a tool listed by [`inspect_tools`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    /// Supplied via [`BlockConfig::host_tools`] (Rust-implemented).
    HostRust,
    /// Embedded StdPkg block (`agent`, `compile_loop`, …) — discovered
    /// statically from [`EMBEDDED_BLOCKS`]. Note: not every embedded
    /// block exposes a registered tool; this entry simply records that
    /// the module is available via `require(...)`.
    EmbeddedBlock,
}

/// Inspect the tools a [`BlockConfig`] will expose to the LLM without
/// actually running the script. Returns the merged list of
/// `host_tools` (declared in the config) and embedded-block sources.
///
/// MCP server tools are deliberately omitted — they only become known
/// after the MCP `initialize` handshake. Use `tool.schema()` from
/// inside the running script for that view.
pub fn inspect_tools(config: &BlockConfig) -> Vec<ToolMeta> {
    let mut out = Vec::new();
    for t in &config.host_tools {
        out.push(ToolMeta {
            name: t.name.clone(),
            description: t.description.clone(),
            group: t.group.clone(),
            source: ToolSource::HostRust,
        });
    }
    for (name, _src) in EMBEDDED_BLOCKS {
        out.push(ToolMeta {
            name: (*name).to_string(),
            description: format!("Embedded StdPkg block (require(\"{name}\"))"),
            group: None,
            source: ToolSource::EmbeddedBlock,
        });
    }
    out
}

/// Build the `blocks/` portion of `package.path` from filesystem locations.
///
/// Priority (highest first):
/// 1. `project_root/blocks/` — user-customisable, overrides embedded StdPkg
/// 2. `exe_dir/blocks/`      — development hot-reload (next to the binary)
///
/// Returns a semicolon-terminated string ready to prepend to `package.path`,
/// or an empty string when no `blocks/` directories are found.
fn build_blocks_path(project_root: &Path) -> String {
    let mut out = String::new();

    // 1. project_root/blocks/
    let project_blocks = project_root.join("blocks");
    if project_blocks.is_dir() {
        let pb = project_blocks.to_string_lossy();
        out.push_str(&format!("{pb}/?.lua;{pb}/?/init.lua;"));
    }

    // 2. exe_dir/blocks/
    match std::env::current_exe() {
        Ok(exe) => {
            if let Some(exe_dir) = exe.parent() {
                let exe_blocks = exe_dir.join("blocks");
                if exe_blocks.is_dir() {
                    let eb = exe_blocks.to_string_lossy();
                    out.push_str(&format!("{eb}/?.lua;{eb}/?/init.lua;"));
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "current_exe() failed; skipping exe_dir/blocks/ from package.path");
        }
    }

    out
}

/// Filesystem roots that `require` searches, highest priority first.
///
/// Same two locations [`build_blocks_path`] encodes into `package.path`,
/// returned as roots so they can also be handed to `mlua_pkg::FsResolver`
/// (which takes a directory, not a `?.lua` pattern).
fn build_blocks_roots(project_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();

    let project_blocks = project_root.join("blocks");
    if project_blocks.is_dir() {
        out.push(project_blocks);
    }

    match std::env::current_exe() {
        Ok(exe) => {
            if let Some(exe_dir) = exe.parent() {
                let exe_blocks = exe_dir.join("blocks");
                if exe_blocks.is_dir() {
                    out.push(exe_blocks);
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "current_exe() failed; skipping exe_dir/blocks/ from require roots");
        }
    }

    out
}

/// Full configuration for a single [`run`] execution.
///
/// # Construction
///
/// Prefer [`BlockConfig::builder`] over struct-literal construction. The
/// struct is `#[non_exhaustive]`, so crates outside `agent-block-core`
/// cannot build it with a `BlockConfig { .. }` literal; the builder is the
/// supported, forward-compatible path. New fields are added with sensible
/// defaults, so existing builder call sites keep compiling when the config
/// surface grows.
///
/// All fields remain `pub` for reading (`config.project_root`, etc.); only
/// the literal-construction form is gated by `#[non_exhaustive]`.
///
/// ```no_run
/// use agent_block_core::BlockConfig;
/// use agent_block_core::host::ScriptSource;
/// use std::path::PathBuf;
///
/// let config = BlockConfig::builder(
///     ScriptSource::Path(PathBuf::from("agent.lua")),
///     PathBuf::from("."),
/// )
/// .auto_serve_bus(true)
/// .build();
/// ```
#[non_exhaustive]
pub struct BlockConfig {
    /// Lua script to execute. See [`ScriptSource`] for the supported
    /// shapes (filesystem path / inline source / embedded default
    /// agent invoker).
    pub script: ScriptSource,
    pub project_root: PathBuf,
    pub relay_url: Option<String>,
    /// Ed25519 secret key for mesh identity. See [`SecretKeySource`]
    /// for the supported shapes (inline 64-hex / environment variable).
    /// `None` generates a random keypair. Required to talk to
    /// registry/ACL-gated hosted meshes.
    pub secret_key: Option<SecretKeySource>,
    /// Per-RPC timeout for every MCP round-trip (connect / list / call).
    /// Defaults to [`agent_block_mcp::DEFAULT_RPC_TIMEOUT`].
    pub mcp_rpc_timeout: Duration,
    /// Prompt payload injected as `_PROMPT` Lua global. See
    /// [`PromptSource`] for the supported shapes. `None` leaves the
    /// global unset.
    pub prompt: Option<PromptSource>,
    /// Context payload injected as `_CONTEXT` Lua global (typically
    /// the system prompt). Same shape rules as [`Self::prompt`].
    pub context: Option<PromptSource>,
    /// Host-side Rust handlers pre-installed on the EventBus before the user
    /// script starts. Each entry registers `handler` against `kind` via
    /// [`EventBus::on`], so a script-side `bus.emit(kind, payload)` is
    /// captured by the Rust handler rather than dispatched to a Lua function.
    ///
    /// Intended for SDK consumers that embed `agent-block-core` and need to
    /// receive script output programmatically (e.g. a Spawner adapter that
    /// turns LLM script output into a typed `WorkerResult`). Lua-side
    /// `bus.on(kind, fn)` registrations layered on top of the handler Isle
    /// are still possible, but the EventBus dispatches a single handler per
    /// `kind` (last-write-wins), so host-side and Lua-side registrations on
    /// the same `kind` collide; choose one side per routing key.
    ///
    /// Defaults to an empty map (no host handlers).
    pub host_handlers: HashMap<String, Arc<dyn Handler>>,
    /// Single host-side Rust handler that catches every event regardless
    /// of `kind`. Internally registered via [`EventBus::on_any`], so it
    /// acts as a fallback when no entry in [`Self::host_handlers`]
    /// matches the incoming `kind`.
    ///
    /// This is the SDK-embed 1-shot sink: SDK consumers do not need to
    /// invent or coordinate a string `kind` between the Lua script and
    /// their Rust code. The agent invoker's emit-kind is irrelevant —
    /// the handler receives every event.
    ///
    /// Use this when you want a single Rust handler to receive results
    /// (typical embedded use). Use [`Self::host_handlers`] instead when
    /// you actually need string-keyed routing (multi-source / multi-
    /// handler dispatch). The two may coexist: kind-specific handlers
    /// in `host_handlers` take precedence, and this single handler is
    /// the fallback for unmatched kinds.
    ///
    /// Defaults to `None`.
    pub host_handler: Option<Arc<dyn Handler>>,
    /// Rust-implemented tools injected into the Lua tool registry
    /// before the user script runs. Each entry becomes
    /// indistinguishable from a Lua-defined tool: it is discoverable
    /// via `tool.list()` / `tool.schema()`, dispatchable via
    /// `tool.call(name, input)`, and visible to `agent.run`'s LLM
    /// function-calling.
    ///
    /// SDK consumers can use this to expose Rust capabilities
    /// (database lookups, business logic, etc.) to the LLM without
    /// writing any Lua. See [`HostToolSpec`] and [`ToolHandler`].
    ///
    /// Defaults to an empty list.
    pub host_tools: Vec<HostToolSpec>,
    /// Optional custom `reqwest::Client` for the `http.*` Lua bridge
    /// and any other in-process HTTP traffic. SDK consumers can wire
    /// in their own TLS roots, proxy, default headers, connection
    /// pool tuning, etc.
    ///
    /// `None` falls back to `reqwest::Client::new()` with default
    /// settings (legacy behavior).
    pub http_client: Option<reqwest::Client>,
    /// Override path for the `std.sql` SQLite database file. `None`
    /// reads the `AGENT_BLOCK_SQL_PATH` env var (CLI default), or
    /// falls back to `{base_dir}/db.sqlite`. Pass `Some(":memory:")`
    /// for an in-memory DB (useful for tests / isolation).
    pub sql_path: Option<PathBuf>,
    /// Override path for the `std.kv` SQLite database file. Same
    /// semantics as [`Self::sql_path`].
    pub kv_path: Option<PathBuf>,
    /// Override path for the `std.ts` SQLite database file. Same
    /// semantics as [`Self::sql_path`].
    pub ts_path: Option<PathBuf>,
    /// Extra Lua globals injected into both the main Isle and the
    /// handler Isle before the user script runs. Each entry
    /// `(name, value)` results in `_G[name] = json_to_lua(value)`.
    ///
    /// Use this to parameterize an inline script from Rust without
    /// baking the values into the Lua source (`_USER_ID`,
    /// `_TENANT`, `_FEATURE_FLAGS`, etc.). Keys must be valid Lua
    /// identifiers; values are any `serde_json::Value`.
    ///
    /// `_PROMPT`, `_CONTEXT`, and `_SCRIPT_NAME` are reserved
    /// (managed by other `BlockConfig` fields); colliding with them
    /// silently overrides those defaults — use with care.
    pub extra_globals: HashMap<String, serde_json::Value>,
    /// When `true`, the EventBus dispatcher loop is driven in the background
    /// for the duration of the script and shut down gracefully after the
    /// script completes. Required for SDK-embed callers that supply
    /// [`Self::host_handlers`] and need `bus.emit(kind, payload)` events
    /// emitted from the script to actually reach those handlers without
    /// requiring the script to call `bus.serve()` (which blocks on
    /// SIGTERM / Ctrl+C and never returns under programmatic embedding).
    ///
    /// After the script finishes, the dispatcher is given a grace window
    /// (`AGENT_BLOCK_TASK_GRACE_MS`, default 1000ms) to drain queued events
    /// and finish any in-flight handler, then is cancelled.
    ///
    /// Mutually exclusive with Lua-side `bus.serve()`: enabling this flag
    /// takes ownership of the EventBus before the script runs, so a script
    /// that calls `bus.on(...)` followed by `bus.serve()` will error
    /// ("bus.serve() has already taken ownership"). Use this flag when the
    /// script's sole purpose is to push events to host handlers.
    ///
    /// Defaults to `false` (legacy behavior: dispatcher only runs when the
    /// script calls `bus.serve()`).
    pub auto_serve_bus: bool,
    /// Optional caller-supplied cancellation token. When cancelled, the
    /// in-flight script is interrupted via the Isle's debug-hook cancel
    /// path, the auto-serve dispatcher (if any) is shut down, and `run()`
    /// returns `Err(BlockError::Cancelled)`.
    ///
    /// Intended for SDK consumers that spawn `run()` as a tokio task and
    /// need an out-of-band abort signal (timeouts, parent-task cancellation
    /// propagation, user-driven stop). The token is observed across the
    /// `coroutine_eval` await; once cancellation propagates, the shutdown
    /// sequence (MCP disconnect, Isle drivers, auto-serve dispatcher)
    /// still runs so file descriptors and remote handles are released.
    ///
    /// Defaults to `None` (legacy behavior: `run()` only completes when
    /// the script returns naturally).
    pub shutdown_token: Option<CancellationToken>,
}

impl BlockConfig {
    /// Start building a [`BlockConfig`] with the two semantically required
    /// inputs supplied up front.
    ///
    /// `script` selects what Lua source to execute — there is no meaningful
    /// default, since a run has nothing to do without a script. `project_root`
    /// anchors `.env` loading, inline-script directory resolution, and the
    /// default working directory handed to spawned MCP servers.
    ///
    /// Every other field starts at the default documented on the matching
    /// [`BlockConfig`] field and is overridden through the chainable
    /// [`BlockConfigBuilder`] setters. This is the recommended construction
    /// path for SDK embedders: `BlockConfig` is `#[non_exhaustive]`, so
    /// struct-literal construction is unavailable to downstream crates and new
    /// fields can be added without breaking existing builder call sites.
    ///
    /// ```no_run
    /// use agent_block_core::BlockConfig;
    /// use agent_block_core::host::ScriptSource;
    /// use std::path::PathBuf;
    ///
    /// let config = BlockConfig::builder(
    ///     ScriptSource::Path(PathBuf::from("agent.lua")),
    ///     PathBuf::from("."),
    /// )
    /// .auto_serve_bus(true)
    /// .build();
    /// ```
    pub fn builder(script: ScriptSource, project_root: PathBuf) -> BlockConfigBuilder {
        BlockConfigBuilder::new(script, project_root)
    }
}

/// Chainable builder for [`BlockConfig`], created via
/// [`BlockConfig::builder`].
///
/// Each setter returns `self` for fluent chaining. Setters for `Option<_>`
/// fields take the inner value and wrap it in `Some` internally, so callers
/// pass e.g. `.prompt(PromptSource::Inline(..))` rather than an `Option`.
/// Fields left untouched keep the defaults documented on the corresponding
/// [`BlockConfig`] field.
///
/// Because [`BlockConfig`] is `#[non_exhaustive]`, this builder is the only
/// supported way for crates outside `agent-block-core` to construct one, and
/// it stays source-compatible as new config fields are introduced.
pub struct BlockConfigBuilder {
    script: ScriptSource,
    project_root: PathBuf,
    relay_url: Option<String>,
    secret_key: Option<SecretKeySource>,
    mcp_rpc_timeout: Duration,
    prompt: Option<PromptSource>,
    context: Option<PromptSource>,
    host_handlers: HashMap<String, Arc<dyn Handler>>,
    host_handler: Option<Arc<dyn Handler>>,
    host_tools: Vec<HostToolSpec>,
    http_client: Option<reqwest::Client>,
    sql_path: Option<PathBuf>,
    kv_path: Option<PathBuf>,
    ts_path: Option<PathBuf>,
    extra_globals: HashMap<String, serde_json::Value>,
    auto_serve_bus: bool,
    shutdown_token: Option<CancellationToken>,
}

impl BlockConfigBuilder {
    fn new(script: ScriptSource, project_root: PathBuf) -> Self {
        Self {
            script,
            project_root,
            relay_url: None,
            secret_key: None,
            mcp_rpc_timeout: agent_block_mcp::DEFAULT_RPC_TIMEOUT,
            prompt: None,
            context: None,
            host_handlers: HashMap::new(),
            host_handler: None,
            host_tools: Vec::new(),
            http_client: None,
            sql_path: None,
            kv_path: None,
            ts_path: None,
            extra_globals: HashMap::new(),
            auto_serve_bus: false,
            shutdown_token: None,
        }
    }

    /// Override the Lua script to execute (`BlockConfig::script`).
    pub fn script(mut self, script: ScriptSource) -> Self {
        self.script = script;
        self
    }

    /// Override the project root (`BlockConfig::project_root`).
    pub fn project_root(mut self, project_root: impl Into<PathBuf>) -> Self {
        self.project_root = project_root.into();
        self
    }

    /// Set the mesh relay URL (`BlockConfig::relay_url`). Defaults to `None`
    /// (mesh disabled).
    pub fn relay_url(mut self, relay_url: impl Into<String>) -> Self {
        self.relay_url = Some(relay_url.into());
        self
    }

    /// Set the mesh identity secret key source (`BlockConfig::secret_key`).
    /// Defaults to `None` (random keypair).
    pub fn secret_key(mut self, secret_key: SecretKeySource) -> Self {
        self.secret_key = Some(secret_key);
        self
    }

    /// Override the per-RPC MCP timeout (`BlockConfig::mcp_rpc_timeout`).
    /// Defaults to [`agent_block_mcp::DEFAULT_RPC_TIMEOUT`].
    pub fn mcp_rpc_timeout(mut self, mcp_rpc_timeout: Duration) -> Self {
        self.mcp_rpc_timeout = mcp_rpc_timeout;
        self
    }

    /// Set the prompt payload injected as `_PROMPT` (`BlockConfig::prompt`).
    /// Defaults to `None`.
    pub fn prompt(mut self, prompt: PromptSource) -> Self {
        self.prompt = Some(prompt);
        self
    }

    /// Set the context payload injected as `_CONTEXT`
    /// (`BlockConfig::context`). Defaults to `None`.
    pub fn context(mut self, context: PromptSource) -> Self {
        self.context = Some(context);
        self
    }

    /// Set the kind-keyed host-side handlers (`BlockConfig::host_handlers`).
    /// Defaults to an empty map.
    pub fn host_handlers(mut self, host_handlers: HashMap<String, Arc<dyn Handler>>) -> Self {
        self.host_handlers = host_handlers;
        self
    }

    /// Set the kind-agnostic fallback host handler
    /// (`BlockConfig::host_handler`). Defaults to `None`.
    pub fn host_handler(mut self, host_handler: Arc<dyn Handler>) -> Self {
        self.host_handler = Some(host_handler);
        self
    }

    /// Set the Rust-implemented tools injected into the Lua registry
    /// (`BlockConfig::host_tools`). Defaults to an empty list.
    pub fn host_tools(mut self, host_tools: Vec<HostToolSpec>) -> Self {
        self.host_tools = host_tools;
        self
    }

    /// Set a custom `reqwest::Client` for the `http.*` bridge
    /// (`BlockConfig::http_client`). Defaults to `None`.
    pub fn http_client(mut self, http_client: reqwest::Client) -> Self {
        self.http_client = Some(http_client);
        self
    }

    /// Override the `std.sql` database path (`BlockConfig::sql_path`).
    /// Defaults to `None`.
    pub fn sql_path(mut self, sql_path: impl Into<PathBuf>) -> Self {
        self.sql_path = Some(sql_path.into());
        self
    }

    /// Override the `std.kv` database path (`BlockConfig::kv_path`).
    /// Defaults to `None`.
    pub fn kv_path(mut self, kv_path: impl Into<PathBuf>) -> Self {
        self.kv_path = Some(kv_path.into());
        self
    }

    /// Override the `std.ts` database path (`BlockConfig::ts_path`).
    /// Defaults to `None`.
    pub fn ts_path(mut self, ts_path: impl Into<PathBuf>) -> Self {
        self.ts_path = Some(ts_path.into());
        self
    }

    /// Set the extra Lua globals injected before the script runs
    /// (`BlockConfig::extra_globals`). Defaults to an empty map.
    pub fn extra_globals(mut self, extra_globals: HashMap<String, serde_json::Value>) -> Self {
        self.extra_globals = extra_globals;
        self
    }

    /// Enable or disable the background EventBus dispatcher
    /// (`BlockConfig::auto_serve_bus`). Defaults to `false`.
    pub fn auto_serve_bus(mut self, auto_serve_bus: bool) -> Self {
        self.auto_serve_bus = auto_serve_bus;
        self
    }

    /// Set the caller-supplied cancellation token
    /// (`BlockConfig::shutdown_token`). Defaults to `None`.
    pub fn shutdown_token(mut self, shutdown_token: CancellationToken) -> Self {
        self.shutdown_token = Some(shutdown_token);
        self
    }

    /// Finalize the builder into a [`BlockConfig`].
    pub fn build(self) -> BlockConfig {
        BlockConfig {
            script: self.script,
            project_root: self.project_root,
            relay_url: self.relay_url,
            secret_key: self.secret_key,
            mcp_rpc_timeout: self.mcp_rpc_timeout,
            prompt: self.prompt,
            context: self.context,
            host_handlers: self.host_handlers,
            host_handler: self.host_handler,
            host_tools: self.host_tools,
            http_client: self.http_client,
            sql_path: self.sql_path,
            kv_path: self.kv_path,
            ts_path: self.ts_path,
            extra_globals: self.extra_globals,
            auto_serve_bus: self.auto_serve_bus,
            shutdown_token: self.shutdown_token,
        }
    }
}

/// A host-owned SQLite connection, in the shape `mlua-batteries-sqlite` takes.
///
/// The pair travels together everywhere: the mutex is what a statement locks
/// (inside the blocking closure, never across an `.await`), and the interrupt
/// handle is how a cancelled task or an expired query timeout gets the
/// blocking thread to return and release it. Cloning shares one connection —
/// there is exactly one per database.
#[cfg(feature = "sqlite")]
#[derive(Clone)]
pub struct SqliteConn {
    /// The connection itself. Locked inside `spawn_blocking`, so the VM
    /// thread never holds the guard.
    pub conn: Arc<Mutex<rusqlite::Connection>>,
    /// `sqlite3_interrupt` for the statement currently running on `conn`.
    pub interrupt: Arc<rusqlite::InterruptHandle>,
}

/// Shared context passed into Lua bridge functions.
#[derive(Clone)]
pub struct HostContext {
    pub project_root: PathBuf,
    /// Connected mesh agent (present only when the `mesh` feature is enabled
    /// and a relay URL was supplied).
    #[cfg(feature = "mesh")]
    pub mesh_agent: Option<Arc<agent_mesh_sdk::MeshAgent>>,
    pub mcp_manager: Arc<RwLock<McpManager>>,
    /// Shared async HTTP client for `http.*` bridge.
    pub http_client: reqwest::Client,
    /// The connection behind the `sql.*` bridge (user tables).
    ///
    /// Opened by the host and handed to `mlua-batteries-sqlite`, which runs
    /// every statement inside `tokio::task::spawn_blocking` and takes the
    /// mutex *there*, not on the VM thread — so no lock guard and no blocking
    /// call crosses an `.await`, and the Lua VM yields while SQLite works.
    #[cfg(feature = "sqlite")]
    pub sql_conn: SqliteConn,
    /// The connection behind the `kv.*` bridge (`__kv` table only).
    ///
    /// A separate database from `sql_conn`, so KV scratch state and user SQL
    /// data do not share WAL, page cache, or backup lifecycle.
    #[cfg(feature = "sqlite")]
    pub kv_conn: SqliteConn,
    /// Handle to the SQLite connection thread behind the `ts.*` bridge (TSDB —
    /// time-series table).
    ///
    /// A third database, on a file of its own, so the TSDB's WAL shares
    /// neither page cache nor backup lifecycle with kv/sql. Unlike the two
    /// beside it, this connection is not shared but confined: it lives on that
    /// thread, and `std.ts` sends statements to it and awaits them. Different
    /// route, same rule — the Lua VM never waits on SQLite.
    #[cfg(feature = "sqlite")]
    pub ts_isle: rusqlite_isle::AsyncIsle,
    /// Async handle to the main Isle Lua VM that runs the user script via
    /// `coroutine_eval`. After Subtask 2, `bridge::bus` no longer dispatches
    /// handlers against this Isle; handlers live on `handler_isle` instead.
    /// The field is retained because bridge code still keyed to the main
    /// Isle (future `coroutine_call` back-edges, introspection APIs) may
    /// need it, and removing it would force another HostContext reshape.
    #[allow(dead_code)]
    pub isle: Arc<AsyncIsle>,
    /// Dedicated Isle for EventBus handler execution. Lua handlers
    /// registered via `bus.on` / `bus.on_any` run here so that CPU-bound
    /// handler code does not occupy the main Isle's LocalSet and block
    /// grace timers / shutdown wakers on the main VM side.
    ///
    /// Used by `bridge::bus` to forward handler bytecode
    /// (`Function::dump(true)` → `handler_isle.exec(...)`) and by
    /// [`LuaHandler::call`](crate::bridge::bus) to dispatch via
    /// `coroutine_call("__bus_dispatch", ...)`.
    pub handler_isle: Arc<AsyncIsle>,
    /// Ingress sender for the EventBus. Adapters (mesh / webhook / …)
    /// clone this and push `Event`s. The ST3 mesh adapter captures its own
    /// clone at `MeshAgent::connect` time, so the field itself is not read
    /// elsewhere in the ST3 cut — kept `pub` for ST4+ adapter wiring.
    #[allow(dead_code)]
    pub bus_tx: mpsc::Sender<Event>,
    /// Mutex-wrapped `Option<EventBus>` so `bus.on` / `bus.on_any` can lock
    /// briefly from sync Lua context, and `bus.serve` can `Option::take`
    /// ownership before entering the long-lived `run()` await (avoiding the
    /// await-holding-lock anti-pattern on a `std::sync::Mutex`).
    pub event_bus: Arc<Mutex<Option<EventBus>>>,
    /// Pre-edit file contents captured by `std.fs.edit`, consumed by
    /// `std.fs.rollback`. One level per path — enough to discard the last
    /// edit, which is what a build-and-fix loop needs when it decides an
    /// iteration made things worse.
    pub fs_snapshots: crate::bridge::fs::SnapshotStore,
    /// The connection threads every `knl` session's event log lives on.
    ///
    /// A kernel session opens its own SQLite thread (and a second, read-only
    /// one the first time it is queried), and hands the *driver* — the only
    /// thing that can drain and join that thread — here rather than keeping
    /// it. That is what lets the drop backstop work: a handle nobody closed
    /// submits its `session_closed` from `Drop`, without waiting, and the
    /// thread is still there to run it because its lifetime is the host's
    /// rather than the session's.
    ///
    /// Cloneable and shared, like the isle handles beside it; the run loop
    /// drains it once, in [`shutdown`], after the Lua VM is gone.
    pub knl_drivers: crate::knl::IsleDrivers,
}

impl HostContext {
    /// Agent id of the connected mesh agent, if any.
    ///
    /// Returns `Some(agent_id)` when the `mesh` feature is enabled and a mesh
    /// agent is connected. Keeps the `#[cfg(feature = "mesh")]` gating out of
    /// bridge call sites that only need a fallback agent-id string.
    #[cfg(feature = "mesh")]
    pub fn mesh_agent_id(&self) -> Option<String> {
        self.mesh_agent.as_ref().map(|a| a.agent_id().to_string())
    }

    /// See the `mesh`-enabled variant. Without the `mesh` feature there is no
    /// mesh agent, so this is always `None`.
    #[cfg(not(feature = "mesh"))]
    pub fn mesh_agent_id(&self) -> Option<String> {
        None
    }
}

/// Create the parent directory of a database file, unless the path names an
/// in-memory database (which has no parent to create).
///
/// `label` names the database in the error (`sql` / `kv` / `ts`).
#[cfg(feature = "sqlite")]
fn prepare_sqlite_dir(path: &Path, label: &'static str) -> BlockResult<bool> {
    let is_memory = crate::bridge::config::is_memory_sql(path);
    if !is_memory {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| BlockError::Runtime(format!("{label} dir create: {e}")))?;
        }
    }
    Ok(is_memory)
}

/// Open the SQLite database at `path` (or `:memory:`) on a connection thread
/// of its own, and return the handle plus the driver that shuts it down.
///
/// The ENV-driven pragmas are applied where they cost the caller nothing — the
/// busy timeout through the builder (which sets it before anything else runs)
/// and `journal_mode` in the init closure, which runs on the connection thread
/// before any job does. `init` runs there too, immediately after, which is
/// where a bridge's own schema DDL belongs: it waits on SQLite, and waiting is
/// the connection thread's business rather than the Lua VM's. The isle owns
/// the connection, so `std.ts` — its only caller now that `std.sql` /
/// `std.kv` are on [`open_sqlite_conn`] — never takes a lock and never blocks
/// the Lua runtime waiting for SQLite.
///
/// The caller must keep the returned [`rusqlite_isle::AsyncIsleDriver`] and
/// shut it down; dropping it alone does not stop the thread.
#[cfg(feature = "sqlite")]
async fn open_sqlite_isle<F>(
    path: &Path,
    label: &'static str,
    init: F,
) -> BlockResult<(rusqlite_isle::AsyncIsle, rusqlite_isle::AsyncIsleDriver)>
where
    F: FnOnce(&mut rusqlite::Connection) -> Result<(), rusqlite::Error> + Send + 'static,
{
    let is_memory = prepare_sqlite_dir(path, label)?;
    let busy = crate::bridge::config::sql_busy_timeout();
    let journal = crate::bridge::config::sql_journal_mode();
    let (isle, driver) = rusqlite_isle::AsyncIsle::builder()
        .thread_name(label)
        .busy_timeout(busy)
        .spawn(path, move |conn| {
            if !is_memory {
                conn.pragma_update(None, "journal_mode", &journal)?;
            }
            init(conn)
        })
        .await
        .map_err(|e| BlockError::Runtime(format!("sqlite open {}: {e}", path.display())))?;
    info!(label, path = %path.display(), busy_ms = busy.as_millis() as i64, "sqlite initialized");
    Ok((isle, driver))
}

/// Open the SQLite database at `path` (or `:memory:`) as a connection the host
/// owns and shares, for the bridges that take one that way.
///
/// This is the other half of the same rule the isle keeps: `std.sql` /
/// `std.kv` run their statements inside `tokio::task::spawn_blocking` and lock
/// the mutex there, so the VM thread hands the work off and yields instead of
/// waiting on SQLite. What it does *not* have is a thread of its own, which is
/// why there is no driver to shut down — the connection closes when the last
/// clone of the [`SqliteConn`] goes.
///
/// The setup mirrors [`open_sqlite_isle`] step for step, because these two
/// databases used to be opened by it: the parent directory is created,
/// `busy_timeout` is applied first, `synchronous` is set to the isle's `NORMAL`
/// preset, and the configured `journal_mode` (`WAL` unless overridden) is
/// applied to file-backed databases — an in-memory one has no journal to set.
/// `init` runs last, on this thread, before the VM exists.
#[cfg(feature = "sqlite")]
fn open_sqlite_conn<F>(path: &Path, label: &'static str, init: F) -> BlockResult<SqliteConn>
where
    F: FnOnce(&rusqlite::Connection) -> Result<(), rusqlite::Error>,
{
    let is_memory = prepare_sqlite_dir(path, label)?;
    let busy = crate::bridge::config::sql_busy_timeout();
    let journal = crate::bridge::config::sql_journal_mode();

    let open = || -> Result<rusqlite::Connection, rusqlite::Error> {
        let conn = rusqlite::Connection::open(path)?;
        conn.busy_timeout(busy)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        if !is_memory {
            conn.pragma_update(None, "journal_mode", &journal)?;
        }
        init(&conn)?;
        Ok(conn)
    };
    let conn =
        open().map_err(|e| BlockError::Runtime(format!("sqlite open {}: {e}", path.display())))?;

    let interrupt = Arc::new(conn.get_interrupt_handle());
    info!(label, path = %path.display(), busy_ms = busy.as_millis() as i64, "sqlite initialized");
    Ok(SqliteConn {
        conn: Arc::new(Mutex::new(conn)),
        interrupt,
    })
}

/// Build the init closure shared between the main Isle and the handler
/// Isle.  Sets `_SCRIPT_NAME`, registers `mlua-batteries` `std.*`, and
/// configures `package.path` / `package.searchers` so `require "agent"`
/// (and any `blocks/` module) works inside the Lua VM.
///
/// Returns an `FnOnce` so each call produces a fresh closure; this lets
/// both Isles be spawned from the same config without `Clone` bounds on
/// the captured `HashMap`.
fn build_isle_init(
    script_name: String,
    script_dir: String,
    blocks_paths: String,
    blocks_roots: Vec<PathBuf>,
    prompt: Option<String>,
    context: Option<String>,
    extra_globals: HashMap<String, serde_json::Value>,
) -> impl FnOnce(&mlua::Lua) -> mlua::Result<()> + Send + 'static {
    move |lua| {
        // Set script name before registering bridges (used by log.* for attribution)
        lua.globals().set("_SCRIPT_NAME", script_name.as_str())?;
        if let Some(ref p) = prompt {
            lua.globals().set("_PROMPT", p.as_str())?;
        }
        if let Some(ref c) = context {
            lua.globals().set("_CONTEXT", c.as_str())?;
        }

        mlua_batteries::register_all(lua, "std")?;

        // ── async overrides ───────────────────────────────────────────
        // `register_all` gives a `std` that needs no runtime, which means
        // its `time.sleep`, `proc.pipeline`, `http.*` and `fs.*` entries
        // park the VM thread — and with it every sibling coroutine — for as
        // long as the OS takes. This replaces them in place with async ones:
        // `tokio::time::sleep` for the sleep, the blocking pool for the
        // rest. Lua-side names, arguments, returns and error messages are
        // unchanged; what changes is that the VM goes on running.
        //
        // Both Isles are built from this closure, so one call covers the
        // main VM and the handler VM. It has to come after `register_all`
        // (there is nothing to override before it); `std.task`, whose
        // cancel token `time.sleep` now races, is registered later with the
        // other bridges — the overrides read the token from a thread-local
        // at call time, not at registration, so the order between them does
        // not matter.
        //
        // The one thing a script must not do is pass a function that calls
        // an overridden entry to `std.time.measure`, which calls its
        // argument synchronously: the yield would cross a Rust call
        // boundary. No block or fixture in this repo does.
        mlua_batteries::async_overrides::register_by_name(lua, "std")?;

        // ── extra_globals from BlockConfig ──────────────────────────
        // Inject SDK-supplied parameterisation values into the Lua
        // global namespace. Registered after mlua_batteries so that
        // any value that *intentionally* shadows a `std.*` symbol
        // wins — callers are responsible for not stomping on bridges
        // they need.
        for (name, value) in &extra_globals {
            let lua_value = crate::bridge::json_to_lua(lua, value.clone())
                .map_err(|e| mlua::Error::external(format!("extra_globals[{name}]: {e}")))?;
            lua.globals().set(name.as_str(), lua_value)?;
        }

        // ── package.path ──────────────────────────────────────────────
        // Priority: script_dir > project_root/blocks/ > exe_dir/blocks/ > default
        let package: mlua::Table = lua.globals().get("package")?;
        let current_path: String = package.get("path")?;
        let new_path =
            format!("{script_dir}/?.lua;{script_dir}/?/init.lua;{blocks_paths}{current_path}");
        package.set("path", new_path)?;

        // ── require resolution — mlua-pkg Registry ────────────────────
        // One priority chain instead of two parallel mechanisms:
        //
        //   script_dir/  >  project_root/blocks/  >  exe_dir/blocks/  >  embedded
        //
        // which is exactly the order `package.path` + the old trailing
        // searcher produced. The Registry hook installs at the FRONT of
        // `package.searchers`, so the filesystem resolvers must be listed
        // ahead of the embedded sources here for overrides to keep winning.
        //
        // `package.path` above is left in place: it still serves plain Lua
        // files that predate the Registry and anything a script requires
        // relative to itself.
        let mut registry = mlua_pkg::Registry::new();

        let mut fs_roots: Vec<PathBuf> = vec![PathBuf::from(&script_dir)];
        fs_roots.extend(blocks_roots.iter().cloned());
        for root in fs_roots {
            // Symlink-aware, not the plain constructor: this repo's own
            // `blocks/agent` is a symlink into `crates/agent-block-core/blocks/`,
            // and the default sandbox rejects anything whose canonical path
            // leaves the root. That rejection is `Some(Err)`, which does not
            // fall through to the next resolver, so one symlinked block
            // directory would break `require` for every module.
            match mlua_pkg::resolvers::FsResolver::new_symlink_aware(root.clone()) {
                Ok(resolver) => {
                    registry.add(resolver);
                }
                Err(e) => {
                    // A missing directory is expected (script_dir always
                    // exists, blocks/ roots are optional); anything else is
                    // worth surfacing without failing the whole run.
                    warn!(root = %root.display(), error = %e, "FsResolver init skipped");
                }
            }
        }

        // Embedded sources baked in at compile time — lowest priority, so a
        // filesystem copy of `blocks/agent/init.lua` still overrides it.
        let mut memory = mlua_pkg::resolvers::MemoryResolver::new();
        for (name, source) in EMBEDDED_BLOCKS.iter().chain(EMBEDDED_LIBS.iter()) {
            memory = memory.add(*name, *source);
        }
        // `knl_types` is the one embedded module with no file behind it: the
        // lshape declaration of the kernel's syscall surface, generated here
        // from the Rust argument and return types in `bridge/knl.rs`. It is
        // built at start rather than checked in because a generated file in
        // the tree is a file that can be edited, and one that has been edited
        // is a second declaration wearing the first one's name — which is
        // exactly the drift the Lua kernel's registry stopped having when it
        // started pointing at this. Same lowest priority as the rest: a
        // filesystem `knl_types` would win, and would be the caller's own.
        memory = memory.add("knl_types", crate::bridge::knl::lshape_module_source());
        registry.add(memory);

        registry
            .install(lua)
            .map_err(|e| mlua::Error::external(format!("require registry install failed: {e}")))?;

        Ok(())
    }
}

/// Spawn the dedicated handler Isle.
///
/// The handler Isle runs Lua bus handlers (`bus.on` / `bus.on_any`) on a
/// separate OS thread with its own `tokio` current-thread runtime, keeping
/// CPU-bound handlers from starving the main Isle's grace timers.
///
/// Bridge registration is deferred to a follow-up `exec` in `run()` because
/// `HostContext` is not constructible until both Isles exist (the struct
/// itself holds `Arc<AsyncIsle>` for both).
async fn spawn_handler_isle(
    script_name: String,
    script_dir: String,
    blocks_paths: String,
    blocks_roots: Vec<PathBuf>,
    prompt: Option<String>,
    context: Option<String>,
    extra_globals: HashMap<String, serde_json::Value>,
) -> BlockResult<(Arc<AsyncIsle>, AsyncIsleDriver)> {
    let init = build_isle_init(
        script_name,
        script_dir,
        blocks_paths,
        blocks_roots,
        prompt,
        context,
        extra_globals,
    );
    let (isle, driver) = AsyncIsle::builder()
        .thread_name("agent-block-handler-isle")
        .spawn(init)
        .await
        .map_err(|e| BlockError::Runtime(format!("handler isle spawn failed: {e}")))?;
    info!(
        thread_name = "agent-block-handler-isle",
        "handler Isle spawned"
    );
    Ok((Arc::new(isle), driver))
}

#[cfg(feature = "mesh")]
fn hex_decode_32(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.len() != 64 {
        return Err(format!("expected 64 hex chars, got {}", s.len()));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = u8::from_str_radix(&s[2 * i..2 * i + 1], 16)
            .map_err(|e| format!("invalid hex at position {}: {e}", 2 * i))?;
        let lo = u8::from_str_radix(&s[2 * i + 1..2 * i + 2], 16)
            .map_err(|e| format!("invalid hex at position {}: {e}", 2 * i + 1))?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

/// Concrete payloads resolved from the `*Source` enums on [`BlockConfig`]
/// before any Isle setup begins.
struct ResolvedSources {
    script_source: String,
    script_name: String,
    script_dir: PathBuf,
    prompt: Option<String>,
    context: Option<String>,
    secret_key: Option<String>,
}

/// Resolve the script / prompt / context / secret-key sources to their
/// concrete values, reading from disk or environment exactly once.
fn resolve_sources(config: &BlockConfig) -> BlockResult<ResolvedSources> {
    let (script_source, script_name, script_dir) = match &config.script {
        ScriptSource::Path(p) => {
            let source = std::fs::read_to_string(p)
                .map_err(|e| BlockError::Script(format!("{}: {e}", p.display())))?;
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let dir = p
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            (source, name, dir)
        }
        ScriptSource::Inline { source, name } => {
            (source.clone(), name.clone(), config.project_root.clone())
        }
        ScriptSource::DefaultAgent => (
            DEFAULT_AGENT_INVOKER.to_string(),
            "default_agent_invoker.lua".to_string(),
            config.project_root.clone(),
        ),
    };

    let prompt: Option<String> = match &config.prompt {
        Some(PromptSource::Inline(s)) => Some(s.clone()),
        Some(PromptSource::File(p)) => Some(
            std::fs::read_to_string(p)
                .map_err(|e| BlockError::Script(format!("prompt file {}: {e}", p.display())))?,
        ),
        None => None,
    };
    let context: Option<String> = match &config.context {
        Some(PromptSource::Inline(s)) => Some(s.clone()),
        Some(PromptSource::File(p)) => Some(
            std::fs::read_to_string(p)
                .map_err(|e| BlockError::Script(format!("context file {}: {e}", p.display())))?,
        ),
        None => None,
    };
    let secret_key: Option<String> = match &config.secret_key {
        Some(SecretKeySource::Inline(s)) => Some(s.clone()),
        Some(SecretKeySource::Env(var)) => std::env::var(var).ok(),
        None => None,
    };

    Ok(ResolvedSources {
        script_source,
        script_name,
        script_dir,
        prompt,
        context,
        secret_key,
    })
}

/// Load `.env` from the project root into the process environment so Lua's
/// `std.env.get()` observes it. A missing file is intentionally ignored.
fn load_dotenv(project_root: &Path) {
    let env_path = project_root.join(".env");
    match dotenvy::from_path(&env_path) {
        Ok(()) => info!(path = %env_path.display(), ".env loaded"),
        Err(dotenvy::Error::Io(_)) => {} // file not found — fine
        Err(e) => tracing::warn!(path = %env_path.display(), error = %e, ".env parse error"),
    }
}

/// Background auto-serve dispatcher task handle plus its cancellation token,
/// or `None` when auto-serve is disabled.
type AutoServeState = Option<(tokio::task::JoinHandle<()>, CancellationToken)>;

/// EventBus wiring produced by [`setup_event_bus`].
struct BusSetup {
    event_bus: Arc<Mutex<Option<EventBus>>>,
    bus_tx: mpsc::Sender<Event>,
    auto_serve_state: AutoServeState,
}

/// Construct the bounded EventBus channel, pre-install host-side Rust
/// handlers, and (when `auto_serve_bus` is set with at least one handler)
/// spawn the background dispatcher loop before the script runs.
fn setup_event_bus(config: &BlockConfig) -> BlockResult<BusSetup> {
    // Construct the bounded mpsc BEFORE MeshAgent::connect so the relay
    // handler can hold a `bus_tx` clone and forward incoming requests
    // into the dispatcher. Capacity is ENV-driven (see bridge::config).
    let bus_capacity = crate::bridge::config::bus_capacity();
    let (bus_tx, bus_rx) = mpsc::channel::<Event>(bus_capacity);
    let event_bus = Arc::new(Mutex::new(Some(EventBus::new(bus_rx))));

    // Install host-side Rust handlers: kind-specific entries from
    // `host_handlers` and, when set, the kind-agnostic `host_handler`
    // (registered via `on_any` as the fallback for unmatched kinds).
    // Registered before any Lua bridge registers handlers and before
    // `bus.serve` takes ownership, so the EventBus already carries the
    // host handlers when the script starts.
    let has_kind_handlers = !config.host_handlers.is_empty();
    let has_any_handler = config.host_handler.is_some();
    if has_kind_handlers || has_any_handler {
        let mut guard = event_bus
            .lock()
            .map_err(|_| BlockError::Bus("event_bus mutex poisoned".into()))?;
        let bus = guard
            .as_mut()
            .ok_or_else(|| BlockError::Bus("event_bus already taken".into()))?;
        for (kind, handler) in &config.host_handlers {
            bus.on(kind.clone(), Arc::clone(handler))
                .map_err(|e| BlockError::Bus(format!("host_handlers on({kind}): {e}")))?;
        }
        if let Some(any_handler) = &config.host_handler {
            bus.on_any(Arc::clone(any_handler))
                .map_err(|e| BlockError::Bus(format!("host_handler on_any: {e}")))?;
        }
        info!(
            kind_handlers = config.host_handlers.len(),
            any_handler = has_any_handler,
            "host handlers pre-installed"
        );
    }

    // auto-serve: when enabled with at least one host-side handler, take the
    // EventBus out of the Mutex *before* the script runs and spawn the
    // dispatcher loop on the runtime. This lets `bus.emit(kind, payload)`
    // from the script reach the host handler without requiring the script to
    // call `bus.serve()` (which blocks on signals and never returns under
    // programmatic embedding).
    let auto_serve = config.auto_serve_bus && (has_kind_handlers || has_any_handler);
    let auto_serve_state: AutoServeState = if auto_serve {
        let bus = {
            let mut guard = event_bus
                .lock()
                .map_err(|_| BlockError::Bus("event_bus mutex poisoned".into()))?;
            guard
                .take()
                .ok_or_else(|| BlockError::Bus("event_bus already taken".into()))?
        };
        let token = CancellationToken::new();
        let token_for_task = token.clone();
        let handle = tokio::spawn(async move {
            let mut bus = bus;
            if let Err(e) = bus.run(token_for_task).await {
                tracing::error!(error = %e, "auto-serve: dispatcher loop returned error");
            }
        });
        info!("auto-serve: dispatcher spawned");
        Some((handle, token))
    } else {
        None
    };

    Ok(BusSetup {
        event_bus,
        bus_tx,
        auto_serve_state,
    })
}

/// Connect to the mesh relay when `relay_url` is set, deriving the Ed25519
/// identity from `secret_key` (or a fresh random keypair) and wiring the
/// EventBus relay handler. Returns `None` when mesh is disabled.
#[cfg(feature = "mesh")]
async fn connect_mesh(
    relay_url: Option<&String>,
    secret_key: Option<&String>,
    bus_tx: &mpsc::Sender<Event>,
) -> BlockResult<Option<Arc<agent_mesh_sdk::MeshAgent>>> {
    let Some(relay_url) = relay_url else {
        return Ok(None);
    };
    let keypair = match secret_key {
        Some(hex_str) => {
            let bytes = hex_decode_32(hex_str)
                .map_err(|e| BlockError::Runtime(format!("secret-key: {e}")))?;
            agent_mesh_core::identity::AgentKeypair::from_bytes(&bytes)
        }
        None => agent_mesh_core::identity::AgentKeypair::generate(),
    };
    info!(agent_id = %keypair.agent_id(), "mesh identity");
    let acl = agent_mesh_core::acl::AclPolicy {
        default_deny: false,
        rules: vec![],
    };
    let handler: Arc<dyn agent_mesh_sdk::RequestHandler> =
        Arc::new(BusRelayHandler::new(bus_tx.clone()));
    let url = relay_url.clone();
    let agent = agent_mesh_sdk::MeshAgent::connect(keypair, &url, acl, handler)
        .await
        .map_err(|e| BlockError::Mesh(format!("connect to {relay_url} failed: {e}")))?;
    info!(relay_url = %relay_url, "mesh connected");
    Ok(Some(Arc::new(agent)))
}

/// The three SQLite databases backing the `sql.*`, `kv.*`, and `ts.*` Lua
/// bridges.
///
/// Two shapes, one rule. `sql` and `kv` are connections the host owns and
/// shares, whose statements go to the blocking pool; `ts` is a connection
/// thread the statements are sent to. Either way the VM thread hands the work
/// off and yields — it never waits on SQLite itself.
#[cfg(feature = "sqlite")]
struct SqliteConns {
    sql: SqliteConn,
    kv: SqliteConn,
    ts_isle: rusqlite_isle::AsyncIsle,
    drivers: SqliteDrivers,
}

/// The lifecycle owner of the `ts` connection thread.
///
/// Kept out of [`HostContext`] (which is cloned into every bridge) because a
/// driver is not clonable by design: there is exactly one, held by the run
/// loop until [`shutdown`] joins the thread. `sql` and `kv` have no entry
/// here — a shared connection closes with its last [`SqliteConn`] clone,
/// which is when the Lua VMs holding them are gone.
#[cfg(feature = "sqlite")]
struct SqliteDrivers {
    ts: rusqlite_isle::AsyncIsleDriver,
}

/// Open the sql / kv / ts SQLite databases, honoring the [`BlockConfig`]
/// path overrides and otherwise falling back to the env-driven resolution.
#[cfg(feature = "sqlite")]
async fn init_sqlite(config: &BlockConfig) -> BlockResult<SqliteConns> {
    let sql_path = match &config.sql_path {
        Some(p) => p.clone(),
        None => crate::bridge::config::sql_path().map_err(BlockError::Runtime)?,
    };
    let sql = open_sqlite_conn(&sql_path, "sql", |_| Ok(()))?;

    let kv_path = match &config.kv_path {
        Some(p) => p.clone(),
        None => crate::bridge::config::kv_path().map_err(BlockError::Runtime)?,
    };
    // The `__kv` table is ensured here, while the connection is still the
    // host's alone — so `bridge::kv::register` has nothing left to do but
    // hand the connection over, and the Lua VM never runs DDL.
    let kv = open_sqlite_conn(&kv_path, "kv", mlua_batteries_sqlite::kv::init_schema)?;

    let ts_path = match &config.ts_path {
        Some(p) => p.clone(),
        None => crate::bridge::config::ts_path().map_err(BlockError::Runtime)?,
    };
    // Same for `ts`, on the connection thread, before the isle takes its
    // first job.
    let (ts_isle, ts_driver) = open_sqlite_isle(&ts_path, "ts", |conn| {
        conn.execute_batch(crate::bridge::ts::SCHEMA_DDL)
    })
    .await?;

    Ok(SqliteConns {
        sql,
        kv,
        ts_isle,
        drivers: SqliteDrivers { ts: ts_driver },
    })
}

/// The main and handler Isles plus their drivers, produced by [`spawn_isles`].
struct SpawnedIsles {
    isle: Arc<AsyncIsle>,
    driver: AsyncIsleDriver,
    handler_isle: Arc<AsyncIsle>,
    handler_driver: AsyncIsleDriver,
}

/// Spawn the main Lua Isle and the dedicated handler Isle from the same
/// resolved script parameters. Their bridges are registered in a later pass
/// (via [`register_bridges`]) once the `HostContext` exists.
async fn spawn_isles(
    script_name: &str,
    script_dir: &str,
    blocks_paths: &str,
    blocks_roots: &[PathBuf],
    prompt: Option<String>,
    context: Option<String>,
    extra_globals: &HashMap<String, serde_json::Value>,
) -> BlockResult<SpawnedIsles> {
    let (isle, driver) = AsyncIsle::spawn(build_isle_init(
        script_name.to_string(),
        script_dir.to_string(),
        blocks_paths.to_string(),
        blocks_roots.to_vec(),
        prompt.clone(),
        context.clone(),
        extra_globals.clone(),
    ))
    .await
    .map_err(|e| BlockError::Runtime(format!("AsyncIsle spawn failed: {e}")))?;
    let isle = Arc::new(isle);

    // handler Isle (sequential, dependencies are trivial)
    let (handler_isle, handler_driver) = spawn_handler_isle(
        script_name.to_string(),
        script_dir.to_string(),
        blocks_paths.to_string(),
        blocks_roots.to_vec(),
        prompt,
        context,
        extra_globals.clone(),
    )
    .await?;

    Ok(SpawnedIsles {
        isle,
        driver,
        handler_isle,
        handler_driver,
    })
}

/// Register the Lua stdlib bridges on both the main Isle
/// (`bridge::register_all`) and the handler Isle
/// (`bridge::register_all_handler_side`).
async fn register_bridges(
    ctx: &HostContext,
    isle: &Arc<AsyncIsle>,
    handler_isle: &Arc<AsyncIsle>,
) -> BlockResult<()> {
    {
        let ctx = ctx.clone();
        isle.exec(move |lua| {
            bridge::register_all(lua, &ctx)
                .map_err(|e| mlua_isle::IsleError::Lua(format!("bridge register failed: {e}")))?;
            Ok(String::new())
        })
        .await
        .map_err(|e| BlockError::Runtime(format!("bridge register: {e}")))?;
    }

    {
        let ctx = ctx.clone();
        handler_isle
            .exec(move |lua| {
                bridge::register_all_handler_side(lua, &ctx).map_err(|e| {
                    mlua_isle::IsleError::Lua(format!("handler bridge register failed: {e}"))
                })?;
                Ok(String::new())
            })
            .await
            .map_err(|e| BlockError::Runtime(format!("handler bridge register: {e}")))?;
    }

    Ok(())
}

/// Inject the [`BlockConfig::host_tools`] Rust tools into the Lua
/// `_TOOL_REGISTRY` so they are indistinguishable from Lua-defined tools.
/// Each entry becomes an Anthropic-shaped tool spec table
/// (`{ name, schema = { description, input_schema }, handler, group? }`)
/// whose `handler` bridges back into the supplied `ToolHandler::call`.
/// No-op when no host tools are supplied.
async fn inject_host_tools(isle: &Arc<AsyncIsle>, host_tools: &[HostToolSpec]) -> BlockResult<()> {
    if host_tools.is_empty() {
        return Ok(());
    }
    let host_tools = host_tools.to_vec();
    let tool_count = host_tools.len();
    isle.exec(move |lua| {
        let registry: mlua::Table = lua
            .globals()
            .get("_TOOL_REGISTRY")
            .map_err(|e| mlua_isle::IsleError::Lua(format!("get _TOOL_REGISTRY: {e}")))?;
        for tool in host_tools {
            let entry = lua
                .create_table()
                .map_err(|e| mlua_isle::IsleError::Lua(format!("create entry: {e}")))?;
            entry
                .set("name", tool.name.as_str())
                .map_err(|e| mlua_isle::IsleError::Lua(format!("set name: {e}")))?;
            // schema = { description, input_schema } — Anthropic shape
            let schema = lua
                .create_table()
                .map_err(|e| mlua_isle::IsleError::Lua(format!("create schema: {e}")))?;
            schema
                .set("description", tool.description.as_str())
                .map_err(|e| mlua_isle::IsleError::Lua(format!("set description: {e}")))?;
            let input_schema_lua = crate::bridge::json_to_lua(lua, tool.input_schema.clone())
                .map_err(|e| mlua_isle::IsleError::Lua(format!("input_schema: {e}")))?;
            schema
                .set("input_schema", input_schema_lua)
                .map_err(|e| mlua_isle::IsleError::Lua(format!("set input_schema: {e}")))?;
            entry
                .set("schema", schema)
                .map_err(|e| mlua_isle::IsleError::Lua(format!("set schema: {e}")))?;
            if let Some(group) = &tool.group {
                entry
                    .set("group", group.as_str())
                    .map_err(|e| mlua_isle::IsleError::Lua(format!("set group: {e}")))?;
            }
            let handler_arc = Arc::clone(&tool.handler);
            let handler_fn = lua
                .create_async_function(move |lua, input: mlua::Value| {
                    let handler = Arc::clone(&handler_arc);
                    async move {
                        let input_json = crate::bridge::lua_to_json(&lua, input)?;
                        let result = handler
                            .call(input_json)
                            .await
                            .map_err(mlua::Error::external)?;
                        crate::bridge::json_to_lua(&lua, result)
                    }
                })
                .map_err(|e| mlua_isle::IsleError::Lua(format!("create handler: {e}")))?;
            entry
                .set("handler", handler_fn)
                .map_err(|e| mlua_isle::IsleError::Lua(format!("set handler: {e}")))?;
            registry
                .set(tool.name.as_str(), entry)
                .map_err(|e| mlua_isle::IsleError::Lua(format!("registry set: {e}")))?;
        }
        Ok(String::new())
    })
    .await
    .map_err(|e| BlockError::Runtime(format!("host_tools inject: {e}")))?;
    info!(count = tool_count, "host tools injected into Lua registry");
    Ok(())
}

/// Execute the resolved Lua script on the main Isle, racing it against the
/// optional caller `shutdown_token`. On cancellation the Isle is unwound via
/// its own cancel token before returning [`BlockError::Cancelled`].
///
/// The returned string is the chunk's value, stringified by the Isle: a Lua
/// string passes through unchanged, `nil` becomes empty, and a table becomes
/// `table: 0x…`. A caller that wants structured data back therefore has the
/// script `return std.json.encode(t)` — see [`run_capture`].
async fn execute_script(
    isle: &Arc<AsyncIsle>,
    script_source: &str,
    script_name: &str,
    shutdown_token: Option<&CancellationToken>,
) -> BlockResult<String> {
    let _exec_span = info_span!("execute", script = %script_name);

    let mut task = isle.spawn_coroutine_eval(script_source);
    let task_cancel = task.cancel_token().clone();
    match shutdown_token {
        Some(token) => {
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    task_cancel.cancel();
                    // Wait for the Isle to unwind so the VM is in a
                    // consistent state before driver shutdown. The
                    // debug hook fires at the next HOOK_INTERVAL.
                    let _ = (&mut task).await;
                    info!("shutdown_token: cancelled by caller");
                    Err(BlockError::Cancelled)
                }
                res = &mut task => res.map_err(|e| BlockError::Script(format!("{e}"))),
            }
        }
        None => (&mut task)
            .await
            .map_err(|e| BlockError::Script(format!("{e}"))),
    }
}

/// Drain the auto-serve dispatcher: give it a grace window to flush queued
/// events, then cancel and bound-join it. No-op when auto-serve is off.
async fn drain_auto_serve(auto_serve_state: AutoServeState) {
    if let Some((handle, token)) = auto_serve_state {
        let grace_ms = crate::bridge::config::task_grace_ms();
        let grace = Duration::from_millis(grace_ms);
        tokio::time::sleep(grace).await;
        token.cancel();
        match tokio::time::timeout(grace, handle).await {
            Ok(Ok(())) => info!("auto-serve: dispatcher shut down cleanly"),
            Ok(Err(join_err)) => {
                tracing::error!(error = %join_err, "auto-serve: dispatcher task join error");
            }
            Err(_) => {
                tracing::warn!(
                    grace_ms,
                    "auto-serve: dispatcher join timed out after cancel; forcing exit"
                );
            }
        }
    }
}

/// Tear down host resources in order: disconnect MCP servers, shut down the
/// main Isle driver (fatal on error), then the handler Isle driver, the
/// kernel's per-session connection threads and the `ts` one (logged,
/// non-fatal so a worker-thread panic does not poison the process exit).
///
/// The kernel's threads go *after* the Isles on purpose. Dropping a VM runs
/// its collector, which is where a session nobody closed submits its
/// `session_closed` without waiting for it — so those threads have to be
/// alive to take that write, and drained only once nothing can still be
/// handed to them.
async fn shutdown(
    mcp_manager: &Arc<RwLock<McpManager>>,
    driver: AsyncIsleDriver,
    handler_driver: AsyncIsleDriver,
    knl_drivers: crate::knl::IsleDrivers,
    #[cfg(feature = "sqlite")] sqlite_drivers: SqliteDrivers,
) -> BlockResult<()> {
    let _shutdown_span = info_span!("shutdown");

    mcp_manager.write().await.disconnect_all().await?;

    driver
        .shutdown()
        .await
        .map_err(|e| BlockError::Runtime(format!("AsyncIsle shutdown failed: {e}")))?;

    // Handler Isle shutdown is independent of main shutdown: a failure
    // here (e.g. ThreadPanic on the handler thread) is logged but does
    // not poison the main process exit. The main Isle has already
    // been stopped cleanly above.
    match handler_driver.shutdown().await {
        Ok(()) => info!(
            thread_name = "agent-block-handler-isle",
            "handler Isle shut down"
        ),
        Err(e) => tracing::error!(
            error = %e,
            thread_name = "agent-block-handler-isle",
            "handler Isle shutdown failed"
        ),
    }

    // The kernel's session threads. Both Isles are gone by now, so every Lua
    // session userdata has been collected and every drop backstop has
    // submitted its boundary; a graceful shutdown is what runs those queued
    // writes before the threads exit.
    {
        let count = knl_drivers.len();
        let failures = knl_drivers.shutdown().await;
        if failures.is_empty() {
            if count > 0 {
                info!(count, "knl session connection threads shut down");
            }
        } else {
            for e in &failures {
                tracing::error!(error = %e, "knl session connection thread shutdown failed");
            }
        }
    }

    // The `ts` connection thread: a graceful shutdown drains whatever the
    // script left queued before the thread exits. Logged rather than fatal,
    // for the same reason as the handler Isle above — the script has already
    // run, and its result is what the caller asked for. The `sql` / `kv`
    // connections need nothing here: they have no thread, and the last
    // reference to each went with the VMs shut down above.
    #[cfg(feature = "sqlite")]
    {
        let SqliteDrivers { ts } = sqlite_drivers;
        match ts.shutdown().await {
            Ok(()) => info!(label = "ts", "sqlite connection thread shut down"),
            Err(e) => {
                tracing::error!(error = %e, label = "ts", "sqlite shutdown failed")
            }
        }
    }

    Ok(())
}

/// Primary SDK entry point: run one agent-block execution to completion.
///
/// Given a fully-populated [`BlockConfig`], this drives the entire host
/// orchestration for a single run: it resolves the script / prompt / context /
/// secret-key sources, loads `.env` from the project root, spawns the main and
/// handler Lua Isles, opens the kv / sql / ts SQLite connections, optionally
/// connects to the mesh relay, initialises the MCP manager, injects the Lua
/// stdlib bridge plus any host-supplied tools / handlers, executes the script,
/// and finally tears everything down (MCP disconnect, Isle shutdown, auto-serve
/// dispatcher join).
///
/// The returned future is `Send`, so SDK consumers may `tokio::spawn` it.
///
/// # Errors
///
/// Returns [`BlockError`] when any stage fails: source resolution / file reads
/// ([`BlockError::Script`]), mesh connect ([`BlockError::Mesh`]), EventBus or
/// Isle setup ([`BlockError::Bus`] / [`BlockError::Runtime`]), or a script
/// runtime error ([`BlockError::Script`]). When a `shutdown_token` is supplied
/// and fires before the script finishes, returns [`BlockError::Cancelled`]
/// after the shutdown sequence completes.
///
/// Use [`run_capture`] when the caller needs the script's value rather than
/// only its success.
pub async fn run(config: BlockConfig) -> BlockResult<()> {
    run_capture(config).await.map(|_| ())
}

/// [`run`], returning what the script evaluated to.
///
/// Same orchestration, one difference: the chunk's value comes back instead of
/// being dropped. It arrives stringified — a Lua string unchanged, `nil` as the
/// empty string, a table as `table: 0x…`, which is useless to a caller. So the
/// contract for a script meant to be consumed this way is to **return a JSON
/// string**:
///
/// ```lua
/// return std.json.encode({ ok = true, summary = "…" })
/// ```
///
/// This exists for hosts that invoke a block on someone else's behalf and have
/// to hand the outcome back across a boundary — the MCP server mode
/// (`agent-block mcp`) is the first such caller. Nothing about the run differs;
/// a script that returns nothing simply yields an empty string.
pub async fn run_capture(config: BlockConfig) -> BlockResult<String> {
    // ── Resolve sources ───────────────────────────────────────────
    // Convert the `Source` enums on `BlockConfig` to their concrete
    // payloads before any Isle setup. `File`/`Path`/`Env` variants
    // read from disk / environment exactly once, here at the start.
    let ResolvedSources {
        script_source,
        script_name,
        script_dir: script_dir_pathbuf,
        prompt: prompt_resolved,
        context: context_resolved,
        secret_key: secret_key_resolved,
    } = resolve_sources(&config)?;

    // NOTE: We previously held entered span guards across awaits for nested
    // span context. That made the `run()` future `!Send`, which prevents
    // SDK consumers from `tokio::spawn(run(config))`. Span context is
    // attached to events via fields on the `info_span!` calls below; the
    // missing nesting is an acceptable trade-off for `Send` correctness.
    let _root_span = info_span!("agent_block", script = %script_name);

    // ── .env ──────────────────────────────────────────────────────
    // Load .env from project_root if present. Variables are merged into
    // the process environment so Lua's `std.env.get()` picks them up.
    load_dotenv(&config.project_root);

    // ── Init ──────────────────────────────────────────────────────
    let _init_span = info_span!("init");

    // ── EventBus + host handlers + auto-serve dispatcher ──────────────
    // Construct the bus channel, pre-install host-side Rust handlers, and
    // (when configured) spawn the background dispatcher before the script.
    let BusSetup {
        event_bus,
        bus_tx,
        auto_serve_state,
    } = setup_event_bus(&config)?;

    #[cfg(feature = "mesh")]
    let mesh_agent = connect_mesh(
        config.relay_url.as_ref(),
        secret_key_resolved.as_ref(),
        &bus_tx,
    )
    .await?;
    // `secret_key` / `relay_url` are consumed only by the mesh connect path.
    #[cfg(not(feature = "mesh"))]
    let _ = (&secret_key_resolved, &config.relay_url);

    let mcp_manager = Arc::new(RwLock::new(McpManager::with_rpc_timeout(
        config.mcp_rpc_timeout,
    )?));

    // Resolve project_root to absolute path.
    // canonicalize() can fail if the path doesn't exist; fall back to
    // joining with current_dir to guarantee an absolute path.
    let project_root = config
        .project_root
        .canonicalize()
        .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(&config.project_root)))?;

    // HTTP client: prefer the SDK-supplied client if any; otherwise
    // construct a fresh default reqwest::Client (legacy behavior).
    let http_client = config.http_client.clone().unwrap_or_default();

    // ── SQLite init (sql + kv + ts get separate DB files) ─────────────
    // BlockConfig overrides take precedence; otherwise the env-driven
    // resolution in `bridge::config::*` applies (see crate docs). Gated
    // behind the `sqlite` feature; when off, the sql/kv/ts bridges are not
    // registered and the `sql_path` / `kv_path` / `ts_path` config fields
    // (which remain present for API stability) are ignored.
    #[cfg(feature = "sqlite")]
    let SqliteConns {
        sql: sql_conn,
        kv: kv_conn,
        ts_isle,
        drivers: sqlite_drivers,
    } = init_sqlite(&config).await?;

    // Use the script dir derived from the resolved `ScriptSource` for
    // `package.path` lookups. For inline / default-agent variants the dir
    // falls back to `project_root` (set during source resolution above).
    let script_dir = script_dir_pathbuf.to_string_lossy().to_string();

    // Precompute values captured by the init closure so we don't need to
    // move the full `HostContext` into it (HostContext now holds
    // `Arc<AsyncIsle>`, which is available only after `AsyncIsle::spawn`
    // returns — classic chicken-and-egg). All bridge registrations run in a
    // second pass via `isle.exec` below.
    let blocks_paths = build_blocks_path(&project_root);
    let blocks_roots = build_blocks_roots(&project_root);
    let prompt = prompt_resolved.clone();
    let context = context_resolved.clone();

    // ── main + handler Isles ──────────────────────────────────────
    let SpawnedIsles {
        isle,
        driver,
        handler_isle,
        handler_driver,
    } = spawn_isles(
        &script_name,
        &script_dir,
        &blocks_paths,
        &blocks_roots,
        prompt,
        context,
        &config.extra_globals,
    )
    .await?;

    // Wire both Isles into McpManager so Lua notification callbacks can be
    // dispatched from the rmcp task thread.
    // - handler_isle: sampling/createMessage dispatch (exec on handler Isle)
    // - main_isle: progress/log notification dispatch (exec on main Isle so
    //   user callback upvalues are preserved — no bytecode dump/reload needed)
    {
        let mut mgr = mcp_manager.write().await;
        mgr.set_handler_isle(Arc::clone(&handler_isle));
        mgr.set_main_isle(Arc::clone(&isle));
    }

    // ── HostContext + bridge registration ──────────────────────────────
    // Wrap the isle in an Arc so `HostContext` can hand it to
    // `bridge::bus` (which uses `AsyncIsle::coroutine_call` to invoke Lua
    // handlers from the EventBus dispatcher task).
    let ctx = HostContext {
        project_root,
        #[cfg(feature = "mesh")]
        mesh_agent,
        mcp_manager: Arc::clone(&mcp_manager),
        http_client,
        #[cfg(feature = "sqlite")]
        sql_conn,
        #[cfg(feature = "sqlite")]
        kv_conn,
        #[cfg(feature = "sqlite")]
        ts_isle,
        isle: Arc::clone(&isle),
        handler_isle: Arc::clone(&handler_isle),
        bus_tx: bus_tx.clone(),
        event_bus: Arc::clone(&event_bus),
        fs_snapshots: Default::default(),
        knl_drivers: crate::knl::IsleDrivers::new(),
    };
    // Kept out of the context clone the bridges get: the run loop needs its
    // own reference to drain the threads after the VM has gone.
    let knl_drivers = ctx.knl_drivers.clone();

    register_bridges(&ctx, &isle, &handler_isle).await?;

    // ── Inject host_tools into the Lua tool registry ───────────────
    // Done after `bridge::register_all` so `_TOOL_REGISTRY` exists.
    inject_host_tools(&isle, &config.host_tools).await?;

    drop(_init_span);

    // ── Execute ───────────────────────────────────────────────────
    // When `shutdown_token` is supplied, race the script future against
    // the caller's cancellation signal. On cancel, propagate to the Isle
    // via the AsyncTask's cancel token so the debug hook unwinds the Lua
    // VM, then continue into the shutdown sequence below (we still want
    // to release MCP/mesh handles and join the auto-serve dispatcher
    // before returning).
    let script_result = execute_script(
        &isle,
        &script_source,
        &script_name,
        config.shutdown_token.as_ref(),
    )
    .await;

    // ── auto-serve drain + cancel ─────────────────────────────────
    // Let the dispatcher drain events queued by the script, then signal
    // shutdown and bound the join. Mirrors `bus.serve`'s grace pattern.
    drain_auto_serve(auto_serve_state).await;

    // ── Shutdown ──────────────────────────────────────────────────
    shutdown(
        &mcp_manager,
        driver,
        handler_driver,
        knl_drivers,
        #[cfg(feature = "sqlite")]
        sqlite_drivers,
    )
    .await?;

    script_result
}

/// mesh → bus source adapter.
///
/// Implements [`agent_mesh_sdk::RequestHandler`] by packaging every incoming
/// mesh request into an [`Event`] with `kind = "mesh"`, pushing it onto the
/// bounded `bus_tx` channel, and awaiting the Lua handler's ack over a
/// oneshot channel carried inside the event.
///
/// Error paths (all `tracing::error!`-logged — silent-err-drop policy):
///
/// | Failure                   | Return value                           |
/// |---------------------------|----------------------------------------|
/// | `bus_tx.send` closed/full | `{"error": "bus channel closed"}`      |
/// | ack receiver dropped      | `{"error": "ack dropped"}`             |
/// | Lua handler `BlockError`  | `{"error": "<handler error>"}`         |
/// | Handler exceeded 30s      | `{"error": "handler timeout"}`         |
///
/// The 30s ack timeout mirrors the client-side timeout on `mesh.request`
/// (see `src/bridge/mesh.rs`).
#[cfg(feature = "mesh")]
struct BusRelayHandler {
    tx: mpsc::Sender<Event>,
}

#[cfg(feature = "mesh")]
impl BusRelayHandler {
    fn new(tx: mpsc::Sender<Event>) -> Self {
        Self { tx }
    }
}

/// Bound used for both the mesh-adapter ack wait and other source timeouts.
#[cfg(feature = "mesh")]
const BUS_ACK_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(feature = "mesh")]
#[async_trait::async_trait]
impl agent_mesh_sdk::RequestHandler for BusRelayHandler {
    async fn handle(
        &self,
        from: &agent_mesh_core::identity::AgentId,
        payload: &serde_json::Value,
        _cancel: agent_mesh_sdk::CancelToken,
    ) -> serde_json::Value {
        let id = uuid::Uuid::new_v4().to_string();
        let meta = serde_json::json!({"from": from.to_string()});
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let event = Event {
            kind: "mesh".into(),
            id: id.clone(),
            payload: payload.clone(),
            meta,
            ack_tx: Some(ack_tx),
        };

        if let Err(e) = self.tx.send(event).await {
            tracing::error!(error = %e, id = %id, "bus channel closed; rejecting mesh request");
            return serde_json::json!({"error": "bus channel closed"});
        }

        match tokio::time::timeout(BUS_ACK_TIMEOUT, ack_rx).await {
            Ok(Ok(Ok(v))) => v,
            Ok(Ok(Err(e))) => {
                tracing::error!(id = %id, error = %e, "mesh handler returned error");
                serde_json::json!({"error": e.to_string()})
            }
            Ok(Err(e)) => {
                tracing::error!(id = %id, error = %e, "mesh ack receiver dropped");
                serde_json::json!({"error": "ack dropped"})
            }
            Err(_) => {
                tracing::error!(id = %id, timeout_secs = BUS_ACK_TIMEOUT.as_secs(), "mesh handler timeout");
                serde_json::json!({"error": "handler timeout"})
            }
        }
    }
}
