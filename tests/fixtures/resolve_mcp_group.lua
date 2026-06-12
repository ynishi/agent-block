-- resolve_mcp_group.lua — verify M._resolve_mcp_group priority logic
-- Priority: _meta.group (string, non-empty) > server_name fallback

local agent = require("agent")

-- Case 1: _meta.group is a valid non-empty string -> use it
local t1 = { name = "foo", _meta = { group = "mygroup" } }
local g1 = agent._resolve_mcp_group(t1, "myserver")
print("case1._meta.group_wins=" .. tostring(g1 == "mygroup"))

-- Case 2: no _meta field -> fallback to server_name
local t2 = { name = "bar" }
local g2 = agent._resolve_mcp_group(t2, "myserver")
print("case2.no_meta_fallback=" .. tostring(g2 == "myserver"))

-- Case 3: _meta present but group is empty string -> fallback to server_name
local t3 = { name = "baz", _meta = { group = "" } }
local g3 = agent._resolve_mcp_group(t3, "myserver")
print("case3.empty_group_fallback=" .. tostring(g3 == "myserver"))

-- Case 4: _meta.group is a number -> fallback to server_name
local t4 = { name = "qux", _meta = { group = 42 } }
local g4 = agent._resolve_mcp_group(t4, "myserver")
print("case4.number_group_fallback=" .. tostring(g4 == "myserver"))

-- Case 5: _meta.group is a table -> fallback to server_name
local t5 = { name = "quux", _meta = { group = { "a", "b" } } }
local g5 = agent._resolve_mcp_group(t5, "myserver")
print("case5.table_group_fallback=" .. tostring(g5 == "myserver"))

-- Case 6: _meta is present but has no group key -> fallback to server_name
local t6 = { name = "corge", _meta = { other = "value" } }
local g6 = agent._resolve_mcp_group(t6, "myserver")
print("case6.no_group_key_fallback=" .. tostring(g6 == "myserver"))

-- Case 7: _meta.group is a valid string; verify it also wires into the def
-- that build_tools eventually returns (integration sanity).
-- Simulate a mcp_tool_map entry as connect_mcp_servers would produce it.
local mcp_tool_map_meta = {
    meta__tool = {
        server = "meta",
        tool = "tool",
        def = {
            name = "meta__tool",
            description = "tool with meta group",
            input_schema = { type = "object", properties = {} },
            group = agent._resolve_mcp_group({ name = "tool", _meta = { group = "custom" } }, "meta"),
        },
    },
}
local built = agent._build_tools(mcp_tool_map_meta, nil, { "custom" })
local found_custom = false
for _, t in ipairs(built) do
    if t.name == "meta__tool" then found_custom = true end
end
print("case7.meta_group_used_for_filtering=" .. tostring(found_custom))
