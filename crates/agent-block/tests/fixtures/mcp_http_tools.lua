-- HTTP MCP round-trip fixture.
-- Reads MCP_HTTP_URL from the environment, connects via HTTP transport,
-- then calls mcp.list_tools and prints markers for the Rust-side assertions.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

-- Connect via HTTP transport (Streamable HTTP / stateless mode)
mcp.connect_http("counter", url)
print("CONNECT_HTTP_OK")

-- list_tools should return ok=true with at least one tool
local lt = mcp.list_tools("counter")
assert(lt.ok == true, "list_tools ok=false: " .. tostring(lt.error))
assert(type(lt.tools) == "table", "tools must be a table")
assert(#lt.tools >= 1, "expected at least 1 tool, got " .. #lt.tools)

-- Verify the 'increment' tool is present
local found = false
for _, t in ipairs(lt.tools) do
    if t.name == "increment" then
        found = true
        break
    end
end
assert(found, "increment tool not found in list_tools response")

print("LIST_TOOLS_OK")

-- Call it for real, then check the result against the shape the Lua side
-- publishes for mcp.call. The bridge assembles that table field by field in
-- Rust; this is the success branch checked against a running server rather than
-- against a fixture's idea of one.
local call = mcp.call("counter", "increment", {})
assert(call.ok == true, "call_tool ok=false: " .. tostring(call.error))

local shape_check = require("lshape").check
local mcp_call_result = require("agent").shapes.mcp_call_result
local shape_ok, why = shape_check.check(call, mcp_call_result)
assert(shape_ok, "mcp.call result violated its shape: " .. tostring(why))
print("CALL_TOOL_SHAPE_OK")

print("FIXTURE_DONE")
