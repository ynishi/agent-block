-- agent_run_test.lua — mlua-lspec tests that drive agent.run itself.
--
-- Run via:
--   just test-lua agent_run_test   # this file
--   just test-lua                  # every spec fixture
--
-- agent_helpers_test covers the pure helpers; these go through `M.run`, so the
-- dev-mode assert on `M.shapes.run_result` fires on every case below —
-- including the ones that are supposed to fail, since a failure is a shape too.
--
-- What is stood in for is the kernel, not a loop module: `agent.run` writes its
-- own loop now, so the seam is `knl.session` / `knl.beat` / `knl.views.usage`.
-- The bracket hands the body a fake session that records what is appended, and
-- `knl.fold` stays REAL — the thread the result carries is folded out of the
-- events the run laid down, which is the part worth checking rather than
-- stubbing. The registry, the runtime globals and the beats are stubbed for the
-- ordinary reason: none of them is what these cases are about.

local describe, it, expect = lust.describe, lust.it, lust.expect

if not log then
    log = { warn = function() end, info = function() end, debug = function() end, error = function() end }
end
if not tool then
    -- The tool set is read off the registry through tool.schema(); an empty
    -- registry is the case where the model is handed no tools at all.
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

local lshape = require("lshape")
local check = lshape.check
local kernel = require("knl")
local Outcome = kernel.Outcome
local agent = require("agent")

local real_fold = kernel.fold

-- The beats this run will answer with, in order, and the reading the usage
-- view reports. Each test sets them before calling run.
local beats = {}
local usage_rows = { { input_tokens = 1, output_tokens = 2, thinking_tokens = 0 } }
-- What the bracket was opened with, so a case can read the grant.
local opened_with = nil

