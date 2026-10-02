-- beat_cap_spec.lua — mlua-lspec unit tests for `policy.beat_cap`, the wrap
-- that holds one beat's tool results together under the room's beat_budget.
--
-- Run via:
--   just test-lua beat_cap_spec
--
-- What this proves:
--   1 the bounds: a room is required and must be one, an unknown option is
--     refused, the binder insists on a session, and a map without handlers
--     is refused;
--   2 over a real beat: results pass while the beat's results so far plus
--     this one stay under beat_budget, and the one that would cross it is
--     answered `{ ok = false, reason = "beat_budget", tokens, used, limit }`
--     while the handler's own answer is kept out of the record — the kernel
--     records what the wrapper returned;
--   3 the count is per beat: a new beat starts from zero, and results of an
--     earlier beat are not held against this one;
--   4 the first result of a beat is never refused by this cap alone, because
--     the room holds beat_share >= result_share.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- A refusal is a table, and the kernel's fold and this cap's own count render
-- a table result through `std.json.encode`; `support` installs the shared
-- encoder (knl/spec/json_stub.lua), whose length grows with the value.
local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")

--- A Port whose count is the length of every string content.
local function port_of(window)
    return {
        profile = function()
            return { context_window = window, max_output = 0 }
        end,
        count = function(_, request)
            local n = 0
            for _, message in ipairs(request.messages or {}) do
                if type(message.content) == "string" then
                    n = n + #message.content
                end
            end
            return n
        end,
    }
end

--- A tool answering `body` under `name`, as the device takes it.
local function tool(name, body)
    return support.tool(name, body)[name]
end

--- A device whose llm asks for `calls` in one response, each `{ name, id }`.
local function device_calling(calls, tools)
    local blocks = {}
    for _, c in ipairs(calls) do
        blocks[#blocks + 1] = { type = "tool_use", id = c.id, name = c.name, input = {} }
    end
    return kernel.device({
        llm = support.always(support.answer(blocks, "tool_use")),
        tools = tools,
    })
end

--- The tool_result events of `session`, in order, as `{ call_id, result }`.
local function results_of(session)
    local out = {}
    for _, ev in ipairs(session:events()) do
        if ev.kind == "tool_result" then
            out[#out + 1] = { call_id = ev.data.call_id, result = ev.data.result }
        end
    end
    return out
end

describe("policy.beat_cap — the bounds", function()
    it("requires a room, refuses an unknown option, and binds to a session only", function()
        expect(function()
            policy.beat_cap({})
        end).to.fail()
        local ok, err = pcall(policy.beat_cap, { room = { beat_budget = 1 } })
        expect(ok).to.be(false)
        expect(tostring(err):find("room must be a value from policy.room", 1, true)).to.exist()
        local room = policy.room({ port = port_of(100) })
        local ok2, err2 = pcall(policy.beat_cap, { room = room, share = 0.5 })
        expect(ok2).to.be(false)
        expect(tostring(err2):find("unknown option 'share'", 1, true)).to.exist()
        local bind = policy.beat_cap({ room = room })
        local ok3, err3 = pcall(bind, { events = function() end })
        expect(ok3).to.be(false)
        expect(tostring(err3):find("session must be a knl session", 1, true)).to.exist()
        local ok4, err4 = pcall(bind(support.session()), { t = { description = "no handler" } })
        expect(ok4).to.be(false)
        expect(tostring(err4):find("has no handler", 1, true)).to.exist()
    end)
end)

describe("policy.beat_cap — over a beat", function()
    -- limit 100, result_share 0.3 -> one result may take 30; beat_share 0.5 ->
    -- a beat's results may take 50 together.
    local function room()
        return policy.room({ port = port_of(100), result_share = 0.3, beat_share = 0.5 })
    end

    it("passes results while the beat stays under its budget, and refuses the one that would cross it", function()
        local s = support.seed(support.session(), "q")
        local tools = policy.beat_cap({ room = room() })(s)({
            a = tool("a", string.rep("a", 20)),
            b = tool("b", string.rep("b", 20)),
            c = tool("c", string.rep("c", 20)),
        })
        local device =
            device_calling({ { name = "a", id = "1" }, { name = "b", id = "2" }, { name = "c", id = "3" } }, tools)
        local out = kernel.beat(s, device)
        expect(out.status).to.be("ok")
        local results = results_of(s)
        expect(#results).to.be(3)
        expect(results[1].result).to.be(string.rep("a", 20))
        expect(results[2].result).to.be(string.rep("b", 20))
        -- 20 + 20 recorded, and 20 more would make 60 of 50.
        local third = results[3].result
        expect(third.ok).to.be(false)
        expect(third.reason).to.be("beat_budget")
        expect(third.tokens).to.be(20)
        expect(third.used).to.be(40)
        expect(third.limit).to.be(50)
        expect(third.error:find("'c' answered 20 tokens", 1, true)).to.exist()
    end)

    it("counts per beat: the next beat starts from zero", function()
        local s = support.seed(support.session(), "q")
        local tools = policy.beat_cap({ room = room() })(s)({
            a = tool("a", string.rep("a", 30)),
            b = tool("b", string.rep("b", 30)),
        })
        local both = device_calling({ { name = "a", id = "1" }, { name = "b", id = "2" } }, tools)
        kernel.beat(s, both)
        local first = results_of(s)
        expect(first[1].result).to.be(string.rep("a", 30))
        expect(first[2].result.reason).to.be("beat_budget")
        -- The same two calls on the next beat: the earlier beat's 30 is not
        -- held against this one.
        kernel.beat(s, both)
        local second = results_of(s)
        expect(#second).to.be(4)
        expect(second[3].result).to.be(string.rep("a", 30))
        expect(second[4].result.reason).to.be("beat_budget")
    end)

    it("never refuses the first result of a beat on its own, and composes outside result_cap", function()
        -- A wider room, because a refusal is a table and its rendering costs
        -- the beat too: limit 1000 -> one result 300, a beat 500.
        local r = policy.room({ port = port_of(1000), result_share = 0.3, beat_share = 0.5 })
        local s = support.seed(support.session(), "q")
        -- 301 is over one result's limit; 200 is under it and, with the
        -- refusal's rendering before it, under the beat's 500.
        local tools = policy.beat_cap({ room = r })(s)(policy.result_cap({ room = r })({
            a = tool("a", string.rep("a", 200)),
            big = tool("big", string.rep("x", 301)),
        }))
        local device = device_calling({ { name = "big", id = "1" }, { name = "a", id = "2" } }, tools)
        kernel.beat(s, device)
        local results = results_of(s)
        -- The oversized one is result_cap's refusal (a small table), which
        -- costs the beat only what that refusal renders to.
        expect(results[1].result.reason).to.be("result_too_large")
        expect(results[2].result).to.be(string.rep("a", 200))
    end)
end)
