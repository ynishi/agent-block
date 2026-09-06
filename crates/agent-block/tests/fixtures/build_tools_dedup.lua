-- build_tools_dedup.lua — a tool_def that is in the registry AND in
-- extra_tools names one tool twice.
--
-- That used to be a first-wins merge, which picked a winner silently. It is a
-- loud error now: two sources claiming one name is a wiring bug, and
-- `knl_adapter.tools` refuses rather than choosing. The case is the same one,
-- flipped — what is asserted is the refusal, and that the registry alone still
-- binds the tool exactly once.
--
-- The def is written out here in the nested `{name, schema, handler}` form
-- rather than built by a factory: registration is the caller's, and this
-- fixture is the caller.

local agent = require("agent")

local td = {
    name = "dedup_test_tool",
    schema = {
        description = "A tool that exists twice over.",
        input_schema = {
            type = "object",
            properties = { spec = { type = "string" } },
            required = { "spec" },
        },
    },
    handler = function()
        return "ok"
    end,
}
tool.register(td.name, td.schema, td.handler)

local registry = agent._registry_candidates()

local both = {}
for _, c in ipairs(registry) do
    both[#both + 1] = c
end
for _, c in ipairs(agent._extra_candidates({ td })) do
    both[#both + 1] = c
end

local ok, err = pcall(agent._build_tools, both, nil)
assert(not ok, "a name claimed by both the registry and extra_tools must be refused, not merged")
assert(
    tostring(err):find("duplicate tool name 'dedup_test_tool'", 1, true) ~= nil,
    "the refusal must name the tool, got: " .. tostring(err)
)

-- The registry on its own still binds it, exactly once.
local tools = agent._build_tools(registry, nil)
assert(tools["dedup_test_tool"] ~= nil, "dedup_test_tool must be bound from the registry")

local count = 0
for _ in pairs(tools) do
    count = count + 1
end

print("dedup=ok")
print("tool_count=" .. tostring(count))
