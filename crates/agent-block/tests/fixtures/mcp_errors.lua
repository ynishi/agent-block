-- MCP error-path fixture. Exits 0; emits markers to stdout for the
-- Rust-side e2e assertions. No external MCP server required —
-- `sleep` stands in for a hung child process.

-- Case 1: `connect` must surface a BlockError::Timeout when the
-- child never completes the initialize handshake.
local ok, err = pcall(mcp.connect, "stuck", "sleep", {"60"})
if ok then
    error("expected connect to fail on sleep child")
end
print("CONNECT_TIMEOUT_ERR=" .. tostring(err))

-- Case 2: call_tool on an unknown server must return { ok=false, error=... }.
local r = mcp.call("ghost", "shelf", {})
assert(r.ok == false, "expected ok=false")
print("UNKNOWN_CALL_ERR=" .. tostring(r.error))

-- Case 3: list_tools on an unknown server must return { ok=false, error=... }.
local lt = mcp.list_tools("ghost")
assert(lt.ok == false, "expected list ok=false")
print("UNKNOWN_LIST_ERR=" .. tostring(lt.error))

print("FIXTURE_DONE")
