-- on_progress envelope dispatch fixture.
--
-- Reads MCP_HTTP_URL from the environment, connects via HTTP transport,
-- registers an on_progress callback using mcp.on_progress, then calls a
-- tool that sends a progress notification.  Verifies the envelope shape
-- (including the message field) and prints markers for the Rust-side
-- assertions.
--
-- The on_progress callback now runs on the main Isle (same Lua VM as this
-- script) via main_isle.exec, so upvalues are preserved.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("prog", url)
print("CONNECT_HTTP_OK")

-- Outer local: captured as an upvalue by the callback below.
-- This verifies that the main Isle exec path preserves upvalues correctly.
local progress_hits = 0

-- Register on_progress.  Callback receives a single ev table.
mcp.on_progress("prog", function(ev)
    -- Increment the upvalue counter (core upvalue-preservation check).
    progress_hits = progress_hits + 1
    -- Verify envelope fields are present.
    assert(ev ~= nil, "ev must not be nil")
    assert(ev.server ~= nil, "envelope server must not be nil")
    assert(ev.token ~= nil, "envelope token must not be nil")
    assert(ev.progress ~= nil, "envelope progress must not be nil")
    -- Print the success marker so the Rust test assertion can see it.
    print("PROGRESS_EV_OK")
end)

-- Call the tool that triggers a progress notification from the server.
-- The server sends progress then returns the tool result.
local r = mcp.call("prog", "emit_progress", {})
assert(r.ok == true, "emit_progress call failed: " .. tostring(r.error))
print("CALL_OK")

-- Yield to the async runtime so the main Isle has time to process the exec
-- and produce PROGRESS_EV_OK on stdout before FIXTURE_DONE is printed.
std.task.sleep(300)

-- Report the hit count so Rust can assert upvalue preservation.
print(string.format("PROGRESS_HITS=%d", progress_hits))
assert(progress_hits >= 1, "progress_hits must be >= 1, got: " .. tostring(progress_hits))

print("FIXTURE_DONE")
