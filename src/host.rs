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
use crate::bus::{Event, EventBus};
use crate::error::{BlockError, BlockResult};
use crate::mcp_client::McpManager;

/// Embedded Lua sources for blocks/ StdPkg modules.
/// These are baked into the binary at compile time so `cargo install` works
/// without any extra file distribution.
const EMBEDDED_BLOCKS: &[(&str, &str)] = &[("agent", include_str!("../blocks/agent/init.lua"))];

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
    pub script_path: PathBuf,
    pub project_root: PathBuf,
    pub relay_url: Option<String>,
    /// Ed25519 secret key (64 hex chars). If `None`, a random keypair is
    /// generated. Required to talk to registry/ACL-gated hosted meshes.
    pub secret_key: Option<String>,
    /// Per-RPC timeout for every MCP round-trip (connect / list / call).
    /// Defaults to [`crate::mcp_client::DEFAULT_RPC_TIMEOUT`].
    pub mcp_rpc_timeout: Duration,
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
) -> impl FnOnce(&mlua::Lua) -> mlua::Result<()> + Send + 'static {
    move |lua| {
        // Set script name before registering bridges (used by log.* for attribution)
        lua.globals().set("_SCRIPT_NAME", script_name.as_str())?;

        mlua_batteries::register_all(lua, "std")?;

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
) -> BlockResult<(Arc<AsyncIsle>, AsyncIsleDriver)> {
    let init = build_isle_init(script_name, script_dir, blocks_paths);
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
    let script_name = config
        .script_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let root_span = info_span!("agent_block", script = %script_name);
    let _root_guard = root_span.enter();

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
    let _init_guard = info_span!("init").entered();

    // ── EventBus channel ─────────────────────────────────────────────
    // Construct the bounded mpsc BEFORE MeshAgent::connect so the relay
    // handler can hold a `bus_tx` clone and forward incoming requests
    // into the dispatcher. Capacity is ENV-driven (see bridge::config).
    let bus_capacity = crate::bridge::config::bus_capacity();
    let (bus_tx, bus_rx) = mpsc::channel::<Event>(bus_capacity);
    let event_bus = Arc::new(Mutex::new(Some(EventBus::new(bus_rx))));

    let mesh_agent = if let Some(ref relay_url) = config.relay_url {
        let keypair = match &config.secret_key {
            Some(hex_str) => {
                let bytes = hex_decode_32(hex_str)
                    .map_err(|e| BlockError::Runtime(format!("--secret-key: {e}")))?;
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

    let http_client = reqwest::Client::new();

    // ── SQLite init (kv + sql get separate DB files) ──────────────────────
    // All knobs are ENV-driven (see `bridge/config.rs`).
    let sql_path = crate::bridge::config::sql_path().map_err(BlockError::Runtime)?;
    let (sql_conn, sql_interrupt) = open_sqlite(&sql_path, "sql")?;

    let kv_path = crate::bridge::config::kv_path().map_err(BlockError::Runtime)?;
    let (kv_conn, kv_interrupt) = open_sqlite(&kv_path, "kv")?;

    let script_path = config.script_path.clone();
    let script_dir = script_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());

    // Precompute values captured by the init closure so we don't need to
    // move the full `HostContext` into it (HostContext now holds
    // `Arc<AsyncIsle>`, which is available only after `AsyncIsle::spawn`
    // returns — classic chicken-and-egg). All bridge registrations run in a
    // second pass via `isle.exec` below.
    let blocks_paths = build_blocks_path(&project_root);

    // ── main Isle ─────────────────────────────────────────────────
    let (isle, driver) = AsyncIsle::spawn(build_isle_init(
        script_name.clone(),
        script_dir.clone(),
        blocks_paths.clone(),
    ))
    .await
    .map_err(|e| BlockError::Runtime(format!("AsyncIsle spawn failed: {e}")))?;
    let isle = Arc::new(isle);

    // ── handler Isle (sequential, dependencies are trivial) ────────
    let (handler_isle, handler_driver) = spawn_handler_isle(
        script_name.clone(),
        script_dir.clone(),
        blocks_paths.clone(),
    )
    .await?;

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

    drop(_init_guard);

    // ── Execute ───────────────────────────────────────────────────
    {
        let _exec_guard = info_span!("execute", script = %script_name).entered();

        let script = std::fs::read_to_string(&script_path)
            .map_err(|e| BlockError::Script(format!("{}: {e}", script_path.display())))?;

        isle.coroutine_eval(&script)
            .await
            .map_err(|e| BlockError::Script(format!("{e}")))?;
    }

    // ── Shutdown ──────────────────────────────────────────────────
    {
        let _shutdown_guard = info_span!("shutdown").entered();

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

    Ok(())
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
