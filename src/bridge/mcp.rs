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
use serde_json::Map;
use std::sync::Arc;

use crate::host::HostContext;

use super::{json_to_lua, lua_to_json};

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let manager = &ctx.mcp_manager;
    let mcp_tbl = lua.create_table()?;
    let script_name: String = lua
        .globals()
        .get::<Option<String>>("_SCRIPT_NAME")?
        .unwrap_or_else(|| "unknown".to_string());
    let fallback_agent_id = ctx.mesh_agent.as_ref().map(|a| a.agent_id().to_string());

    // mcp.connect(name, command, args)
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "connect",
            lua.create_async_function(
                move |_, (name, command, args): (String, String, Option<LuaTable>)| {
                    let mgr = Arc::clone(&mgr);
                    async move {
                        // Iterate by integer index (1..=len) so argv order is
                        // preserved regardless of table layout. `pairs` gives
                        // no ordering guarantee for integer-keyed tables.
                        let args: Vec<String> = match args {
                            Some(tbl) => {
                                let len = tbl.raw_len();
                                let mut v = Vec::with_capacity(len);
                                for i in 1..=len {
                                    v.push(tbl.raw_get::<String>(i)?);
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
    //
    // Return shape:
    //   { ok=true,  content=[...], is_error=bool, structured_content=... }  (RPC success)
    //   { ok=false, error="..." }                                           (transport/protocol)
    //
    // `ok` is reserved for protocol / transport / timeout failures.
    // `is_error` mirrors the server-reported `isError` from `CallToolResult`
    // so tool-execution errors reach the LLM unchanged (MCP spec intent).
    {
        let mgr = Arc::clone(manager);
        let fallback_agent_id = fallback_agent_id.clone();
        let script_name = script_name.clone();
        mcp_tbl.set(
            "call",
            lua.create_async_function(
                move |lua, (name, tool_name, arguments): (String, String, Option<LuaValue>)| {
                    let mgr = Arc::clone(&mgr);
                    let fallback_agent_id = fallback_agent_id.clone();
                    let script_name = script_name.clone();
                    async move {
                        // None → Null (mcp_client treats Null as "no arguments").
                        let mut args_json = match arguments {
                            Some(v) => lua_to_json(&lua, v)?,
                            None => serde_json::Value::Null,
                        };
                        inject_obs_context(&mut args_json, fallback_agent_id.as_deref());
                        tracing::info!(
                            target: "lua",
                            script = %script_name,
                            "{}",
                            obs_line(
                                "mcp_call",
                                &obs_context(fallback_agent_id.as_deref()),
                                &[("server", name.as_str()), ("tool", tool_name.as_str())],
                            )
                        );

                        let result = mgr
                            .read()
                            .await
                            .call_tool(&name, &tool_name, args_json)
                            .await;

                        let tbl = lua.create_table()?;
                        match result {
                            Ok(val) => {
                                tracing::info!(
                                    target: "lua",
                                    script = %script_name,
                                    "{}",
                                    obs_line(
                                        "mcp_result",
                                        &obs_context(fallback_agent_id.as_deref()),
                                        &[("server", name.as_str()), ("tool", tool_name.as_str()), ("ok", "true")],
                                    )
                                );
                                tbl.set("ok", true)?;
                                let content = val
                                    .get("content")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Array(vec![]));
                                tbl.set("content", json_to_lua(&lua, content)?)?;
                                let is_error = val
                                    .get("isError")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                tbl.set("is_error", is_error)?;
                                if let Some(sc) = val.get("structuredContent").cloned() {
                                    tbl.set("structured_content", json_to_lua(&lua, sc)?)?;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    target: "lua",
                                    script = %script_name,
                                    "{}",
                                    obs_line(
                                        "mcp_result",
                                        &obs_context(fallback_agent_id.as_deref()),
                                        &[("server", name.as_str()), ("tool", tool_name.as_str()), ("ok", "false")],
                                    )
                                );
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

fn inject_obs_context(args_json: &mut serde_json::Value, fallback_agent_id: Option<&str>) {
    fn insert_obs(into: &mut Map<String, serde_json::Value>, fallback_agent_id: Option<&str>) {
        if into.contains_key("__ab_obs") {
            return;
        }
        let mut obs = Map::<String, serde_json::Value>::new();
        if let Ok(v) = std::env::var("AGENT_BLOCK_TRACE_ID") {
            if !v.is_empty() {
                obs.insert("trace_id".to_string(), serde_json::Value::String(v));
            }
        }
        if let Ok(v) = std::env::var("AGENT_BLOCK_RUN_ID") {
            if !v.is_empty() {
                obs.insert("run_id".to_string(), serde_json::Value::String(v));
            }
        }
        let agent_id = std::env::var("AGENT_BLOCK_AGENT_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| fallback_agent_id.map(ToString::to_string));
        if let Some(v) = agent_id {
            obs.insert("agent_id".to_string(), serde_json::Value::String(v));
        }
        if let Ok(v) = std::env::var("AGENT_BLOCK_AGENT_NAME") {
            if !v.is_empty() {
                obs.insert("agent_name".to_string(), serde_json::Value::String(v));
            }
        }
        if !obs.is_empty() {
            into.insert("__ab_obs".to_string(), serde_json::Value::Object(obs));
        }
    }

    match args_json {
        serde_json::Value::Object(obj) => insert_obs(obj, fallback_agent_id),
        serde_json::Value::Null => {
            let mut obj = Map::<String, serde_json::Value>::new();
            insert_obs(&mut obj, fallback_agent_id);
            if !obj.is_empty() {
                *args_json = serde_json::Value::Object(obj);
            }
        }
        _ => {}
    }
}

fn obs_context(fallback_agent_id: Option<&str>) -> (String, String, String, String) {
    let trace_id = std::env::var("AGENT_BLOCK_TRACE_ID").unwrap_or_default();
    let run_id = std::env::var("AGENT_BLOCK_RUN_ID").unwrap_or_default();
    let agent_id = std::env::var("AGENT_BLOCK_AGENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| fallback_agent_id.map(ToString::to_string))
        .unwrap_or_default();
    let agent_name = std::env::var("AGENT_BLOCK_AGENT_NAME").unwrap_or_default();
    (trace_id, run_id, agent_id, agent_name)
}

fn obs_line(event: &str, ctx: &(String, String, String, String), extra: &[(&str, &str)]) -> String {
    let mut parts = vec![
        "prefix=ab.obs".to_string(),
        format!("event={}", event),
        "component=mcp".to_string(),
        format!("trace_id={}", kv_escape(&ctx.0)),
        format!("run_id={}", kv_escape(&ctx.1)),
        format!("agent_id={}", kv_escape(&ctx.2)),
        format!("agent_name={}", kv_escape(&ctx.3)),
    ];
    for (k, v) in extra {
        parts.push(format!("{}={}", k, kv_escape(v)));
    }
    parts.join(" ")
}

fn kv_escape(v: &str) -> String {
    if v.is_empty() {
        "\"\"".to_string()
    } else if v.chars().any(|c| c.is_whitespace() || c == '=') {
        serde_json::Value::String(v.to_string()).to_string()
    } else {
        v.to_string()
    }
}
