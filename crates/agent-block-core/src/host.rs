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
use tokio::sync::{mpsc, oneshot, RwLock};

use mlua_isle::{AsyncIsle, AsyncIsleDriver};
use tracing::{info, info_span, warn};

use crate::bridge;
use crate::bus::{Event, EventBus, Handler};
use agent_block_mcp::McpManager;
use agent_block_types::error::{BlockError, BlockResult};
use tokio_util::sync::CancellationToken;

/// Embedded Lua sources for blocks/ StdPkg modules.
/// These are baked into the binary at compile time so `cargo install` works
/// without any extra file distribution.
const EMBEDDED_BLOCKS: &[(&str, &str)] = &[
    ("agent", include_str!("../blocks/agent/init.lua")),
    ("session", include_str!("../blocks/session/init.lua")),
    (
        "compile_loop",
        include_str!("../blocks/compile_loop/init.lua"),
    ),
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

/// Shared context passed into Lua bridge functions.
#[derive(Clone)]
pub struct HostContext {
    pub project_root: PathBuf,
    pub mesh_agent: Option<Arc<agent_mesh_sdk::MeshAgent>>,
    pub mcp_manager: Arc<RwLock<McpManager>>,
    /// Shared async HTTP client for `http.*` bridge.
    pub http_client: reqwest::Client,
    /// Shared SQLite connection for `sql.*` bridge (user tables).
    pub sql_conn: Arc<Mutex<rusqlite::Connection>>,
    /// Interrupt handle for the sql connection.
    /// Used to cancel in-flight queries on timeout (see `bridge/sql.rs`).
    pub sql_interrupt: Arc<rusqlite::InterruptHandle>,
    /// Shared SQLite connection for `kv.*` bridge (`__kv` table only).
    /// Separate from sql_conn so KV scratch state and user SQL data don't
    /// share WAL, page cache, or backup lifecycle.
    pub kv_conn: Arc<Mutex<rusqlite::Connection>>,
    /// Interrupt handle for the kv connection.
    pub kv_interrupt: Arc<rusqlite::InterruptHandle>,
    /// Shared SQLite connection for `ts.*` bridge (TSDB — time-series table).
    /// Separate DB file so TSDB WAL does not share page cache with kv/sql.
    pub ts_conn: Arc<Mutex<rusqlite::Connection>>,
    /// Interrupt handle for the ts connection.
    /// Used by `bridge::ts` to cancel in-flight queries on timeout (Subtask 2).
    #[allow(dead_code)]
    pub ts_interrupt: Arc<rusqlite::InterruptHandle>,
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
}

/// Open a SQLite connection at `path` (or `:memory:`) and apply the shared
/// pragmas driven by ENV (`journal_mode`, `busy_timeout`). Returns the
/// connection wrapped in Arc<Mutex<_>> together with its interrupt handle.
///
/// `label` is used only for the init log line (`sql` / `kv`) so that the two
/// databases are distinguishable in tracing output.
fn open_sqlite(
    path: &Path,
    label: &'static str,
) -> BlockResult<(
    Arc<Mutex<rusqlite::Connection>>,
    Arc<rusqlite::InterruptHandle>,
)> {
    let is_memory = crate::bridge::config::is_memory_sql(path);
    if !is_memory {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| BlockError::Runtime(format!("{label} dir create: {e}")))?;
        }
    }
    let conn = rusqlite::Connection::open(path)
        .map_err(|e| BlockError::Runtime(format!("sqlite open {}: {e}", path.display())))?;
    if !is_memory {
        let journal = crate::bridge::config::sql_journal_mode();
        conn.pragma_update(None, "journal_mode", &journal)
            .map_err(|e| BlockError::Runtime(format!("journal_mode={journal}: {e}")))?;
    }
    let busy_ms = crate::bridge::config::sql_busy_timeout().as_millis() as i64;
    conn.pragma_update(None, "busy_timeout", busy_ms)
        .map_err(|e| BlockError::Runtime(format!("busy_timeout pragma: {e}")))?;
    info!(label, path = %path.display(), busy_ms, "sqlite initialized");
    let interrupt = Arc::new(conn.get_interrupt_handle());
    let conn = Arc::new(Mutex::new(conn));
    Ok((conn, interrupt))
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

        // ── package.searchers — embedded fallback ─────────────────────
        // Register a custom searcher that loads blocks/ modules from the
        // sources baked in at compile time.  This is the lowest-priority
        // searcher so filesystem copies always win.
        let embedded: HashMap<&'static str, &'static str> =
            EMBEDDED_BLOCKS.iter().copied().collect();

        let searchers: mlua::Table = package.get("searchers")?;
        let loader =
            lua.create_function(move |lua, name: String| match embedded.get(name.as_str()) {
                Some(source) => {
                    let chunk = lua
                        .load(*source)
                        .set_name(format!("@embedded:blocks/{name}/init.lua"));
                    let func = chunk.into_function()?;
                    Ok(mlua::Value::Function(func))
                }
                None => {
                    let msg = lua.create_string(format!("\n\tno embedded block '{name}'"))?;
                    Ok(mlua::Value::String(msg))
                }
            })?;
        // Append as the last searcher so filesystem paths remain preferred.
        let next_idx = searchers.raw_len() + 1;
        searchers.raw_set(next_idx, loader)?;

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
    prompt: Option<String>,
    context: Option<String>,
    extra_globals: HashMap<String, serde_json::Value>,
) -> BlockResult<(Arc<AsyncIsle>, AsyncIsleDriver)> {
    let init = build_isle_init(
        script_name,
        script_dir,
        blocks_paths,
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

pub async fn run(config: BlockConfig) -> BlockResult<()> {
    // ── Resolve sources ───────────────────────────────────────────
    // Convert the `Source` enums on `BlockConfig` to their concrete
    // payloads before any Isle setup. `File`/`Path`/`Env` variants
    // read from disk / environment exactly once, here at the start.
    let (script_source, script_name, script_dir_pathbuf) = match &config.script {
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

    let prompt_resolved: Option<String> = match &config.prompt {
        Some(PromptSource::Inline(s)) => Some(s.clone()),
        Some(PromptSource::File(p)) => Some(
            std::fs::read_to_string(p)
                .map_err(|e| BlockError::Script(format!("prompt file {}: {e}", p.display())))?,
        ),
        None => None,
    };
    let context_resolved: Option<String> = match &config.context {
        Some(PromptSource::Inline(s)) => Some(s.clone()),
        Some(PromptSource::File(p)) => Some(
            std::fs::read_to_string(p)
                .map_err(|e| BlockError::Script(format!("context file {}: {e}", p.display())))?,
        ),
        None => None,
    };
    let secret_key_resolved: Option<String> = match &config.secret_key {
        Some(SecretKeySource::Inline(s)) => Some(s.clone()),
        Some(SecretKeySource::Env(var)) => std::env::var(var).ok(),
        None => None,
    };

    // NOTE: We previously held entered span guards across awaits for nested
    // span context. That made the `run()` future `!Send`, which prevents
    // SDK consumers from `tokio::spawn(run(config))`. Span context is
    // attached to events via fields on the `info_span!` calls below; the
    // missing nesting is an acceptable trade-off for `Send` correctness.
    let _root_span = info_span!("agent_block", script = %script_name);

    // ── .env ──────────────────────────────────────────────────────
    // Load .env from project_root if present. Variables are merged into
    // the process environment so Lua's `std.env.get()` picks them up.
    let env_path = config.project_root.join(".env");
    match dotenvy::from_path(&env_path) {
        Ok(()) => info!(path = %env_path.display(), ".env loaded"),
        Err(dotenvy::Error::Io(_)) => {} // file not found — fine
        Err(e) => tracing::warn!(path = %env_path.display(), error = %e, ".env parse error"),
    }

    // ── Init ──────────────────────────────────────────────────────
    let _init_span = info_span!("init");

    // ── EventBus channel ─────────────────────────────────────────────
    // Construct the bounded mpsc BEFORE MeshAgent::connect so the relay
    // handler can hold a `bus_tx` clone and forward incoming requests
    // into the dispatcher. Capacity is ENV-driven (see bridge::config).
    let bus_capacity = crate::bridge::config::bus_capacity();
    let (bus_tx, bus_rx) = mpsc::channel::<Event>(bus_capacity);
    let event_bus = Arc::new(Mutex::new(Some(EventBus::new(bus_rx))));

    // ── Pre-install host-side Rust handlers ───────────────────────────
    // SDK consumers attach Rust handlers via `BlockConfig.host_handlers`
    // so that script-side `bus.emit(kind, payload)` is captured by a Rust
    // `Arc<dyn Handler>` instead of being dispatched to a Lua function.
    // Registered here (before any Lua bridge registers handlers and before
    // `bus.serve` takes ownership of the bus) so the EventBus already
    // carries the host handlers when the script starts.
    // Install host-side Rust handlers: kind-specific entries from
    // `host_handlers` and, when set, the kind-agnostic `host_handler`
    // (registered via `on_any` as the fallback for unmatched kinds).
    // SDK-embed 1-shot callers typically only set `host_handler`.
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

    // ── auto-serve: background dispatcher for SDK-embed callers ───────
    // When `auto_serve_bus` is on and at least one host-side handler
    // (kind-specific or kind-agnostic) is installed, take the EventBus
    // out of the Mutex *before* the script runs and spawn the dispatcher
    // loop on the runtime. This lets `bus.emit(kind, payload)` from the
    // script reach the host handler without requiring the script to call
    // `bus.serve()` (which blocks on signals and never returns under
    // programmatic embedding).
    let auto_serve = config.auto_serve_bus && (has_kind_handlers || has_any_handler);
    let auto_serve_state: Option<(tokio::task::JoinHandle<()>, CancellationToken)> = if auto_serve {
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

    let mesh_agent = if let Some(ref relay_url) = config.relay_url {
        let keypair = match &secret_key_resolved {
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
        Some(Arc::new(agent))
    } else {
        None
    };

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

    // ── SQLite init (kv + sql get separate DB files) ──────────────────────
    // BlockConfig overrides take precedence; otherwise the env-driven
    // resolution in `bridge::config::*` applies (see crate docs).
    let sql_path = match &config.sql_path {
        Some(p) => p.clone(),
        None => crate::bridge::config::sql_path().map_err(BlockError::Runtime)?,
    };
    let (sql_conn, sql_interrupt) = open_sqlite(&sql_path, "sql")?;

    let kv_path = match &config.kv_path {
        Some(p) => p.clone(),
        None => crate::bridge::config::kv_path().map_err(BlockError::Runtime)?,
    };
    let (kv_conn, kv_interrupt) = open_sqlite(&kv_path, "kv")?;

    let ts_path = match &config.ts_path {
        Some(p) => p.clone(),
        None => crate::bridge::config::ts_path().map_err(BlockError::Runtime)?,
    };
    let (ts_conn, ts_interrupt) = open_sqlite(&ts_path, "ts")?;

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
    let prompt = prompt_resolved.clone();
    let context = context_resolved.clone();

    // ── main Isle ─────────────────────────────────────────────────
    let (isle, driver) = AsyncIsle::spawn(build_isle_init(
        script_name.clone(),
        script_dir.clone(),
        blocks_paths.clone(),
        prompt.clone(),
        context.clone(),
        config.extra_globals.clone(),
    ))
    .await
    .map_err(|e| BlockError::Runtime(format!("AsyncIsle spawn failed: {e}")))?;
    let isle = Arc::new(isle);

    // ── handler Isle (sequential, dependencies are trivial) ────────
    let (handler_isle, handler_driver) = spawn_handler_isle(
        script_name.clone(),
        script_dir.clone(),
        blocks_paths.clone(),
        prompt,
        context,
        config.extra_globals.clone(),
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
        mesh_agent,
        mcp_manager: Arc::clone(&mcp_manager),
        http_client,
        sql_conn,
        sql_interrupt,
        kv_conn,
        kv_interrupt,
        ts_conn,
        ts_interrupt,
        isle: Arc::clone(&isle),
        handler_isle: Arc::clone(&handler_isle),
        bus_tx: bus_tx.clone(),
        event_bus: Arc::clone(&event_bus),
    };

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

    // ── Inject host_tools into the Lua tool registry ───────────────
    // Done after `bridge::register_all` so `_TOOL_REGISTRY` exists.
    // Each entry becomes an Anthropic-shaped tool spec table
    //   { name, schema = { description, input_schema }, handler, group? }
    // where `handler` is a Lua async function that bridges back into
    // the supplied `ToolHandler::call`. Lua-side `tool.list()` /
    // `tool.schema()` / `agent.run` see these uniformly with native
    // Lua-defined tools.
    if !config.host_tools.is_empty() {
        let host_tools = config.host_tools.clone();
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
                let input_schema_lua =
                    crate::bridge::json_to_lua(lua, tool.input_schema.clone())
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
    }

    drop(_init_span);

    // ── Execute ───────────────────────────────────────────────────
    // When `shutdown_token` is supplied, race the script future against
    // the caller's cancellation signal. On cancel, propagate to the Isle
    // via the AsyncTask's cancel token so the debug hook unwinds the Lua
    // VM, then continue into the shutdown sequence below (we still want
    // to release MCP/mesh handles and join the auto-serve dispatcher
    // before returning).
    let script_result: Result<(), BlockError> = {
        let _exec_span = info_span!("execute", script = %script_name);

        let mut task = isle.spawn_coroutine_eval(&script_source);
        let task_cancel = task.cancel_token().clone();
        match config.shutdown_token.as_ref() {
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
                    res = &mut task => res.map(|_| ()).map_err(|e| BlockError::Script(format!("{e}"))),
                }
            }
            None => (&mut task)
                .await
                .map(|_| ())
                .map_err(|e| BlockError::Script(format!("{e}"))),
        }
    };

    // ── auto-serve drain + cancel ─────────────────────────────────
    // Let the dispatcher drain events queued by the script, then signal
    // shutdown and bound the join. Mirrors `bus.serve`'s grace pattern.
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

    // ── Shutdown ──────────────────────────────────────────────────
    {
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
    }

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
struct BusRelayHandler {
    tx: mpsc::Sender<Event>,
}

impl BusRelayHandler {
    fn new(tx: mpsc::Sender<Event>) -> Self {
        Self { tx }
    }
}

/// Bound used for both the mesh-adapter ack wait and other source timeouts.
const BUS_ACK_TIMEOUT: Duration = Duration::from_secs(30);

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
        let (ack_tx, ack_rx) = oneshot::channel();
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
