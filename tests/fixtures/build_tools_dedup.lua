-- build_tools_dedup.lua — verify that compile_loop.make({register=false}) + extra_tools
-- does not produce duplicate tool names in _build_tools output.

local agent = require("agent")
local compile_loop = require("compile_loop")

-- Build a tool_def via compile_loop.make with register=false.
-- register=false means tool.register() is NOT called, so tool.schema() will not
-- contain this tool. Passing it via extra_tools is the only path into _build_tools.
local td = compile_loop.make({
	register = false,
	name = "dedup_test_tool",
	runner = function()
		return { ok = true, stdout = "", stderr = "" }
	end,
})

-- Simulate the extra_tools path: td is passed once via extra_tools.
-- With register=false, tool.schema() does NOT include dedup_test_tool,
-- so _build_tools should produce exactly one entry named dedup_test_tool.
local tools = agent._build_tools({}, { td })

assert(tools ~= nil and #tools >= 1, "tools must not be empty")

local seen = {}
for _, t in ipairs(tools) do
	assert(seen[t.name] == nil, "duplicate tool: " .. t.name)
	seen[t.name] = true
end

print("dedup=ok")
print("tool_count=" .. tostring(#tools))
