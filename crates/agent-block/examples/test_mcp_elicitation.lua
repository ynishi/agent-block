-- test_mcp_elicitation.lua
--
-- Smoke test for mcp.set_elicitation_handler.
-- Registers an elicitation handler that responds to server-originated
-- elicitation/create requests (Form variant only).
--
-- Run against a real MCP server that supports elicitation:
--   agent-block -s examples/test_mcp_elicitation.lua

local server = "algocline"

-- Connect to the MCP server.
mcp.connect(server, "algocline-mcp", {})

-- Register a handler for server-originated elicitation/create (Form variant) requests.
-- The server may call this at any time to collect structured input from the client.
-- schema_json contains the ElicitationSchema serialized as a JSON string.
mcp.set_elicitation_handler(server, function(server_name, message, schema_json)
    log.info("elicitation requested by: " .. server_name .. " message: " .. message)
    -- Accept the request with a sample response.
    return {
        action = "accept",
        content = { name = "Alice" },
    }
end)

log.info("elicitation handler registered for: " .. server)

mcp.disconnect(server)
