-- mcp_resource_subscribe.lua
--
-- Demonstrates all 6 MCP Resource Subscribe APIs:
--   mcp.subscribe_resource(server, uri)
--   mcp.unsubscribe_resource(server, uri)
--   mcp.on_resource_update(server, callback)
--   mcp.on_resources_list_changed(server, callback)
--   mcp.on_tools_list_changed(server, callback)
--   mcp.on_prompts_list_changed(server, callback)
--
-- Run against a real MCP server that supports resource subscriptions.
-- Example (outline-mcp):
--   agent-block -s examples/mcp_resource_subscribe.lua

local server = "outline"

-- Connect to the MCP server (replace command/args as needed).
mcp.connect(server, "outline-mcp", {})

-- Register resource-update callback.
-- Fires when a subscribed resource is updated server-side.
-- ev = { type = "resource_update", server = <srv>, uri = <uri> }
mcp.on_resource_update(server, function(ev)
    log.info("resource_update: server=" .. ev.server .. " uri=" .. ev.uri)
end)

-- Register resources-list-changed callback.
-- Fires when the server's resource list changes (add/remove resources).
-- ev = { type = "resources_list_changed", server = <srv> }
mcp.on_resources_list_changed(server, function(ev)
    log.info("resources_list_changed: server=" .. ev.server)
end)

-- Register tools-list-changed callback.
-- ev = { type = "tools_list_changed", server = <srv> }
mcp.on_tools_list_changed(server, function(ev)
    log.info("tools_list_changed: server=" .. ev.server)
end)

-- Register prompts-list-changed callback.
-- ev = { type = "prompts_list_changed", server = <srv> }
mcp.on_prompts_list_changed(server, function(ev)
    log.info("prompts_list_changed: server=" .. ev.server)
end)

-- Subscribe to a specific resource URI.
local sub = mcp.subscribe_resource(server, "resource:///example")
if sub.ok then
    log.info("subscribed to resource:///example")
else
    log.warn("subscribe failed: " .. (sub.error or "unknown error"))
end

-- (Do useful work here — callbacks will fire when the server pushes notifications.)

-- Unsubscribe when done.
local unsub = mcp.unsubscribe_resource(server, "resource:///example")
if unsub.ok then
    log.info("unsubscribed from resource:///example")
else
    log.warn("unsubscribe failed: " .. (unsub.error or "unknown error"))
end

mcp.disconnect(server)