--- A session that records what is appended and hands it back verbatim: enough
--- for `seed` to write into and for the real fold to read out of.
local function new_session()
    local events = {}
    return {
        append = function(_self, ev)
            events[#events + 1] = ev
            return #events
        end,
        events = function(_self)
            return events
        end,
    }
end

kernel.session = function(opts, fn)
    opened_with = opts
    return fn(new_session())
end

kernel.beat = function(_s, _d)
    local out = table.remove(beats, 1)
    if out == nil then
        error("the spec ran out of beats: the loop asked for one more than it was given")
    end
    return out
end

kernel.views.usage = function(_s)
    return usage_rows
end

--- An `ok` beat: the answer, and whether it asked for a tool.
local function answered(text, opts)
    opts = opts or {}
    local content = {}
    if text ~= nil then
        content[#content + 1] = { type = "text", text = text }
    end
    if opts.tool then
        content[#content + 1] = { type = "tool_use", id = "t1", name = opts.tool, input = {} }
    end
    return Outcome.ok({
        content = content,
        usage = { input_tokens = 1, output_tokens = 2, thinking_tokens = 0 },
        stop_reason = opts.stop_reason or (opts.tool and "tool_use" or "end_turn"),
        tools = {},
        beat = "b-1",
    })
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

    it("refuses a history that is not a messages array", function()
        local res = agent.run({ prompt = "ask", history = "nope" })
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("history must be a table (messages array)")
    end)

    it("returns content on the success path", function()
        beats = { answered("thinking", { tool = "x" }), answered("the answer") }
        local res = agent.run({ prompt = "ask" })
        expect(res.ok).to.equal(true)
        expect(res.content).to.equal("the answer")
        expect(res.num_turns).to.equal(2)
        expect(res.usage.total_tokens).to.equal(3)
        expect(check.check(res, agent.shapes.run_result)).to.equal(true)
    end)

    it("substitutes empty content rather than returning none", function()
        -- The success half of the contract requires `content`; a beat is
        -- allowed to settle without producing any text.
        beats = { answered(nil) }
        local res = agent.run({ prompt = "ask" })
        expect(res.ok).to.equal(true)
        expect(res.content).to.equal("")
    end)

    it("keeps beating while the server paused its own tool loop", function()
        -- No tool was asked for, but the turn is unfinished.
        beats = { answered("half", { stop_reason = "pause_turn" }), answered("done") }
        local res = agent.run({ prompt = "ask" })
        expect(res.ok).to.equal(true)
        expect(res.content).to.equal("done")
        expect(res.num_turns).to.equal(2)
    end)

    it("propagates a failed beat as ok=false, naming the stage and the class", function()
        beats = {
            Outcome.err("call", {
                kind = "server",
                retryable = true,
                message = "API error 500",
            }),
        }
        local res = agent.run({ prompt = "ask" })
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("call: server: API error 500")
        expect(res.content).to.equal(nil)
        expect(check.check(res, agent.shapes.run_result)).to.equal(true)
    end)

    it("names the refusal class, and what the provider said with it", function()
        beats = {
            Outcome.refused("model", {
                content = {},
                usage = { input_tokens = 1, output_tokens = 0, thinking_tokens = 0 },
                status = "refused",
                refusal = { kind = "model", detail = "I cannot help with that" },
            }),
        }
        local res = agent.run({ prompt = "ask" })
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("model refused to respond (kind=model): I cannot help with that")
    end)

    it("reports the exhausted grant as the iteration cap it is", function()
        beats = { answered("more", { tool = "x" }), Outcome.stopped("budget", "beats") }
        local res = agent.run({ prompt = "ask", max_iterations = 1 })
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("max_iterations (1) reached")
        -- The stopped beat never reached the provider, so it is not a turn.
        expect(res.num_turns).to.equal(1)
    end)

    it("grants one unit per iteration, so the cap and the budget are one bound", function()
        beats = { answered("done") }
        agent.run({ prompt = "ask", max_iterations = 7 })
        expect(opened_with.budget.amount).to.equal(7)
        expect(opened_with.budget.tag).to.equal("beats")
    end)

    it("stops on the token budget, reading the spend off the log", function()
        beats = { answered("more", { tool = "x" }), answered("still more", { tool = "x" }) }
        local res = agent.run({ prompt = "ask", max_tokens_budget = 3 })
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("token budget exceeded (3/3)")
        expect(res.num_turns).to.equal(1)
    end)

    it("stops when on_turn says so, and reports what was answered", function()
        beats = { answered("first", { tool = "x" }) }
        local seen = {}
        local res = agent.run({
            prompt = "ask",
            on_turn = function(info)
                seen[#seen + 1] = info
                return false
            end,
        })
        expect(res.ok).to.equal(true)
        expect(res.content).to.equal("first")
        expect(#seen).to.equal(1)
        expect(seen[1].turn_number).to.equal(1)
        expect(#seen[1].tool_calls).to.equal(1)
        expect(seen[1].tool_calls[1].name).to.equal("x")
    end)

    -- Without this the fixture would only show that valid results are valid,
    -- and removing the wrapper would break nothing here. The thread comes
    -- straight out of the fold, so a fold that answers the wrong type for it is
    -- the drift the contract is placed to stop.
    it("raises when the assembled result does not match, rather than passing it on", function()
        beats = { answered("done") }
        kernel.fold = function()
            return { messages = "not a message array" }
        end
        local ok, err = pcall(agent.run, { prompt = "ask" })
        kernel.fold = real_fold
        expect(ok).to.equal(false)
        expect(tostring(err):find("shape violation", 1, true) ~= nil).to.be.truthy()
        expect(tostring(err):find("agent.run result", 1, true) ~= nil).to.be.truthy()
    end)

    it("carries the thread back out, folded from what the run laid down", function()
        beats = { answered("done") }
        local res = agent.run({ prompt = "ask" })
        expect(#res.messages).to.equal(1)
        expect(res.messages[1].role).to.equal("user")
        expect(res.messages[1].content).to.equal("ask")
    end)

    -- What `blocks/lib/session` round-trips. A tool_result block seeded inside
    -- the user message that carried it would leave its tool_use unanswered, and
    -- the fold's repair would then close it a second time — so the results are
    -- laid down as the events they are, and the thread comes back unchanged.
    it("replays a prior thread without duplicating its tool results", function()
        beats = { answered("done") }
        local res = agent.run({
            prompt = "and now?",
            history = {
                { role = "user", content = "hello" },
                {
                    role = "assistant",
                    content = { { type = "tool_use", id = "t1", name = "x", input = {} } },
                },
                {
                    role = "user",
                    content = { { type = "tool_result", tool_use_id = "t1", content = "42" } },
                },
            },
        })
        expect(res.ok).to.equal(true)
        expect(#res.messages).to.equal(4)
        expect(res.messages[2].role).to.equal("assistant")
        expect(res.messages[3].role).to.equal("user")
        expect(#res.messages[3].content).to.equal(1)
        expect(res.messages[3].content[1].tool_use_id).to.equal("t1")
        expect(res.messages[3].content[1].content).to.equal("42")
        expect(res.messages[4].content).to.equal("and now?")
    end)
end)
