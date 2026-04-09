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

pub mod http;
pub mod log;
pub mod mcp;
pub mod mesh;
pub mod sh;
pub mod tool;

use mlua::prelude::*;

use crate::host::HostContext;

/// Convert a Lua value to a serde_json::Value.
pub fn lua_to_json(lua: &Lua, val: LuaValue) -> LuaResult<serde_json::Value> {
    use mlua::serde::LuaSerdeExt;
    lua.from_value(val)
}

/// Convert a serde_json::Value to a Lua value.
pub fn json_to_lua(lua: &Lua, val: serde_json::Value) -> LuaResult<LuaValue> {
    use mlua::serde::LuaSerdeExt;
    lua.to_value(&val)
}

/// Register all bridge APIs into the Lua state.
///
/// Note: `fs`, `env`, `json`, `path`, `time` are provided by mlua-batteries
/// (registered as `std.*` in host.rs). This function registers only
/// agent-block-specific APIs.
pub fn register_all(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    mesh::register(lua, ctx)?;
    sh::register(lua, ctx)?;
    tool::register(lua)?;
    log::register(lua, ctx)?;
    mcp::register(lua, &ctx.mcp_manager)?;
    http::register(lua, ctx)?;
    Ok(())
}
