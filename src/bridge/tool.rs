//! tool.* — Lua-side tool registry for LLM function calling.

use crate::bridge::obs;
use mlua::prelude::*;

/// Register tool.* Lua API.
///
/// Tool registry is maintained in Lua globals (_TOOL_REGISTRY table).
/// Rust provides thin helpers: register/call/list/schema.
pub fn register(lua: &Lua) -> LuaResult<()> {
    let script_name: String = lua
        .globals()
        .get::<Option<String>>("_SCRIPT_NAME")?
        .unwrap_or_else(|| "unknown".to_string());

    // Initialize registry in Lua globals
    let registry = lua.create_table()?;
    lua.globals().set("_TOOL_REGISTRY", registry)?;

    let tool_tbl = lua.create_table()?;

    // tool.register(name, schema, handler_fn)
    let script_name_register = script_name.clone();
    tool_tbl.set(
        "register",
        lua.create_function(
            move |lua, (name, schema, handler): (String, LuaValue, LuaFunction)| {
                let registry: LuaTable = lua.globals().get("_TOOL_REGISTRY")?;
                let entry = lua.create_table()?;
                entry.set("name", name.clone())?;
                entry.set("schema", schema)?;
                entry.set("handler", handler)?;
                registry.set(name.clone(), entry)?;
                tracing::info!(
                    target: "lua",
                    script = %script_name_register,
                    "{}",
                    obs::obs_line(
                        "tool",
                        "tool_register",
                        &obs::obs_context(None),
                        &[("tool", name.as_str())],
                    ),
                );
                Ok(())
            },
        )?,
    )?;

    // tool.call(name, input) -> result or error
    //
    // Async so that handlers may invoke async stdlib functions (e.g.
    // `std.sql.query`, `http.request`, `mcp.call`) via coroutine yield.
    // Purely synchronous handlers still work unchanged.
    let script_name_call = script_name.clone();
    tool_tbl.set(
        "call",
        lua.create_async_function(move |lua, (name, input): (String, LuaValue)| {
            let script_name = script_name_call.clone();
            async move {
                tracing::info!(
                    target: "lua",
                    script = %script_name,
                    "{}",
                    obs::obs_line(
                        "tool",
                        "tool_call",
                        &obs::obs_context(None),
                        &[("tool", name.as_str())],
                    ),
                );
                let registry: LuaTable = lua.globals().get("_TOOL_REGISTRY")?;
                let entry: Option<LuaTable> = registry.get(name.clone())?;
                match entry {
                    None => {
                        tracing::warn!(
                            target: "lua",
                            script = %script_name,
                            "{}",
                            obs::obs_line(
                                "tool",
                                "tool_result",
                                &obs::obs_context(None),
                                &[("tool", name.as_str()), ("ok", "false")],
                            ),
                        );
                        Err(LuaError::external(format!("tool not found: {name}")))
                    }
                    Some(e) => {
                        let handler: LuaFunction = e.get("handler")?;
                        match handler.call_async::<LuaValue>(input).await {
                            Ok(v) => {
                                tracing::info!(
                                    target: "lua",
                                    script = %script_name,
                                    "{}",
                                    obs::obs_line(
                                        "tool",
                                        "tool_result",
                                        &obs::obs_context(None),
                                        &[("tool", name.as_str()), ("ok", "true")],
                                    ),
                                );
                                Ok(v)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    target: "lua",
                                    script = %script_name,
                                    "{}",
                                    obs::obs_line(
                                        "tool",
                                        "tool_result",
                                        &obs::obs_context(None),
                                        &[("tool", name.as_str()), ("ok", "false")],
                                    ),
                                );
                                Err(e)
                            }
                        }
                    }
                }
            }
        })?,
    )?;

    // tool.list() -> array of names
    tool_tbl.set(
        "list",
        lua.create_function(|lua, ()| {
            let registry: LuaTable = lua.globals().get("_TOOL_REGISTRY")?;
            let names = lua.create_table()?;
            let mut idx = 1;
            for pair in registry.pairs::<String, LuaTable>() {
                let (name, _) = pair?;
                names.set(idx, name)?;
                idx += 1;
            }
            Ok(names)
        })?,
    )?;

    // tool.schema() -> JSON array of Anthropic tool definitions
    // Each entry: { name = "...", description = "...", input_schema = {...} }
    tool_tbl.set(
        "schema",
        lua.create_function(|lua, ()| {
            let registry: LuaTable = lua.globals().get("_TOOL_REGISTRY")?;
            let arr = lua.create_table()?;
            let mut idx = 1;
            for pair in registry.pairs::<String, LuaTable>() {
                let (_, entry) = pair?;
                let name: String = entry.get("name")?;
                let schema: LuaTable = entry.get("schema")?;
                let description: String = schema.get("description")?;
                let input_schema: LuaValue = schema.get("input_schema")?;

                let tool_def = lua.create_table()?;
                tool_def.set("name", name)?;
                tool_def.set("description", description)?;
                tool_def.set("input_schema", input_schema)?;
                arr.set(idx, tool_def)?;
                idx += 1;
            }
            Ok(arr)
        })?,
    )?;

    lua.globals().set("tool", tool_tbl)?;
    Ok(())
}
