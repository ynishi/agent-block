-- on_log callback envelope dispatch fixture.
--
-- Reads MCP_HTTP_URL from the environment, connects via HTTP transport,
-- registers an on_log callback using mcp.on_log, then calls a tool that
-- triggers a log notification from the server.  Verifies the envelope shape
-- and prints markers for the Rust-side assertions.
--
-- Note: the on_log callback runs on the handler isle (a separate Lua VM).
-- State cannot be shared between vms via Lua variables.  The callback prints
-- LOG_EV_OK directly to stdout; the main script sleeps briefly to ensure
-- the callback fires before FIXTURE_DONE.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("logserver", url)
print("CONNECT_HTTP_OK")

-- Register on_log.  The callback runs on the handler isle; it prints its own marker.
mcp.on_log("logserver", function(server, level, logger, data_json)
    -- Verify envelope fields are present.
    assert(server ~= nil, "envelope server must not be nil")
    assert(level ~= nil, "envelope level must not be nil")
    -- Regression guard: Rust normalises logger (None→"") before push.
    assert(logger ~= nil, "envelope logger must not be nil (regression guard)")
    -- Regression guard: Rust serialises data to JSON so data_json is never nil.
    assert(data_json ~= nil, "envelope data_json must not be nil (regression guard)")
    -- Print the success marker so the Rust test assertion can see it.
    print("LOG_EV_OK")
end)

-- Call the tool that triggers a log notification from the server.
local r = mcp.call("logserver", "emit_log", {})
assert(r.ok == true, "emit_log call failed: " .. tostring(r.error))
print("CALL_OK")

-- Yield to the async runtime so the handler isle has time to fire the callback
-- and produce LOG_EV_OK on stdout before FIXTURE_DONE is printed.
std.task.sleep(300)

print("FIXTURE_DONE")
