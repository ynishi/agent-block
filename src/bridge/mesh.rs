//! mesh.* — Agent mesh communication bridge (async).
//!
//! `mesh.send` and `mesh.request` use `create_async_function` so that
//! Lua coroutines yield while waiting for mesh I/O.

use mlua::prelude::*;
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
