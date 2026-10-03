-- build_tools_extra.lua — verify that the extra_tools nested-schema+handler
-- form is flattened, that an already-flat entry passes through, that a flat
-- entry carrying its own handler is bound to that handler, and that neither a
-- handler nor the internal `group` reaches the wire declaration. A handler on
-- the wire once made the request fail to encode as JSON for the flat form.
--
-- `agent._build_tools` now answers knl's tools MAP (name -> entry) rather than
-- an array of API payloads, so what a provider is actually shown is one fold
-- away: `knl.fold` strips the handler and the declaration has three fields, so
-- `group` has nowhere to leak to by construction.

local agent = require("agent")
local kernel = require("knl")

-- nested-schema+handler form: what a tool factory returns
local nested_tool = {
    name = "nested_x",
    schema = { description = "nested desc", input_schema = { type = "object", properties = {} } },
    handler = function()
        return ""
    end,
    group = "mygroup",
}

-- already-flat form: plain tool definition, no handler (dispatches through the
-- Lua registry, as it always did)
local flat_tool = {
    name = "flat_y",
    description = "flat desc",
    input_schema = { type = "object", properties = {} },
}

-- flat form with its own handler: the shape a caller writes by hand
local flat_handled = {
    name = "flat_h",
    description = "flat with handler",
    input_schema = { type = "object", properties = {} },
    handler = function()
        return "banana"
    end,
}

local tools = agent._build_tools(agent._extra_candidates({ nested_tool, flat_tool, flat_handled }), nil)

-- The wire declarations: what the request carries, keyed by name.
local decls = {}
for _, d in ipairs(kernel.fold({}, kernel.device({ tools = tools })).tools) do
    decls[d.name] = d
end

local nested = decls["nested_x"] or {}
local flat = decls["flat_y"] or {}
local handled = decls["flat_h"] or {}

-- nested_x: must be flattened to {name, description, input_schema}, no handler
print("nested.name=" .. tostring(nested.name))
print("nested.description=" .. tostring(nested.description))
print("nested.handler=" .. tostring(nested.handler))
print("nested.schema=" .. tostring(nested.schema))

-- flat_y: must pass through unchanged
print("flat.name=" .. tostring(flat.name))
print("flat.description=" .. tostring(flat.description))

-- group must not be on the emitted defs (a provider rejects extra fields)
print("nested.group=" .. tostring(nested.group))
print("flat.group=" .. tostring(flat.group))

-- flat_h: declared without its handler, the declarations encode, and a call
-- reaches the caller's handler
print("handled.name=" .. tostring(handled.name))
print("handled.handler=" .. tostring(handled.handler))
local encoded_ok = pcall(std.json.encode, kernel.fold({}, kernel.device({ tools = tools })).tools)
print("decls.encode=" .. tostring(encoded_ok))
print("handled.call=" .. tostring(tools["flat_h"].handler({})))
