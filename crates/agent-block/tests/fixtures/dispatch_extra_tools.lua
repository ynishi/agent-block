-- dispatch_extra_tools.lua — verify that a registered tool_def is the one
-- tool.call() invokes (regression test for a past "tool not found" bug).
--
-- The def is written out here in the nested `{name, schema, handler}` form:
-- building a def and registering it are two steps, and the identity this pins
-- is between what the caller registered and what the registry dispatches.

local td = {
    name = "dispatch_probe",
    schema = {
        description = "Echo the spec back as JSON.",
        input_schema = {
            type = "object",
            properties = {
                spec = { type = "string" },
                target_file = { type = "string" },
            },
            required = { "spec" },
        },
    },
    handler = function(args)
        return std.json.encode({ ok = true, spec = args.spec })
    end,
}
tool.register(td.name, td.schema, td.handler)

-- Confirm registry entry exists by calling tool.call.
local ok, res = pcall(tool.call, "dispatch_probe", {
    spec = "no-op spec",
    target_file = "/tmp/dispatch_test_target.lua",
})
if not ok then
    print("dispatch=err: " .. tostring(res))
    return
end

-- The handler returns a JSON string. Dispatch is confirmed if we received one
-- back, which is only possible if the handler ran.
if type(res) == "string" and res:find('"ok":', 1, true) then
    print("dispatch=ok")
else
    print("dispatch=unexpected: " .. tostring(res))
end
