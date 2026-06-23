-- build_tools_mcp_group.lua — verify that MCP tool defs with group=server_name
-- are filtered correctly by active_groups in build_tools.

local agent = require("agent")

-- Simulate two MCP servers: "outline" and "search"
local mcp_tool_map = {
    outline__docs = {
        server = "outline",
        tool = "docs",
        def = {
            name = "outline__docs",
            description = "outline docs",
            input_schema = { type = "object", properties = {} },
            group = "outline",
        },
    },
    search__query = {
        server = "search",
        tool = "query",
        def = {
            name = "search__query",
            description = "search query",
            input_schema = { type = "object", properties = {} },
            group = "search",
        },
    },
}

-- Case 1: active_groups = {"outline"} — only outline tool should appear
local tools_outline = agent._build_tools(mcp_tool_map, nil, { "outline" })
local found_outline = false
local found_search_in_outline_filter = false
for _, t in ipairs(tools_outline) do
    if t.name == "outline__docs" then found_outline = true end
    if t.name == "search__query" then found_search_in_outline_filter = true end
end
print("case1.outline_included=" .. tostring(found_outline))
print("case1.search_excluded=" .. tostring(not found_search_in_outline_filter))

-- Case 2: active_groups = {"search"} — only search tool should appear
local tools_search = agent._build_tools(mcp_tool_map, nil, { "search" })
local found_search = false
local found_outline_in_search_filter = false
for _, t in ipairs(tools_search) do
    if t.name == "search__query" then found_search = true end
    if t.name == "outline__docs" then found_outline_in_search_filter = true end
end
print("case2.search_included=" .. tostring(found_search))
print("case2.outline_excluded=" .. tostring(not found_outline_in_search_filter))

-- Case 3: active_groups = nil — all tools should appear (backwards compat)
local tools_all = agent._build_tools(mcp_tool_map, nil, nil)
local count = 0
for _ in pairs(mcp_tool_map) do count = count + 1 end
print("case3.all_tools_count=" .. tostring(#tools_all) .. "_expected=" .. tostring(count))

-- Case 4: active_groups = {"default"} — MCP tools (group=server_name) must NOT appear
--   (they are not in "default" group)
local tools_default = agent._build_tools(mcp_tool_map, nil, { "default" })
local mcp_leaked = false
for _, t in ipairs(tools_default) do
    if t.name == "outline__docs" or t.name == "search__query" then
        mcp_leaked = true
    end
end
print("case4.mcp_not_in_default=" .. tostring(not mcp_leaked))

-- Case 5: emitted tool defs must NOT contain the `group` field.
-- group is an internal filtering field; forwarding it to the Anthropic API
-- causes 400 "Extra inputs are not permitted".
local tools_all2 = agent._build_tools(mcp_tool_map, nil, nil)
local group_leaked = false
for _, t in ipairs(tools_all2) do
    if t.group ~= nil then
        group_leaked = true
    end
end
print("case5.group_not_in_emitted_def=" .. tostring(not group_leaked))
