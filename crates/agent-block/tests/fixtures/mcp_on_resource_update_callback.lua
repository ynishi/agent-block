-- mcp_on_resource_update_callback.lua
--
-- Reads MCP_HTTP_URL from the environment, connects via HTTP transport,
-- registers an on_resource_update callback, subscribes to a resource URI,
-- and waits for the server-side notify_resource_updated to fire the callback.
-- Prints sentinel markers for Rust-side assertion.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("subsrv", url)
print("CONNECT_HTTP_OK")

local update_hits = 0
local received_uri = ""

-- Register on_resource_update callback.
-- ev = { type = "resource_update", server = <srv>, uri = <uri> }
mcp.on_resource_update("subsrv", function(ev)
    update_hits = update_hits + 1
    received_uri = ev.uri or ""
    assert(ev.type == "resource_update", "ev.type must be resource_update, got: " .. tostring(ev.type))
    assert(ev.server == "subsrv", "ev.server must be subsrv, got: " .. tostring(ev.server))
    assert(ev.uri ~= nil and ev.uri ~= "", "ev.uri must be non-empty")
    print("RESOURCE_UPDATE_EV_OK")
end)

-- Subscribe to a resource URI.
-- The server will immediately notify_resource_updated with the same URI.
local sub = mcp.subscribe_resource("subsrv", "resource:///test-e2e")
assert(sub.ok == true, "subscribe_resource failed: " .. tostring(sub.error))
print("SUBSCRIBE_OK")

-- Yield to the async runtime so the notification can arrive and the
-- main Isle can process the exec before FIXTURE_DONE is printed.
std.task.sleep(300)

print(string.format("UPDATE_HITS=%d", update_hits))
assert(update_hits >= 1, "update_hits must be >= 1, got: " .. tostring(update_hits))
assert(received_uri == "resource:///test-e2e",
    "received_uri must be resource:///test-e2e, got: " .. tostring(received_uri))

print("FIXTURE_DONE")
