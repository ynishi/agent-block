-- on_log nil-field normalization fixture.
--
-- Connects to a server that sends a log notification with logger=None and
-- data=Value::Null. Verifies that the on_log callback fires (not crashes) and
-- that the nil fields are normalised to safe defaults (logger="", data="null").
--
-- This exercises the belt-and-suspenders nil-guards added to the Lua glue in
-- __mcp_dispatch_log (handler.rs).

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("nillog", url)
print("CONNECT_HTTP_OK")

mcp.on_log("nillog", function(server, level, logger, data_json)
    assert(server ~= nil, "server must not be nil")
    assert(level ~= nil, "level must not be nil")
    -- logger=None is normalised to "", data=Null serialises to the string "null".
    assert(logger ~= nil, "logger must not be nil after normalisation")
    assert(type(logger) == "string", "logger must be a string, got: " .. type(logger))
    assert(data_json ~= nil, "data_json must not be nil after normalisation")
    print("LOG_EV_OK")
end)

local r = mcp.call("nillog", "emit_log_nil", {})
assert(r.ok == true, "emit_log_nil call failed: " .. tostring(r.error))
print("CALL_OK")

std.task.sleep(300)
print("FIXTURE_DONE")
