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
    // After tool::register — the `fs_tools` module needs the `tool` global.
    fs::register(lua, std::sync::Arc::clone(&ctx.fs_snapshots))?;
    log::register(lua, ctx)?;
    mcp::register(lua, ctx)?;
    http::register(lua, ctx)?;
    llm::register(lua)?;
    knl::register(
        lua,
        ctx.knl_logs.clone(),
        ctx.knl_store.clone(),
        ctx.session_labels.clone(),
    )?;
    #[cfg(feature = "sqlite")]
    kv::register(lua, ctx)?;
    #[cfg(feature = "sqlite")]
    sql::register(lua, ctx)?;
    #[cfg(feature = "sqlite")]
    ts::register(lua, ctx.ts_isle.clone())?;
    task::register(lua)?;
    Ok(())
}

/// Install the Lua half of a bridge — `std.<x>.tool_specs` /
/// `std.<x>.register_tools`, the helpers a model is handed — onto the
/// `std.<std_key>` table the Rust half has just built.
///
/// The module is a LIBRARY: `fs_tools` and its three siblings are ordinary
/// modules that answer a table of functions, exactly as `agent`, `policy` and
/// every other embedded module do. They write nothing onto `std` themselves,
/// because a library is not wiring — which is what makes
/// `require("fs_tools")` a table a project can wrap. The wiring is here: each
/// exported FUNCTION is set on `std.<std_key>` under its own name. Other
/// exports (`M.shapes`) stay on the module, where a caller that wants the
/// contract reads it as `require("fs_tools").shapes`.
///
/// The module is found through `require`, so the project's vendored copy is
/// the one that runs. The Rust half of a bridge is registered on a VM whose
/// require registry is already installed (the Isle init closure in `host.rs`
/// runs first), so `require("<name>")` resolves the way every other module
/// does: `.agent-block/lib/<name>/` first, the embedded source last. That is
/// what makes `agent-block vendor fs_tools` a change to the tool surface
/// without an install — the same path a vendored `agent` or `policy` takes,
/// including the delegation idiom, since the vendored copy is installed from
/// whatever table it answers:
///
/// ```lua
/// local base = require("embedded.fs_tools")
/// local M = setmetatable({}, { __index = base })
/// function M.tool_specs(opts) return base.tool_specs(opts) end
/// return M
/// ```
///
/// The wrapper above defines ONE of the module's functions and inherits the
/// rest through `__index`, which raw iteration does not see — so the install
/// walks the `__index` chain, nearest table first, and a name already taken
/// at a nearer level is not looked for again. Without that walk a vendored
/// copy that overrode `tool_specs` would leave `std.fs.register_tools` nil:
/// the one idiom this shape exists for would half-work. A `__index` that is
/// a FUNCTION cannot be enumerated at all; a copy written that way installs
/// only what it holds itself, and the rest stays reachable through
/// `require("fs_tools")`.
///
/// A VM with no registry — a unit test's bare `Lua::new()` — cannot resolve
/// the name at all, and for that one case the embedded source is run
/// directly; the chunk answers the same table, and is installed the same way.
/// Only "not found" falls back: an error *inside* a copy (a syntax error in
/// what the project wrote) is the project's to see, not a reason to silently
/// run the embedded one instead.
pub(crate) fn load_tools_module(
    lua: &Lua,
    name: &str,
    std_key: &str,
    embedded: &str,
) -> LuaResult<()> {
    // `require` answers the module, or the sentinel when the name resolves
    // nowhere. A table cannot be the sentinel, so the two cannot be confused.
    let probe = format!(
        r#"local ok, mod = pcall(require, "{name}")
if ok then return mod end
if tostring(mod):find("module '{name}' not found", 1, true) then return false end
error(mod, 0)"#
    );
    let found: LuaValue = lua
        .load(&probe)
        .set_name(format!("require {name}"))
        .eval()?;
    let module = match found {
        LuaValue::Boolean(false) => lua.load(embedded).set_name(name).eval::<LuaValue>()?,
        other => other,
    };

    let module = match module {
        LuaValue::Table(table) => table,
        // A copy written against the older form assigned onto `std.<x>` as a
        // side effect and returned nothing (`require` then answers `true`).
        // Say which module and what it has to do, rather than installing
        // nothing and letting the tools go missing at the first call.
        other => {
            return Err(mlua::Error::external(format!(
                "the `{name}` module answered {} — a tool module is a library: it must end \
                 with `return M`, the table holding its functions. \
                 `agent-block vendor --force {name}` rewrites the copy from the current \
                 embedded source.",
                found_type(&other)
            )))
        }
    };

    lua.load(INSTALL_TOOLS_MODULE)
        .set_name(format!("install {name}"))
        .call::<()>((module, std_key))?;
    Ok(())
}

/// The install itself, in Lua because the `__index` walk is: `getmetatable`
/// and `rawget` say in four lines what the same traversal costs through the
/// Rust bindings, and this is the only place either is needed.
///
/// A name seen at a nearer level is not taken from a farther one, whatever
/// its type — a wrapper that replaced a function with a table meant to
/// replace it, not to fall through to the one it shadowed.
const INSTALL_TOOLS_MODULE: &str = r#"
local module, key = ...
local target = std[key]
local seen = {}
local level = module
while type(level) == "table" do
    for name, value in pairs(level) do
        if not seen[name] then
            seen[name] = true
            if type(value) == "function" then
                target[name] = value
            end
        end
    end
    local mt = getmetatable(level)
    level = type(mt) == "table" and rawget(mt, "__index") or nil
end
"#;

/// The Lua type name of a value, for the message above.
fn found_type(value: &LuaValue) -> &'static str {
    match value {
        LuaValue::Nil => "nil",
        LuaValue::Boolean(_) => "a boolean",
        _ => value.type_name(),
    }
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
