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

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use mlua_isle::AsyncIsle;
use tracing::{info, info_span};

use crate::bridge;
use crate::error::{BlockError, BlockResult};
use crate::mcp_client::McpManager;

pub struct BlockConfig {
    pub script_path: PathBuf,
    pub project_root: PathBuf,
    pub relay_url: Option<String>,
}

/// Shared context passed into Lua bridge functions.
#[derive(Clone)]
pub struct HostContext {
    pub project_root: PathBuf,
    pub mesh_agent: Option<Arc<agent_mesh_sdk::MeshAgent>>,
    pub mcp_manager: Arc<Mutex<McpManager>>,
    /// Shared async HTTP client for `http.*` bridge.
    pub http_client: reqwest::Client,
}

pub async fn run(config: BlockConfig) -> BlockResult<()> {
    let script_name = config
        .script_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let root_span = info_span!("agent_block", script = %script_name);
    let _root_guard = root_span.enter();

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

    let mcp_manager = Arc::new(Mutex::new(McpManager::new()));

    // Resolve project_root to absolute path.
    // canonicalize() can fail if the path doesn't exist; fall back to
    // joining with current_dir to guarantee an absolute path.
    let project_root = config
        .project_root
        .canonicalize()
        .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(&config.project_root)))?;

    let http_client = reqwest::Client::new();

    let ctx = HostContext {
        project_root,
        mesh_agent,
        mcp_manager: Arc::clone(&mcp_manager),
        http_client,
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

        let package: mlua::Table = lua.globals().get("package")?;
        let current_path: String = package.get("path")?;
        let new_path = format!("{script_dir}/?.lua;{script_dir}/?/init.lua;{current_path}");
        package.set("path", new_path)?;

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

        mcp_manager.lock().await.disconnect_all().await?;

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
