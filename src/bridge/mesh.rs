//! mesh.* — Agent mesh communication bridge (async).
//!
//! `mesh.send` and `mesh.request` use `create_async_function` so that
//! Lua coroutines yield while waiting for mesh I/O.

use mlua::prelude::*;
use serde_json::Map;
use std::sync::Arc;
use std::time::Duration;

use crate::host::HostContext;

/// Register the `mesh.*` Lua table.
///
/// `is_handler_side` selects the surface exposed by this bridge:
///
/// - `false` (main Isle): registers `send`, `request`, `on`, `agent_id`.
///   `mesh.on` is a thin alias over `bus.on("mesh", fn)` and therefore
///   depends on `bus.register` having run first on the same VM
///   (`bridge::register_all` orders them correctly).
/// - `true` (handler Isle): registers `send`, `request`, `agent_id` only.
///   The handler Isle does not expose the `bus.*` global, so installing
///   `mesh.on` would fail with `bus global missing`. Handlers dispatched on
///   the handler Isle can still call `mesh.send` / `mesh.request` because
///   the `MeshAgent` Arc is shared across Isles via `HostContext`.
pub fn register(lua: &Lua, ctx: &HostContext, is_handler_side: bool) -> LuaResult<()> {
    let mesh_tbl = lua.create_table()?;
    let script_name: String = lua
        .globals()
        .get::<Option<String>>("_SCRIPT_NAME")?
        .unwrap_or_else(|| "unknown".to_string());

    match &ctx.mesh_agent {
        None => {
            // All functions return error when mesh is not connected.
            // Skip `on` on the handler side because it is not exposed there.
            let names: &[&str] = if is_handler_side {
                &["send", "request", "agent_id"]
            } else {
                &["send", "request", "on", "agent_id"]
            };
            for name in names {
                let n = name.to_string();
                mesh_tbl.set(
                    *name,
                    lua.create_function(move |_, _: LuaValue| {
                        Err::<LuaValue, _>(LuaError::external(format!(
                            "mesh.{n}: mesh not connected (no --relay specified)"
                        )))
                    })?,
                )?;
            }
        }
        Some(agent) => {
            let agent_send = Arc::clone(agent);
            let script_name_send = script_name.clone();
            mesh_tbl.set(
                "send",
                lua.create_async_function(
                    move |lua, (agent_id_str, payload): (String, LuaValue)| {
                        let agent = Arc::clone(&agent_send);
                        let script_name = script_name_send.clone();
                        async move {
                            use crate::bridge::lua_to_json;
                            let mut payload_json = lua_to_json(&lua, payload)?;
                            inject_obs_context(&mut payload_json, Some(agent.agent_id().to_string()));
                            tracing::info!(
                                target: "lua",
                                script = %script_name,
                                "{}",
                                obs_line(
                                    "mesh_send",
                                    &obs_context(Some(agent.agent_id().to_string())),
                                    &[("target", agent_id_str.as_str())],
                                )
                            );
                            let target = agent_mesh_core::identity::AgentId::from_raw(agent_id_str);
                            agent
                                .request(&target, payload_json, Duration::from_secs(10))
                                .await
                                .map_err(LuaError::external)?;
                            Ok(())
                        }
                    },
                )?,
            )?;

            let agent_req = Arc::clone(agent);
            let script_name_req = script_name.clone();
            mesh_tbl.set(
                "request",
                lua.create_async_function(
                    move |lua, (agent_id_str, payload): (String, LuaValue)| {
                        let agent = Arc::clone(&agent_req);
                        let script_name = script_name_req.clone();
                        async move {
                            use crate::bridge::{json_to_lua, lua_to_json};
                            let mut payload_json = lua_to_json(&lua, payload)?;
                            inject_obs_context(&mut payload_json, Some(agent.agent_id().to_string()));
                            tracing::info!(
                                target: "lua",
                                script = %script_name,
                                "{}",
                                obs_line(
                                    "mesh_request",
                                    &obs_context(Some(agent.agent_id().to_string())),
                                    &[("target", agent_id_str.as_str())],
                                )
                            );
                            let target = agent_mesh_core::identity::AgentId::from_raw(agent_id_str);
                            let resp = agent
                                .request(&target, payload_json, Duration::from_secs(30))
                                .await
                                .map_err(LuaError::external)?;
                            json_to_lua(&lua, resp)
                        }
                    },
                )?,
            )?;

            let agent_id_str = agent.agent_id().to_string();
            mesh_tbl.set(
                "agent_id",
                lua.create_function(move |_, ()| Ok(agent_id_str.clone()))?,
            )?;

            if !is_handler_side {
                // mesh.on is a thin alias over bus.on("mesh", fn). The
                // EventBus (registered in bridge/bus.rs before this function
                // runs on the main Isle) owns the actual dispatch. Capture
                // `bus.on` at registration time so subsequent reassignments
                // of the `bus` global do not hijack the alias.
                //
                // Because `bus.on` is now an async function (it forwards
                // handler bytecode to the handler Isle), `mesh.on` must be
                // an async function too and `.call_async().await` the
                // underlying `bus.on`.
                let bus_tbl: LuaTable = lua.globals().get("bus")?;
                let bus_on: LuaFunction = bus_tbl.get("on")?;
                mesh_tbl.set(
                    "on",
                    lua.create_async_function(move |_, func: LuaFunction| {
                        let bus_on = bus_on.clone();
                        async move { bus_on.call_async::<()>(("mesh", func)).await }
                    })?,
                )?;
            }
        }
    }

    lua.globals().set("mesh", mesh_tbl)?;
    Ok(())
}

fn inject_obs_context(payload_json: &mut serde_json::Value, fallback_agent_id: Option<String>) {
    let serde_json::Value::Object(obj) = payload_json else {
        return;
    };
    if obj.contains_key("__ab_obs") {
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
        .or(fallback_agent_id);
    if let Some(v) = agent_id {
        obs.insert("agent_id".to_string(), serde_json::Value::String(v));
    }
    if let Ok(v) = std::env::var("AGENT_BLOCK_AGENT_NAME") {
        if !v.is_empty() {
            obs.insert("agent_name".to_string(), serde_json::Value::String(v));
        }
    }
    if !obs.is_empty() {
        obj.insert("__ab_obs".to_string(), serde_json::Value::Object(obs));
    }
}

fn obs_context(fallback_agent_id: Option<String>) -> (String, String, String, String) {
    let trace_id = std::env::var("AGENT_BLOCK_TRACE_ID").unwrap_or_default();
    let run_id = std::env::var("AGENT_BLOCK_RUN_ID").unwrap_or_default();
    let agent_id = std::env::var("AGENT_BLOCK_AGENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .or(fallback_agent_id)
        .unwrap_or_default();
    let agent_name = std::env::var("AGENT_BLOCK_AGENT_NAME").unwrap_or_default();
    (trace_id, run_id, agent_id, agent_name)
}

fn obs_line(event: &str, ctx: &(String, String, String, String), extra: &[(&str, &str)]) -> String {
    let mut parts = vec![
        "prefix=ab.obs".to_string(),
        format!("event={}", event),
        "component=mesh".to_string(),
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
