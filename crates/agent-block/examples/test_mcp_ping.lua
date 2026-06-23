-- test_mcp_ping.lua
--
-- Smoke test for mcp.ping(name).
-- Sends a keepalive ping to an MCP server and prints round-trip latency.
--
-- Run against a real MCP server (e.g. algocline):
--   agent-block -s examples/test_mcp_ping.lua

local server = "algocline"

-- Connect to the MCP server.
mcp.connect(server, "algocline-mcp", {})

-- Send a ping and measure round-trip latency.
local result = mcp.ping(server)

if result.ok then
    log.info("ping succeeded, latency_ms=" .. result.latency_ms)
else
    log.warn("ping failed: " .. (result.error or "unknown error"))
end

mcp.disconnect(server)
