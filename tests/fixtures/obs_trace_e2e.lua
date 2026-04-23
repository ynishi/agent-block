-- obs_trace_e2e.lua
--
-- Exercises bridge-level ab.obs logging without external dependencies:
-- - http.request to 127.0.0.1:1 (expected connection error)
-- - mcp.call against a missing server (expected ok=false)

-- HTTP: trigger request/response-side logging path (request always logs before send).
local http_ok, http_err = pcall(function()
    return http.request("http://127.0.0.1:1", {
        method = "GET",
        timeout = 1,
    })
end)
if http_ok then
    -- Unexpected, but keep script green for log assertions.
    print("http_unexpected_ok")
else
    print("http_error_ok")
end

-- MCP: trigger mcp_call + mcp_result logs (ok=false for unknown server).
local mcp_res = mcp.call("missing", "noop", {})
if mcp_res.ok then
    print("mcp_unexpected_ok")
else
    print("mcp_error_ok")
end

-- Tool: trigger tool_register + tool_call + tool_result logs.
tool.register("obs_echo", {
    description = "echo tool for obs e2e",
    input_schema = {
        type = "object",
        properties = {
            message = { type = "string" },
        },
    },
}, function(input)
    return input.message
end)

local echoed = tool.call("obs_echo", { message = "ok" })
if echoed == "ok" then
    print("tool_ok")
else
    print("tool_unexpected")
end
