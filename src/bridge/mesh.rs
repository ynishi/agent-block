//! mesh.* — Agent mesh communication bridge (async).
//!
//! `mesh.send` and `mesh.request` use `create_async_function` so that
//! Lua coroutines yield while waiting for mesh I/O.

use mlua::prelude::*;
use serde_json::Map;
use std::sync::Arc;
use std::time::Duration;

use crate::bridge::obs;
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
                            let local_agent_id = agent.agent_id().to_string();
                            inject_obs_context(&mut payload_json, Some(local_agent_id.clone()));
                            tracing::info!(
                                target: "lua",
                                script = %script_name,
                                "{}",
                                obs::obs_line(
                                    "mesh",
                                    "mesh_send",
                                    &obs::obs_context(Some(local_agent_id.as_str())),
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
                            let local_agent_id = agent.agent_id().to_string();
                            inject_obs_context(&mut payload_json, Some(local_agent_id.clone()));
                            tracing::info!(
                                target: "lua",
                                script = %script_name,
                                "{}",
                                obs::obs_line(
                                    "mesh",
                                    "mesh_request",
                                    &obs::obs_context(Some(local_agent_id.as_str())),
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
    fn insert_obs(obj: &mut Map<String, serde_json::Value>, fallback_agent_id: Option<String>) {
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

    match payload_json {
        serde_json::Value::Object(obj) => insert_obs(obj, fallback_agent_id),
        serde_json::Value::Null => {
            let mut obj = Map::<String, serde_json::Value>::new();
            insert_obs(&mut obj, fallback_agent_id);
            if !obj.is_empty() {
                *payload_json = serde_json::Value::Object(obj);
            }
        }
        _ => {}
    }
}
