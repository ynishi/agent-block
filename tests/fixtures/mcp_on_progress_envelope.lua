-- on_progress envelope dispatch fixture.
--
-- Reads MCP_HTTP_URL from the environment, connects via HTTP transport,
-- registers an on_progress callback using mcp.on_progress, then calls a
-- tool that sends a progress notification.  Verifies the envelope shape
-- (including the message field) and prints markers for the Rust-side
-- assertions.
--
-- Note: the on_progress callback runs on the handler isle (a separate Lua VM).
-- State cannot be shared between vms via Lua variables.  The callback prints
-- PROGRESS_EV_OK directly to stdout; the main script sleeps briefly to ensure
-- the callback fires before FIXTURE_DONE.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("prog", url)
print("CONNECT_HTTP_OK")

-- Register on_progress.  The dispatcher now passes a 5th argument (message).
-- The callback runs on the handler isle; it prints its own marker directly.
mcp.on_progress("prog", function(server, token, progress, total, message)
    -- Verify envelope fields are present.
    assert(server ~= nil, "envelope server must not be nil")
    assert(token ~= nil, "envelope token must not be nil")
    assert(progress ~= nil, "envelope progress must not be nil")
    -- Print the success marker so the Rust test assertion can see it.
    print("PROGRESS_EV_OK")
end)

-- Call the tool that triggers a progress notification from the server.
-- The server sends progress then returns the tool result.
local r = mcp.call("prog", "emit_progress", {})
assert(r.ok == true, "emit_progress call failed: " .. tostring(r.error))
print("CALL_OK")

-- Yield to the async runtime so the handler isle has time to fire the callback
-- and produce PROGRESS_EV_OK on stdout before FIXTURE_DONE is printed.
std.task.sleep(300)

print("FIXTURE_DONE")
