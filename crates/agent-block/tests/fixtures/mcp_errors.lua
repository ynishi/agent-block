-- MCP error-path fixture. Exits 0; emits markers to stdout for the
-- Rust-side e2e assertions. No external MCP server required —
-- `sleep` stands in for a hung child process.

-- Case 1: `connect` must surface a BlockError::Timeout when the
-- child never completes the initialize handshake.
local ok, err = pcall(mcp.connect, "stuck", "sleep", { "60" })
if ok then
    error("expected connect to fail on sleep child")
end
print("CONNECT_TIMEOUT_ERR=" .. tostring(err))

-- Case 2: call_tool on an unknown server must return { ok=false, error=... }.
local r = mcp.call("ghost", "shelf", {})
assert(r.ok == false, "expected ok=false")
print("UNKNOWN_CALL_ERR=" .. tostring(r.error))

-- The bridge assembles mcp.call's table field by field in Rust, and the Lua
-- side publishes a shape for it. Checked here, on the failure branch, against
-- what the bridge actually produced rather than against a stub of it.
local shape_check = require("lshape").check
local mcp_call_result = require("agent").shapes.mcp_call_result
local shape_ok, why = shape_check.check(r, mcp_call_result)
assert(shape_ok, "mcp.call failure result violated its shape: " .. tostring(why))
print("CALL_ERR_SHAPE_OK")

-- Case 3: list_tools on an unknown server must return { ok=false, error=... }.
local lt = mcp.list_tools("ghost")
assert(lt.ok == false, "expected list ok=false")
print("UNKNOWN_LIST_ERR=" .. tostring(lt.error))

print("FIXTURE_DONE")
