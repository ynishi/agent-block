-- agent_run_test.lua — mlua-lspec tests that drive agent.run itself.
--
-- Run via:
--   just test-lua agent_run_test   # this file
--   just test-lua                  # every spec fixture
--
-- agent_helpers_test covers the pure helpers; nothing drove `run`, which meant
-- the contract on its result was checked as data and at no call site. These
-- tests go through `M.run`, so the dev-mode assert on `M.shapes.run_result`
-- fires on every case below — including the ones that are supposed to fail,
-- since a failure is a shape too.
--
-- `tool_loop` is replaced through package.loaded: what is under test is the
-- result agent assembles, not the turns underneath it. The registry, the
-- runtime globals and the LLM call are all stubbed for the same reason.

local describe, it, expect = lust.describe, lust.it, lust.expect

if not log then
    log = { warn = function() end, info = function() end, debug = function() end, error = function() end }
end
if not tool then
    -- build_tools reads the registry through tool.schema(); an empty registry
    -- is the case where the model is handed no tools at all.
    tool = {
        register = function() end,
        schema = function()
            return {}
        end,
    }
end
if not std then
    std = {
        env = {
            get = function(_name)
                return nil
            end,
            get_or = function(_name, default)
                return default
            end,
            agent_id = function()
                return nil
            end,
        },
        json = {
            encode = function(v)
                return tostring(v)
            end,
        },
    }
end

-- What the stubbed loop hands back; each test sets it before calling run.
local loop_result = nil

package.loaded["tool_loop"] = {
    run = function(_opts)
        return loop_result
    end,
}

local lshape = require("lshape")
local check = lshape.check
local agent = require("agent")

local function ok_loop_result(content)
    return {
        ok = true,
        content = content,
        turns = 2,
        tool_calls = {},
        usage = { input_tokens = 1, output_tokens = 2, total_tokens = 3 },
        messages = {},
    }
end

describe("agent.run result contract", function()
    it("checks the contract at the call site, not only as data", function()
        -- If the wrapper were not applying it, this fixture would prove nothing.
        expect(check.is_dev_mode()).to.equal(true)
    end)

    it("refuses a missing prompt as a failure, not an exception", function()
        local res = agent.run({})
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("prompt is required")
        expect(res.num_turns).to.equal(0)
        expect(check.check(res, agent.shapes.run_result)).to.equal(true)
    end)

    it("treats an empty prompt the same way", function()
        local res = agent.run({ prompt = "" })
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("prompt is required")
    end)

    it("returns content on the success path", function()
        loop_result = ok_loop_result("the answer")
        local res = agent.run({ prompt = "ask" })
        expect(res.ok).to.equal(true)
        expect(res.content).to.equal("the answer")
        expect(res.num_turns).to.equal(2)
        expect(check.check(res, agent.shapes.run_result)).to.equal(true)
    end)

    it("substitutes empty content rather than returning none", function()
        -- The success half of the contract requires `content`; the loop is
        -- allowed to finish without producing any.
        loop_result = ok_loop_result(nil)
        local res = agent.run({ prompt = "ask" })
        expect(res.ok).to.equal(true)
        expect(res.content).to.equal("")
    end)

    it("propagates a loop failure as ok=false with the error", function()
        loop_result = {
            ok = false,
            error = "API error 500 (server)",
            turns = 1,
            tool_calls = {},
            usage = { input_tokens = 1, output_tokens = 0, total_tokens = 1 },
            messages = {},
        }
        local res = agent.run({ prompt = "ask" })
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("API error 500 (server)")
        expect(res.content).to.equal(nil)
        expect(check.check(res, agent.shapes.run_result)).to.equal(true)
    end)

    it("names the refusal category when the provider supplies one", function()
        loop_result = {
            ok = false,
            error = "model refused to respond",
            content = "",
            turns = 1,
            tool_calls = {},
            usage = { input_tokens = 1, output_tokens = 0, total_tokens = 1 },
            messages = {},
            stop_reason = "refusal",
            stop_details = { category = "safety" },
        }
        local res = agent.run({ prompt = "ask" })
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("model refused to respond (category=safety)")
    end)

    -- Without this the fixture would only show that valid results are valid,
    -- and removing the wrapper would break nothing here. agent passes the
    -- loop's `messages` straight through, so a loop that returns the wrong type
    -- for it is the drift the contract is placed to stop.
    it("raises when the assembled result does not match, rather than passing it on", function()
        loop_result = ok_loop_result("done")
        loop_result.messages = "not a message array"

        local ok, err = pcall(agent.run, { prompt = "ask" })
        expect(ok).to.equal(false)
        expect(tostring(err):find("shape violation", 1, true) ~= nil).to.be.truthy()
        expect(tostring(err):find("agent.run result", 1, true) ~= nil).to.be.truthy()
    end)

    it("carries the loop's messages back out", function()
        local convo = { { role = "user", content = "ask" } }
        loop_result = ok_loop_result("done")
        loop_result.messages = convo
        local res = agent.run({ prompt = "ask" })
        expect(#res.messages).to.equal(1)
        expect(res.messages[1].role).to.equal("user")
    end)
end)
