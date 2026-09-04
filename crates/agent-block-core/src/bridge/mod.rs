//! Lua Stdlib Bridge — injects all `*.*` global APIs into the Lua VM.
//!
//! Each submodule registers one namespace:
//!
//! | Module | Lua namespace | Purpose |
//! |--------|--------------|---------|
//! | `mesh` | `mesh.*`     | Agent-to-agent mesh communication |
//! | `mcp`  | `mcp.*`      | MCP server management |
//! | `sh`   | `sh.*`       | Shell command execution |
//! | `tool` | `tool.*`     | Tool registry (define and call tools from Lua) |
//! | `http` | `http.*`     | Async HTTP client |
//! | `log`  | `log.*`, `env.*` | Logging and environment access |
//! | `ts`   | `std.ts.*`   | SQLite-backed time-series primitive (in-tree) |
//! | `knl`  | `knl.*`      | Kernel syscall layer (session / append-only history / budget) |

pub mod bus;
pub mod config;
pub mod fs;
pub mod http;
pub mod knl;
#[cfg(feature = "sqlite")]
pub mod kv;
pub mod llm;
pub mod log;
pub mod mcp;
#[cfg(feature = "mesh")]
pub mod mesh;
pub mod sh;
#[cfg(feature = "sqlite")]
pub mod sql;
pub mod task;
pub mod tool;
#[cfg(feature = "sqlite")]
pub mod ts;

use mlua::prelude::*;

use crate::host::HostContext;

// Re-export `obs` from agent-block-types so that existing
// `crate::bridge::obs::*` paths inside core keep compiling without
// duplicating the module body.
pub use agent_block_types::obs;

// Re-export the Lua ↔ JSON converters from agent-block-mcp.  They live in
// the MCP crate because the rmcp handler depends on them; core only needs
// to forward `lua_to_json` / `json_to_lua` for the in-process bridges
// (llm / mesh / mcp.lua) that historically reached `crate::bridge::*`.
pub use agent_block_mcp::lua_json::{json_to_lua, lua_to_json};

/// Register bridge APIs shared between main VM and handler VM.
///
/// Registers everything except `bus::*`.  Split out from `register_all` so
/// the handler-side Isle can re-use the same set of bridges without
/// installing the main-VM-only `bus` global.
///
/// `is_handler_side` is forwarded to `mesh::register` so the handler Isle
/// can skip the `mesh.on` alias (which depends on `bus.on` and would fail
/// because the handler Isle does not expose a `bus` global).
fn register_non_bus_bridges(lua: &Lua, ctx: &HostContext, is_handler_side: bool) -> LuaResult<()> {
    #[cfg(feature = "mesh")]
    mesh::register(lua, ctx, is_handler_side)?;
    #[cfg(not(feature = "mesh"))]
    let _ = is_handler_side;
    sh::register(lua, ctx)?;
    tool::register(lua)?;
    // After tool::register — fs_tools.lua needs the `tool` global.
    fs::register(lua, std::sync::Arc::clone(&ctx.fs_snapshots))?;
    log::register(lua, ctx)?;
    mcp::register(lua, ctx)?;
    http::register(lua, ctx)?;
    llm::register(lua)?;
    knl::register(lua, ctx.knl_drivers.clone())?;
    #[cfg(feature = "sqlite")]
    kv::register(lua, ctx)?;
    #[cfg(feature = "sqlite")]
    sql::register(lua, ctx)?;
    #[cfg(feature = "sqlite")]
    ts::register(lua, ctx)?;
    task::register(lua)?;
    Ok(())
}

/// Register all bridge APIs into the Lua state (main Isle).
///
/// Note: `fs`, `env`, `json`, `path`, `time` are provided by mlua-batteries
/// (registered as `std.*` in host.rs). This function registers only
/// agent-block-specific APIs.
pub fn register_all(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    // bus must register before mesh — the mesh.on alias (see
    // bridge/mesh.rs) reads the `bus` global produced here.
    bus::register(lua, ctx)?;
    register_non_bus_bridges(lua, ctx, false)
}

/// Register bridge APIs for the handler Isle.
///
/// The handler Isle runs Lua handlers forwarded from the main Isle's
/// `bus.on` / `bus.on_any` via bytecode transfer. It therefore needs the
/// dispatcher-side globals (`__bus_handlers`, `__bus_on_any`,
/// `__bus_dispatch`) installed by
/// [`bus::install_bus_dispatcher_on_handler_isle`], but does **not** expose
/// the `bus.*` Lua table — nested `bus.on(...)` from inside a handler is
/// intentionally unsupported.
pub fn register_all_handler_side(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    bus::install_bus_dispatcher_on_handler_isle(lua)?;
    agent_block_mcp::handler::install_mcp_dispatcher_on_handler_isle(lua)?;
    register_non_bus_bridges(lua, ctx, true)
}
