-- agent_test.tl — `htl test` for the `agent` block: the contracts it
-- publishes as data. The Lua specs beside it drive `agent.run` itself against
-- a stood-in kernel and hold the helpers.
--
-- Run via `just test-tl` (walks `blocks/lib` for `*_test.tl`; one state per
-- file).
local t = require("htl.test")
local agent = require("agent")

t.describe("shapes", function()
    t.it("publishes its three contracts", function()
        t.expect(agent.shapes.usage):to_not_be_nil()
        t.expect(agent.shapes.run_result):to_not_be_nil()
        t.expect(agent.shapes.mcp_call_result):to_not_be_nil()
    end)
end)
