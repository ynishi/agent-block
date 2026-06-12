-- build_tools_extra.lua — verify that build_tools flattens nested-schema+handler form
-- and passes through already-flat entries unchanged.

local agent = require("agent")

-- nested-schema+handler form: what compile_loop.make() returns
local nested_tool = {
	name = "nested_x",
	schema = { description = "nested desc", input_schema = { type = "object", properties = {} } },
	handler = function()
		return ""
	end,
	group = "mygroup",
}

-- already-flat form: plain Anthropic tool definition
local flat_tool = {
	name = "flat_y",
	description = "flat desc",
	input_schema = { type = "object", properties = {} },
}

local tools = agent._build_tools({}, { nested_tool, flat_tool })

-- nested_x: must be flattened to {name, description, input_schema}, no handler
print("nested.name=" .. tostring(tools[1].name))
print("nested.description=" .. tostring(tools[1].description))
print("nested.handler=" .. tostring(tools[1].handler))
print("nested.schema=" .. tostring(tools[1].schema))

-- flat_y: must pass through unchanged
print("flat.name=" .. tostring(tools[2].name))
print("flat.description=" .. tostring(tools[2].description))

-- group must be stripped from all emitted defs (Anthropic API rejects extra fields)
print("nested.group=" .. tostring(tools[1].group))
print("flat.group=" .. tostring(tools[2].group))
