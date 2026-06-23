-- on_log callback envelope dispatch fixture.
--
-- Reads MCP_HTTP_URL from the environment, connects via HTTP transport,
-- registers an on_log callback using mcp.on_log, then calls a tool that
-- triggers a log notification from the server.  Verifies the envelope shape
-- and prints markers for the Rust-side assertions.
--
-- The on_log callback now runs on the main Isle (same Lua VM as this script)
-- via main_isle.exec, so upvalues are preserved.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("logserver", url)
print("CONNECT_HTTP_OK")

-- Outer local: captured as an upvalue by the callback below.
local log_hits = 0

-- Register on_log.  Callback receives a single ev table.
mcp.on_log("logserver", function(ev)
    -- Increment the upvalue counter (core upvalue-preservation check).
    log_hits = log_hits + 1
    -- Verify envelope fields are present.
    assert(ev ~= nil, "ev must not be nil")
    assert(ev.server ~= nil, "envelope server must not be nil")
    assert(ev.level ~= nil, "envelope level must not be nil")
    -- Rust normalises logger (None→"") and data (JSON string).
    assert(ev.logger ~= nil, "envelope logger must not be nil (regression guard)")
    assert(ev.data ~= nil, "envelope data must not be nil (regression guard)")
    -- Print the success marker so the Rust test assertion can see it.
    print("LOG_EV_OK")
end)

-- Call the tool that triggers a log notification from the server.
local r = mcp.call("logserver", "emit_log", {})
assert(r.ok == true, "emit_log call failed: " .. tostring(r.error))
print("CALL_OK")

-- Yield to the async runtime so the main Isle has time to process the exec
-- and produce LOG_EV_OK on stdout before FIXTURE_DONE is printed.
std.task.sleep(300)

-- Report the hit count so Rust can assert upvalue preservation.
print(string.format("LOG_HITS=%d", log_hits))
assert(log_hits >= 1, "log_hits must be >= 1, got: " .. tostring(log_hits))

print("FIXTURE_DONE")
