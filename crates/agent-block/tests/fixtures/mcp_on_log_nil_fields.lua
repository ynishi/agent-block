-- on_log nil-field normalization fixture.
--
-- Connects to a server that sends a log notification with logger=None and
-- data=Value::Null. Verifies that the on_log callback fires (not crashes) and
-- that the fields are normalised to safe defaults (logger="", data="null").
--
-- The callback now runs on the main Isle via main_isle.exec.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("nillog", url)
print("CONNECT_HTTP_OK")

-- Outer local: captured as an upvalue by the callback below.
local log_hits = 0

mcp.on_log("nillog", function(ev)
    -- Increment the upvalue counter (core upvalue-preservation check).
    log_hits = log_hits + 1
    assert(ev ~= nil, "ev must not be nil")
    assert(ev.server ~= nil, "server must not be nil")
    assert(ev.level ~= nil, "level must not be nil")
    -- logger=None is normalised to "" by Rust before building ev.
    assert(ev.logger ~= nil, "logger must not be nil after normalisation")
    assert(type(ev.logger) == "string", "logger must be a string, got: " .. type(ev.logger))
    -- data=Null serialises to the string "null" by Rust.
    assert(ev.data ~= nil, "data must not be nil after normalisation")
    print("LOG_EV_OK")
end)

local r = mcp.call("nillog", "emit_log_nil", {})
assert(r.ok == true, "emit_log_nil call failed: " .. tostring(r.error))
print("CALL_OK")

std.task.sleep(300)

-- Report the hit count so Rust can assert upvalue preservation.
print(string.format("LOG_HITS=%d", log_hits))
assert(log_hits >= 1, "log_hits must be >= 1, got: " .. tostring(log_hits))

print("FIXTURE_DONE")
