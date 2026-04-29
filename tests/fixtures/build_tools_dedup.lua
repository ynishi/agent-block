-- build_tools_dedup.lua — verify that compile_loop.make() registers via tool.register
-- by default; extra_tools then dedups against tool.schema() first-wins, so no
-- duplicate tool names appear in _build_tools output.

local agent = require("agent")
local compile_loop = require("compile_loop")

-- Build a tool_def via compile_loop.make (default: registers via tool.register).
-- compile_loop.make() registers via `tool.register` by default; extra_tools then
-- dedups against `tool.schema()` first-wins, so dedup_test_tool will be present
-- exactly once in the final tools list.
local td = compile_loop.make({
	name = "dedup_test_tool",
	runner = function()
		return { ok = true, stdout = "", stderr = "" }
	end,
})

-- Simulate the extra_tools path: td is passed once via extra_tools.
-- tool.schema() already contains dedup_test_tool (registered above),
-- so _build_tools step 1 picks it up and step 3 dedup drops the extra_tools copy.
local tools = agent._build_tools({}, { td })

assert(tools ~= nil and #tools >= 1, "tools must not be empty")

local seen = {}
for _, t in ipairs(tools) do
	assert(seen[t.name] == nil, "duplicate tool: " .. t.name)
	seen[t.name] = true
end

assert(seen["dedup_test_tool"] == true, "dedup_test_tool must appear in tools")

print("dedup=ok")
print("tool_count=" .. tostring(#tools))
