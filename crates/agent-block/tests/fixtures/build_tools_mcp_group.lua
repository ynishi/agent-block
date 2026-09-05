-- build_tools_mcp_group.lua — verify that candidates carrying a group
-- (MCP tools take the server name unless `_meta.group` says otherwise) are
-- filtered by the active groups, and that the group never reaches the wire.
--
-- A CANDIDATE is `{ group?, bind }`: the value knl_adapter.tools binds, plus
-- the label the filter reads. MCP tools arrive as ToolPorts; a flat spec with
-- a handler stands in for one here, because the filter reads the candidate and
-- not the source.

local agent = require("agent")
local kernel = require("knl")

local function stub(name, group)
    return {
        group = group,
        bind = {
            name = name,
            description = name .. " (stub)",
            input_schema = { type = "object", properties = {} },
            handler = function()
                return ""
            end,
        },
    }
end

-- Simulate two MCP servers: "outline" and "search"
local candidates = { stub("outline__docs", "outline"), stub("search__query", "search") }

local function count(tools)
    local n = 0
    for _ in pairs(tools) do
        n = n + 1
    end
    return n
end

-- Case 1: active_groups = {"outline"} — only outline tool should appear
local tools_outline = agent._build_tools(candidates, { "outline" })
print("case1.outline_included=" .. tostring(tools_outline["outline__docs"] ~= nil))
print("case1.search_excluded=" .. tostring(tools_outline["search__query"] == nil))

-- Case 2: active_groups = {"search"} — only search tool should appear
local tools_search = agent._build_tools(candidates, { "search" })
print("case2.search_included=" .. tostring(tools_search["search__query"] ~= nil))
print("case2.outline_excluded=" .. tostring(tools_search["outline__docs"] == nil))

-- Case 3: active_groups = nil — all tools should appear (backwards compat)
local tools_all = agent._build_tools(candidates, nil)
print("case3.all_tools_count=" .. tostring(count(tools_all)) .. "_expected=" .. tostring(#candidates))

-- Case 4: active_groups = {"default"} — MCP tools (group=server_name) must NOT
--   appear (they are not in "default" group)
local tools_default = agent._build_tools(candidates, { "default" })
local mcp_leaked = tools_default["outline__docs"] ~= nil or tools_default["search__query"] ~= nil
print("case4.mcp_not_in_default=" .. tostring(not mcp_leaked))

-- Case 5: the wire declarations must NOT carry the `group` field. It is a
-- filtering label the candidate holds, and a declaration has three fields, so
-- there is nowhere for it to ride to the provider.
local group_leaked = false
for _, decl in ipairs(kernel.fold({}, kernel.device({ tools = tools_all })).tools) do
    if decl.group ~= nil then
        group_leaked = true
    end
end
print("case5.group_not_in_emitted_def=" .. tostring(not group_leaked))
