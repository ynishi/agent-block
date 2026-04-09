//! mcp.* — MCP server client bridge (async).
//!
//! All functions use `create_async_function` so that Lua coroutines
//! yield while waiting for MCP server I/O.

use mlua::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::mcp_client::McpManager;

use super::{json_to_lua, lua_to_json};

pub fn register(lua: &Lua, manager: &Arc<Mutex<McpManager>>) -> LuaResult<()> {
    let mcp_tbl = lua.create_table()?;

    // mcp.connect(name, command, args)
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "connect",
            lua.create_async_function(
                move |_, (name, command, args): (String, String, Option<LuaTable>)| {
                    let mgr = Arc::clone(&mgr);
                    async move {
                        let args: Vec<String> = match args {
                            Some(tbl) => {
                                let mut v = Vec::new();
                                for pair in tbl.pairs::<LuaValue, String>() {
                                    let (_, s) = pair?;
                                    v.push(s);
                                }
                                v
                            }
                            None => Vec::new(),
                        };
                        mgr.lock()
                            .await
                            .connect(&name, &command, &args)
                            .await
                            .map_err(LuaError::external)
                    }
                },
            )?,
        )?;
    }

    // mcp.list_tools(name)
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "list_tools",
            lua.create_async_function(move |lua, name: String| {
                let mgr = Arc::clone(&mgr);
                async move {
                    let result = mgr.lock().await.list_tools(&name).await;

                    let tbl = lua.create_table()?;
                    match result {
                        Ok(val) => {
                            tbl.set("ok", true)?;
                            let tools = val
                                .get("tools")
                                .cloned()
                                .unwrap_or(serde_json::Value::Array(vec![]));
                            tbl.set("tools", json_to_lua(&lua, tools)?)?;
                        }
                        Err(e) => {
                            tbl.set("ok", false)?;
                            tbl.set("error", e.to_string())?;
                        }
                    }
                    Ok(tbl)
                }
            })?,
        )?;
    }

    // mcp.call(name, tool_name, arguments)
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "call",
            lua.create_async_function(
                move |lua, (name, tool_name, arguments): (String, String, Option<LuaValue>)| {
                    let mgr = Arc::clone(&mgr);
                    async move {
                        let args_json = match arguments {
                            Some(v) => lua_to_json(&lua, v)?,
                            None => serde_json::json!({}),
                        };

                        let result = mgr
                            .lock()
                            .await
                            .call_tool(&name, &tool_name, args_json)
                            .await;

                        let tbl = lua.create_table()?;
                        match result {
                            Ok(val) => {
                                tbl.set("ok", true)?;
                                let content = val
                                    .get("content")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Array(vec![]));
                                tbl.set("content", json_to_lua(&lua, content)?)?;
                            }
                            Err(e) => {
                                tbl.set("ok", false)?;
                                tbl.set("error", e.to_string())?;
                            }
                        }
                        Ok(tbl)
                    }
                },
            )?,
        )?;
    }

    // mcp.disconnect(name)
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "disconnect",
            lua.create_async_function(move |_, name: String| {
                let mgr = Arc::clone(&mgr);
                async move {
                    mgr.lock()
                        .await
                        .disconnect(&name)
                        .await
                        .map_err(LuaError::external)
                }
            })?,
        )?;
    }

    lua.globals().set("mcp", mcp_tbl)?;
    Ok(())
}
