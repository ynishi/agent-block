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
use mlua_isle::IsleError;
use serde_json::Map;
use std::sync::Arc;

use crate::bridge::obs;
use crate::host::HostContext;

use super::{json_to_lua, lua_to_json};

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let manager = &ctx.mcp_manager;
    let handler_isle = Arc::clone(&ctx.handler_isle);
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
                            obs::obs_line(
                                "mcp",
                                "mcp_call",
                                &obs::obs_context(fallback_agent_id.as_deref()),
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
                                    obs::obs_line(
                                        "mcp",
                                        "mcp_result",
                                        &obs::obs_context(fallback_agent_id.as_deref()),
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
                                    obs::obs_line(
                                        "mcp",
                                        "mcp_result",
                                        &obs::obs_context(fallback_agent_id.as_deref()),
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

    // mcp.connect_http(name, url, opts)
    // opts: { auth_header = "..." } (optional)
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "connect_http",
            lua.create_async_function(
                move |lua, (name, url, opts): (String, String, Option<LuaValue>)| {
                    let mgr = Arc::clone(&mgr);
                    async move {
                        let opts_json = match opts {
                            Some(v) => match lua_to_json(&lua, v) {
                                Ok(j) => j,
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "mcp.connect_http: opts conversion failed, using empty opts"
                                    );
                                    serde_json::Value::Object(serde_json::Map::new())
                                }
                            },
                            None => serde_json::Value::Object(serde_json::Map::new()),
                        };
                        mgr.write()
                            .await
                            .connect_http(&name, &url, opts_json)
                            .await
                            .map_err(LuaError::external)
                    }
                },
            )?,
        )?;
    }

    // mcp.list_resources(name) → { ok=bool, resources=[...], error=str }
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "list_resources",
            lua.create_async_function(move |lua, name: String| {
                let mgr = Arc::clone(&mgr);
                async move {
                    let result = mgr.read().await.list_resources(&name).await;
                    let tbl = lua.create_table()?;
                    match result {
                        Ok(val) => {
                            tbl.set("ok", true)?;
                            tbl.set("resources", json_to_lua(&lua, val)?)?;
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

    // mcp.read_resource(name, uri) → { ok=bool, contents=[...], error=str }
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "read_resource",
            lua.create_async_function(move |lua, (name, uri): (String, String)| {
                let mgr = Arc::clone(&mgr);
                async move {
                    let result = mgr.read().await.read_resource(&name, &uri).await;
                    let tbl = lua.create_table()?;
                    match result {
                        Ok(val) => {
                            tbl.set("ok", true)?;
                            // ReadResourceResult has a `contents` array
                            let contents = val
                                .get("contents")
                                .cloned()
                                .unwrap_or(serde_json::Value::Array(vec![]));
                            tbl.set("contents", json_to_lua(&lua, contents)?)?;
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

    // mcp.list_prompts(name) → { ok=bool, prompts=[...], error=str }
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "list_prompts",
            lua.create_async_function(move |lua, name: String| {
                let mgr = Arc::clone(&mgr);
                async move {
                    let result = mgr.read().await.list_prompts(&name).await;
                    let tbl = lua.create_table()?;
                    match result {
                        Ok(val) => {
                            tbl.set("ok", true)?;
                            tbl.set("prompts", json_to_lua(&lua, val)?)?;
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

    // mcp.get_prompt(name, prompt_name, args) → { ok=bool, messages=[...], description=str, error=str }
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "get_prompt",
            lua.create_async_function(
                move |lua, (name, prompt_name, args): (String, String, Option<LuaValue>)| {
                    let mgr = Arc::clone(&mgr);
                    async move {
                        let args_json = match args {
                            Some(v) => lua_to_json(&lua, v)?,
                            None => serde_json::Value::Null,
                        };
                        let result = mgr
                            .read()
                            .await
                            .get_prompt(&name, &prompt_name, args_json)
                            .await;
                        let tbl = lua.create_table()?;
                        match result {
                            Ok(val) => {
                                tbl.set("ok", true)?;
                                let messages = val
                                    .get("messages")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Array(vec![]));
                                tbl.set("messages", json_to_lua(&lua, messages)?)?;
                                if let Some(desc) = val.get("description").and_then(|v| v.as_str())
                                {
                                    tbl.set("description", desc)?;
                                }
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

    // mcp.on_progress(server_name, fn)
    // Registers a Lua callback for progress notifications from `server_name`.
    // The callback signature: function(server_name, progress_token, progress, total, message)
    // `message` is an optional human-readable description (empty string when absent).
    // `fn` must be a pure Lua function (C functions are not supported).
    {
        let mgr = Arc::clone(manager);
        let isle = Arc::clone(&handler_isle);
        mcp_tbl.set(
            "on_progress",
            lua.create_async_function(
                move |_, (server_name, func): (String, LuaFunction)| {
                    let mgr = Arc::clone(&mgr);
                    let isle = Arc::clone(&isle);
                    async move {
                        if func.info().what != "Lua" {
                            return Err(LuaError::external(
                                "mcp.on_progress: handler must be a pure Lua function \
                                 (C functions and Rust-bound callbacks are not supported)",
                            ));
                        }
                        let bytecode = func.dump(true);
                        if bytecode.is_empty() {
                            return Err(LuaError::external(
                                "mcp.on_progress: Function::dump returned empty bytecode",
                            ));
                        }

                        // Forward bytecode to handler Isle: register in __mcp_progress_handlers
                        let server_for_exec = server_name.clone();
                        let bytecode_name = format!("@mcp_progress[{server_name}]");
                        isle.exec(move |lua| {
                            use mlua::prelude::*;
                            let loaded: LuaFunction = lua
                                .load(bytecode.as_slice())
                                .set_mode(mlua::ChunkMode::Binary)
                                .set_name(&bytecode_name)
                                .into_function()
                                .map_err(|e| IsleError::Lua(format!("on_progress load: {e}")))?;
                            let tbl: LuaTable = lua
                                .globals()
                                .get("__mcp_progress_handlers")
                                .map_err(|e| {
                                    IsleError::Lua(format!("on_progress get table: {e}"))
                                })?;
                            tbl.set(server_for_exec.as_str(), loaded)
                                .map_err(|e| IsleError::Lua(format!("on_progress set: {e}")))?;
                            Ok(String::new())
                        })
                        .await
                        .map_err(|e| {
                            tracing::error!(server = %server_name, error = %e, "mcp.on_progress: handler isle load failed");
                            LuaError::external(format!(
                                "mcp.on_progress: handler isle load failed: {e}"
                            ))
                        })?;

                        // Mark the registry so AgentBlockClientHandler::on_progress
                        // knows to dispatch notifications for this server.
                        mgr.read().await.handler.mark_on_progress(&server_name);

                        Ok(())
                    }
                },
            )?,
        )?;
    }

    // mcp.on_log(server_name, fn)
    // Registers a Lua callback for logging notifications from `server_name`.
    // The callback signature: function(server_name, level, logger, data_json)
    // `fn` must be a pure Lua function (C functions are not supported).
    {
        let mgr = Arc::clone(manager);
        let isle = Arc::clone(&handler_isle);
        mcp_tbl.set(
            "on_log",
            lua.create_async_function(
                move |_, (server_name, func): (String, LuaFunction)| {
                    let mgr = Arc::clone(&mgr);
                    let isle = Arc::clone(&isle);
                    async move {
                        if func.info().what != "Lua" {
                            return Err(LuaError::external(
                                "mcp.on_log: handler must be a pure Lua function \
                                 (C functions and Rust-bound callbacks are not supported)",
                            ));
                        }
                        let bytecode = func.dump(true);
                        if bytecode.is_empty() {
                            return Err(LuaError::external(
                                "mcp.on_log: Function::dump returned empty bytecode",
                            ));
                        }

                        let server_for_exec = server_name.clone();
                        let bytecode_name = format!("@mcp_log[{server_name}]");
                        isle.exec(move |lua| {
                            use mlua::prelude::*;
                            let loaded: LuaFunction = lua
                                .load(bytecode.as_slice())
                                .set_mode(mlua::ChunkMode::Binary)
                                .set_name(&bytecode_name)
                                .into_function()
                                .map_err(|e| IsleError::Lua(format!("on_log load: {e}")))?;
                            let tbl: LuaTable = lua
                                .globals()
                                .get("__mcp_log_handlers")
                                .map_err(|e| {
                                    IsleError::Lua(format!("on_log get table: {e}"))
                                })?;
                            tbl.set(server_for_exec.as_str(), loaded)
                                .map_err(|e| IsleError::Lua(format!("on_log set: {e}")))?;
                            Ok(String::new())
                        })
                        .await
                        .map_err(|e| {
                            tracing::error!(server = %server_name, error = %e, "mcp.on_log: handler isle load failed");
                            LuaError::external(format!(
                                "mcp.on_log: handler isle load failed: {e}"
                            ))
                        })?;

                        mgr.read().await.handler.mark_on_log(&server_name);

                        Ok(())
                    }
                },
            )?,
        )?;
    }

    // mcp.cancel(server_name, request_id)
    // Send a notifications/cancelled to the named server.
    // request_id is a number. Pass 0 if you do not have a specific ID.
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "cancel",
            lua.create_async_function(move |_, (server_name, request_id): (String, i64)| {
                let mgr = Arc::clone(&mgr);
                async move {
                    mgr.read().await.send_cancelled(&server_name, request_id);
                    Ok(())
                }
            })?,
        )?;
    }

    // mcp.set_sampling_handler(server_name, fn)
    // Register a Lua callback for sampling/createMessage requests from `server_name`.
    // The callback signature: function(server_name, params_json) -> table
    //   where the returned table has fields: model, stop_reason, role, content
    // `fn` must be a pure Lua function.
    {
        let mgr = Arc::clone(manager);
        let isle = Arc::clone(&handler_isle);
        mcp_tbl.set(
            "set_sampling_handler",
            lua.create_async_function(
                move |_, (server_name, func): (String, LuaFunction)| {
                    let mgr = Arc::clone(&mgr);
                    let isle = Arc::clone(&isle);
                    async move {
                        if func.info().what != "Lua" {
                            return Err(LuaError::external(
                                "mcp.set_sampling_handler: handler must be a pure Lua function \
                                 (C functions and Rust-bound callbacks are not supported)",
                            ));
                        }
                        let bytecode = func.dump(true);
                        if bytecode.is_empty() {
                            return Err(LuaError::external(
                                "mcp.set_sampling_handler: Function::dump returned empty bytecode",
                            ));
                        }

                        let server_for_exec = server_name.clone();
                        let bytecode_name = format!("@mcp_sampling[{server_name}]");
                        isle.exec(move |lua| {
                            use mlua::prelude::*;
                            let loaded: LuaFunction = lua
                                .load(bytecode.as_slice())
                                .set_mode(mlua::ChunkMode::Binary)
                                .set_name(&bytecode_name)
                                .into_function()
                                .map_err(|e| {
                                    IsleError::Lua(format!("set_sampling_handler load: {e}"))
                                })?;
                            let tbl: LuaTable = lua
                                .globals()
                                .get("__mcp_sampling_handlers")
                                .map_err(|e| {
                                    IsleError::Lua(format!("set_sampling_handler get table: {e}"))
                                })?;
                            tbl.set(server_for_exec.as_str(), loaded).map_err(|e| {
                                IsleError::Lua(format!("set_sampling_handler set: {e}"))
                            })?;
                            Ok(String::new())
                        })
                        .await
                        .map_err(|e| {
                            tracing::error!(server = %server_name, error = %e, "mcp.set_sampling_handler: handler isle load failed");
                            LuaError::external(format!(
                                "mcp.set_sampling_handler: handler isle load failed: {e}"
                            ))
                        })?;

                        mgr.read().await.handler.mark_sampling(&server_name);

                        Ok(())
                    }
                },
            )?,
        )?;
    }

    // mcp.server_info(name)
    // Return the server's InitializeResult as a Lua table.
    // Shape: { ok=true, server_info={...} } | { ok=false, error=... }
    {
        let mgr = Arc::clone(manager);
        mcp_tbl.set(
            "server_info",
            lua.create_async_function(move |lua, name: String| {
                let mgr = Arc::clone(&mgr);
                async move {
                    let result = mgr.read().await.server_info(&name);
                    let tbl = lua.create_table()?;
                    match result {
                        Ok(val) => {
                            tbl.set("ok", true)?;
                            tbl.set("server_info", json_to_lua(&lua, val)?)?;
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
