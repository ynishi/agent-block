//! mesh.* — Agent mesh communication bridge (async).
//!
//! `mesh.send` and `mesh.request` use `create_async_function` so that
//! Lua coroutines yield while waiting for mesh I/O.

use mlua::prelude::*;
use std::sync::Arc;
use std::time::Duration;

use crate::host::HostContext;

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let mesh_tbl = lua.create_table()?;

    match &ctx.mesh_agent {
        None => {
            // All functions return error when mesh is not connected
            for name in &["send", "request", "on", "agent_id"] {
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
            mesh_tbl.set(
                "send",
                lua.create_async_function(
                    move |lua, (agent_id_str, payload): (String, LuaValue)| {
                        let agent = Arc::clone(&agent_send);
                        async move {
                            use crate::bridge::lua_to_json;
                            let payload_json = lua_to_json(&lua, payload)?;
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
            mesh_tbl.set(
                "request",
                lua.create_async_function(
                    move |lua, (agent_id_str, payload): (String, LuaValue)| {
                        let agent = Arc::clone(&agent_req);
                        async move {
                            use crate::bridge::{json_to_lua, lua_to_json};
                            let payload_json = lua_to_json(&lua, payload)?;
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

            // mesh.on is now a thin alias over bus.on("mesh", fn). The
            // EventBus (registered in bridge/bus.rs before this function
            // runs) owns the actual dispatch. Capture `bus.on` at
            // registration time so subsequent reassignments of the `bus`
            // global do not hijack the alias.
            let bus_tbl: LuaTable = lua.globals().get("bus")?;
            let bus_on: LuaFunction = bus_tbl.get("on")?;
            mesh_tbl.set(
                "on",
                lua.create_function(move |_, func: LuaFunction| bus_on.call::<()>(("mesh", func)))?,
            )?;
        }
    }

    lua.globals().set("mesh", mesh_tbl)?;
    Ok(())
}
