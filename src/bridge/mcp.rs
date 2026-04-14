//! mcp.* — MCP server client bridge (async).
//!
//! All functions use `create_async_function` so that Lua coroutines
//! yield while waiting for MCP server I/O.
//!
//! The manager is held under `RwLock`:
//! - `connect` / `disconnect` take the write lock (they mutate the
//!   internal server map).
//! - `list_tools` / `call` take the read lock, so multiple RPCs — even
//!   against the same server — can be in flight simultaneously. The
//!   per-server multiplexing of concurrent requests is delegated to
//!   rmcp's `RunningService`, which tracks pending requests by ID
//!   internally over a channel-based peer.

use mlua::prelude::*;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::mcp_client::McpManager;

use super::{json_to_lua, lua_to_json};

pub fn register(lua: &Lua, manager: &Arc<RwLock<McpManager>>) -> LuaResult<()> {
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
                        mgr.write()
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
                    let result = mgr.read().await.list_tools(&name).await;

                    let tbl = lua.create_table()?;
                    match result {
                        Ok(val) => {
                            tbl.set("ok", true)?;
                            tbl.set("tools", json_to_lua(&lua, val)?)?;
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
                            .read()
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
                    mgr.write()
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
