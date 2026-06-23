-- dispatch_extra_tools.lua — verify that compile_loop.make() registers a tool
-- in the registry and tool.call() can invoke its handler (regression test for
-- a past "tool not found" bug).

local compile_loop = require("compile_loop")

local _ = compile_loop.make({
	name = "compile_loop",
	runner = function()
		return { ok = true, stdout = "PASS", stderr = "", exit_code = 0 }
	end,
})

-- Confirm registry entry exists by calling tool.call.
local ok, res = pcall(tool.call, "compile_loop", {
	spec = "no-op spec",
	target_file = "/tmp/dispatch_test_target.lua",
})
if not ok then
	print("dispatch=err: " .. tostring(res))
	return
end

-- handler returns a JSON string. Dispatch is confirmed if we received any
-- JSON string back (handler was invoked). ok=false with failure_reason=llm_call
-- is expected in test environments where no API key is configured.
if type(res) == "string" and res:find('"ok":', 1, true) then
	print("dispatch=ok")
else
	print("dispatch=unexpected: " .. tostring(res))
end
