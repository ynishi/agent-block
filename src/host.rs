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
use tokio::sync::RwLock;

use mlua_isle::AsyncIsle;
use tracing::{info, info_span, warn};

use crate::bridge;
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

    let mesh_agent = if let Some(ref relay_url) = config.relay_url {
        let keypair = agent_mesh_core::identity::AgentKeypair::generate();
        let acl = agent_mesh_core::acl::AclPolicy {
            default_deny: false,
            rules: vec![],
        };
        let handler: Arc<dyn agent_mesh_sdk::RequestHandler> = Arc::new(NoopHandler);
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

    let ctx = HostContext {
        project_root,
        mesh_agent,
        mcp_manager: Arc::clone(&mcp_manager),
        http_client,
        sql_conn,
        sql_interrupt,
        kv_conn,
        kv_interrupt,
    };

    let script_path = config.script_path.clone();
    let script_dir = script_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());

    let script_name_for_lua = script_name.clone();
    let (isle, driver) = AsyncIsle::spawn(move |lua| {
        // Set script name before registering bridges (used by log.* for attribution)
        lua.globals()
            .set("_SCRIPT_NAME", script_name_for_lua.as_str())?;

        mlua_batteries::register_all(lua, "std")?;
        bridge::register_all(lua, &ctx)?;

        // ── package.path ──────────────────────────────────────────────
        // Priority: script_dir > project_root/blocks/ > exe_dir/blocks/ > default
        let package: mlua::Table = lua.globals().get("package")?;
        let current_path: String = package.get("path")?;
        let blocks_paths = build_blocks_path(&ctx.project_root);
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
    })
    .await
    .map_err(|e| BlockError::Runtime(format!("AsyncIsle spawn failed: {e}")))?;

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
    }

    Ok(())
}

/// No-op request handler (placeholder until mesh.on is implemented).
struct NoopHandler;

#[async_trait::async_trait]
impl agent_mesh_sdk::RequestHandler for NoopHandler {
    async fn handle(
        &self,
        _from: &agent_mesh_core::identity::AgentId,
        _payload: &serde_json::Value,
        _cancel: agent_mesh_sdk::CancelToken,
    ) -> serde_json::Value {
        serde_json::json!({"error": "no handler registered"})
    }
}
