-- on_progress nil-field normalization fixture.
--
-- Connects to a server that sends a progress notification with total=None and
-- message=None. Verifies that the on_progress callback fires (not crashes) and
-- that the nil fields are normalised to safe defaults (total=0.0, message="").
--
-- This exercises the belt-and-suspenders nil-guards added to the Lua glue in
-- __mcp_dispatch_progress (handler.rs) that prevent nil-concat crashes if the
-- Rust-side normalisation were ever bypassed.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("nilprog", url)
print("CONNECT_HTTP_OK")

mcp.on_progress("nilprog", function(server, token, progress, total, message)
    assert(server ~= nil, "server must not be nil")
    assert(token ~= nil, "token must not be nil")
    assert(progress ~= nil, "progress must not be nil")
    -- total=None is normalised to 0 (number), message=None is normalised to "".
    assert(total ~= nil, "total must not be nil after normalisation")
    assert(type(total) == "number", "total must be a number, got: " .. type(total))
    assert(message ~= nil, "message must not be nil after normalisation")
    assert(type(message) == "string", "message must be a string, got: " .. type(message))
    print("PROGRESS_EV_OK")
end)

local r = mcp.call("nilprog", "emit_progress_nil", {})
assert(r.ok == true, "emit_progress_nil call failed: " .. tostring(r.error))
print("CALL_OK")

std.task.sleep(300)
print("FIXTURE_DONE")
