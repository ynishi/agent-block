-- HTTP MCP cancel API fixture.
-- Reads MCP_HTTP_URL from the environment, connects via HTTP transport,
-- then calls mcp.cancel and verifies it does not error.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("counter", url)
print("CONNECT_HTTP_OK")

-- mcp.cancel is fire-and-forget: it must not throw even with request_id=0.
mcp.cancel("counter", 0)
print("CANCEL_OK")

-- Disconnect cleanly.
mcp.disconnect("counter")
print("FIXTURE_DONE")
