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

pub mod bus;
pub mod config;
pub mod http;
pub mod kv;
pub mod llm;
pub mod log;
pub mod mcp;
pub mod mesh;
pub mod obs;
pub mod sh;
pub mod sql;
pub mod task;
pub mod tool;

use mlua::prelude::*;

use crate::host::HostContext;

/// Convert a Lua value to a serde_json::Value.
///
/// Round-trips with `json_to_lua` and `std.json.encode` (mlua-batteries).
/// Lua `nil` maps to JSON `null`.  Unsupported types (functions, userdata
/// other than `null`) yield an error so that callers do not silently emit
/// malformed JSON.
pub fn lua_to_json(_lua: &Lua, val: LuaValue) -> LuaResult<serde_json::Value> {
    lua_to_json_inner(&val, 0)
}

fn lua_to_json_inner(val: &LuaValue, depth: usize) -> LuaResult<serde_json::Value> {
    const MAX_DEPTH: usize = 128;
    if depth > MAX_DEPTH {
        return Err(LuaError::external(format!(
            "Lua table nesting too deep for JSON (limit: {MAX_DEPTH})"
        )));
    }
    match val {
        LuaValue::Nil => Ok(serde_json::Value::Null),
        // mlua serde uses LightUserData(null_ptr) for JSON null.  Treat it
        // the same as Nil so values produced by `json_to_lua` round-trip.
        LuaValue::LightUserData(u) if u.0.is_null() => Ok(serde_json::Value::Null),
        LuaValue::Boolean(b) => Ok(serde_json::Value::Bool(*b)),
        LuaValue::Integer(i) => Ok(serde_json::Value::Number((*i).into())),
        LuaValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .ok_or_else(|| LuaError::external(format!("cannot convert {n} to JSON number"))),
        LuaValue::String(s) => Ok(serde_json::Value::String(s.to_str()?.to_string())),
        LuaValue::Table(t) => {
            let len = t.raw_len();
            if len > 0 {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    let v: LuaValue = t.raw_get(i)?;
                    arr.push(lua_to_json_inner(&v, depth + 1)?);
                }
                Ok(serde_json::Value::Array(arr))
            } else {
                let mut map = serde_json::Map::new();
                for pair in t.clone().pairs::<LuaValue, LuaValue>() {
                    let (k, v) = pair?;
                    let key = match k {
                        LuaValue::String(s) => s.to_str()?.to_string(),
                        LuaValue::Integer(i) => i.to_string(),
                        LuaValue::Number(n) => n.to_string(),
                        other => {
                            return Err(LuaError::external(format!(
                                "unsupported table key type for JSON: {}",
                                other.type_name()
                            )));
                        }
                    };
                    map.insert(key, lua_to_json_inner(&v, depth + 1)?);
                }
                Ok(serde_json::Value::Object(map))
            }
        }
        other => Err(LuaError::external(format!(
            "unsupported type for JSON conversion: {}",
            other.type_name()
        ))),
    }
}

/// Convert a serde_json::Value to a Lua value.
///
/// JSON `null` maps to the `LightUserData(null_ptr)` sentinel
/// (`mlua::Value::NULL`), which is the same representation `lua_to_json`
/// accepts on the way out — so the round-trip is symmetric.  Using the
/// sentinel rather than Lua `nil` means JSON `null` values survive being
/// placed into Lua tables (tables cannot hold `nil`), so SQL NULL columns
/// and MCP/LLM JSON payloads do not lose the distinction between "null"
/// and "absent".  Agents can compare a value against the exposed
/// `std.sql.null` constant to detect it.
///
/// Note: this differs from mlua-batteries' `std.json.decode`, which keeps
/// the Lua-idiomatic "null → nil" lowering for `json.decode` itself.  Our
/// bridge paths (sql / kv / mcp / mesh / llm) prefer round-trip fidelity.
pub fn json_to_lua(lua: &Lua, val: serde_json::Value) -> LuaResult<LuaValue> {
    json_to_lua_inner(lua, &val, 0)
}

fn json_to_lua_inner(lua: &Lua, val: &serde_json::Value, depth: usize) -> LuaResult<LuaValue> {
    const MAX_DEPTH: usize = 128;
    if depth > MAX_DEPTH {
        return Err(LuaError::external(format!(
            "JSON nesting too deep (limit: {MAX_DEPTH})"
        )));
    }
    match val {
        serde_json::Value::Null => Ok(LuaValue::NULL),
        serde_json::Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(LuaValue::Number(f))
            } else {
                Err(LuaError::external(format!(
                    "JSON number {n} is not representable as i64 or f64"
                )))
            }
        }
        serde_json::Value::String(s) => lua.create_string(s).map(LuaValue::String),
        serde_json::Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                table.set(i + 1, json_to_lua_inner(lua, v, depth + 1)?)?;
            }
            Ok(LuaValue::Table(table))
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table()?;
            for (k, v) in map {
                table.set(k.as_str(), json_to_lua_inner(lua, v, depth + 1)?)?;
            }
            Ok(LuaValue::Table(table))
        }
    }
}

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
    mesh::register(lua, ctx, is_handler_side)?;
    sh::register(lua, ctx)?;
    tool::register(lua)?;
    log::register(lua, ctx)?;
    mcp::register(lua, ctx)?;
    http::register(lua, ctx)?;
    llm::register(lua)?;
    kv::register(lua, ctx)?;
    sql::register(lua, ctx)?;
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
    register_non_bus_bridges(lua, ctx, true)
}
