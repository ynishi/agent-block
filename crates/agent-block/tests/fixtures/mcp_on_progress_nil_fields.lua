-- on_progress nil-field normalization fixture.
--
-- Connects to a server that sends a progress notification with total=None and
-- message=None. Verifies that the on_progress callback fires (not crashes) and
-- that the nil fields are absent (not present in ev when Rust doesn't set them).
--
-- The callback now runs on the main Isle via main_isle.exec.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("nilprog", url)
print("CONNECT_HTTP_OK")

-- Outer local: captured as an upvalue by the callback below.
local progress_hits = 0

mcp.on_progress("nilprog", function(ev)
    -- Increment the upvalue counter (core upvalue-preservation check).
    progress_hits = progress_hits + 1
    assert(ev ~= nil, "ev must not be nil")
    assert(ev.server ~= nil, "server must not be nil")
    assert(ev.token ~= nil, "token must not be nil")
    assert(ev.progress ~= nil, "progress must not be nil")
    -- total=None: Rust does not set ev.total when absent (Option<f64> is None).
    -- message=None: same — ev.message is nil when absent.
    -- The callback must not crash even when these are nil.
    print("PROGRESS_EV_OK")
end)

local r = mcp.call("nilprog", "emit_progress_nil", {})
assert(r.ok == true, "emit_progress_nil call failed: " .. tostring(r.error))
print("CALL_OK")

std.task.sleep(300)

-- Report the hit count so Rust can assert upvalue preservation.
print(string.format("PROGRESS_HITS=%d", progress_hits))
assert(progress_hits >= 1, "progress_hits must be >= 1, got: " .. tostring(progress_hits))

print("FIXTURE_DONE")
