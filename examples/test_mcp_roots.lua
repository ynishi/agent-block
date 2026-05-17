-- test_mcp_roots.lua
--
-- Smoke test for mcp.set_roots_handler and mcp.notify_roots_list_changed.
-- Registers a roots handler that responds to server-originated list_roots
-- requests, then sends a roots/list_changed notification to the server.
--
-- Run against a real MCP server that supports roots (e.g. algocline):
--   agent-block -s examples/test_mcp_roots.lua

local server = "algocline"

-- Connect to the MCP server.
mcp.connect(server, "algocline-mcp", {})

-- Register a handler for server-originated list_roots requests.
-- The server may call this at any time to discover the client's roots.
mcp.set_roots_handler(server, function(server_name)
    log.info("list_roots requested by: " .. server_name)
    return {
        { uri = "file:///", name = "Root" },
    }
end)

-- Notify the server that the roots list has changed.
mcp.notify_roots_list_changed(server)
log.info("notify_roots_list_changed sent to: " .. server)

mcp.disconnect(server)
