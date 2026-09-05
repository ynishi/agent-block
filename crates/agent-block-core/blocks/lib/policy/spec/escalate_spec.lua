-- escalate_spec.lua — mlua-lspec unit tests for `policy.escalate`, the policy
-- that answers the device the next beat should use.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/escalate_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- What this proves:
--   1 ESCALATING HERE IS CHANGING THE TOOL, NOT ASKING A SUPERVISOR. What
--     comes back is a device — `d:with{ llm = strong }` — and the next beat
--     runs in the same session, against the same log, with nothing delegated
--     and nobody notified;
--   2 when `when` is false the device that came in is handed back, the SAME
--     value, so a loop can assign the result unconditionally;
--   3 the original device is untouched, because a device is frozen and `with`
--     derives rather than mutates;
--   4 the default judgement: a refusal or a failure that asking again would
--     not fix escalates; a retryable failure does not (that is `retry`'s
--     question), and neither does a beat that came off;
--   5 `strong` is required and must be callable, loud at construction.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")
local Outcome = kernel.Outcome

local function weak_llm()
    return support.always(support.text("weak"))
end

local function strong_llm()
    return support.always(support.text("strong"))
end

--- The two error readings the default judgement turns on.
local function retryable_error()
    return Outcome.err("state", { kind = "busy", method = "reserve", retryable = true, message = "locked" })
end

local function permanent_error()
    return Outcome.err("state", { kind = "storage", method = "append", retryable = false, message = "gone" })
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.escalate — construction", function()
    it("answers `next`, a function over an outcome and a device", function()
        expect(type(policy.escalate({ strong = strong_llm() }))).to.be("function")
    end)

    it("insists on something to escalate to", function()
        expect(function()
            policy.escalate({})
        end).to.fail()
        expect(function()
            policy.escalate({ strong = "claude" })
        end).to.fail()
        expect(function()
            policy.escalate()
        end).to.fail()
    end)

    it("takes a callable table as an llm, like the device does", function()
        local port = setmetatable({}, {
            __call = function(_self, _request)
                return support.text("from a port")
            end,
        })
        expect(function()
            policy.escalate({ strong = port })
        end).to_not.fail()
    end)

    it("refuses an option it does not know, and a `when` that is not a function", function()
        expect(function()
            policy.escalate({ strong = strong_llm(), whne = function() end })
        end).to.fail()
        expect(function()
            policy.escalate({ strong = strong_llm(), when = "always" })
        end).to.fail()
    end)
end)

describe("policy.escalate — the device it answers", function()
    it("hands the same device back when `when` is false", function()
        local device = kernel.device({ llm = weak_llm() })
        local next_device = policy.escalate({
            strong = strong_llm(),
            when = function()
                return false
            end,
        })
        expect(next_device(Outcome.ok({ beat = "b1" }), device)).to.be(device)
        expect(next_device(permanent_error(), device)).to.be(device)
    end)

    it("derives one carrying the strong llm when `when` is true", function()
        local strong = strong_llm()
        local device = kernel.device({ llm = weak_llm(), system = "be terse" })
        local next_device = policy.escalate({
            strong = strong,
            when = function()
                return true
            end,
        })
        local escalated = next_device(Outcome.refused("model", {}), device)

        expect(escalated == device).to.be(false)
        expect(escalated.llm).to.be(strong)
        -- the rest of the policy rides along, and the original is intact
        expect(escalated.system).to.be("be terse")
        expect(device.llm == strong).to.be(false)
    end)

    it("answers a device a beat can actually run", function()
        local session = support.session()
        support.seed(session, "q")
        local device = kernel.device({ llm = weak_llm() })
        local escalate = policy.escalate({ strong = strong_llm() })

        local first = kernel.beat(session, device)
        expect(Outcome.is_ok(first)).to.be(true)
        expect(first.out.content[1].text).to.be("weak")

        -- The loop assigns unconditionally; nothing failed, so nothing changed.
        device = escalate(first, device)
        local second = kernel.beat(session, device)
        expect(second.out.content[1].text).to.be("weak")

        -- After a refusal it changes, and the next beat is the strong one's —
        -- in the same session, against the same log.
        device = escalate(Outcome.refused("model", {}), device)
        local third = kernel.beat(session, device)
        expect(third.out.content[1].text).to.be("strong")
        expect(#support.beat_ids(session)).to.be(3)
    end)
end)

describe("policy.escalate — the default judgement", function()
    local device = kernel.device({ llm = weak_llm() })
    local escalate = policy.escalate({ strong = strong_llm() })

    local function escalates(outcome)
        return escalate(outcome, device) ~= device
    end

    it("escalates on a refusal: asking the same model again is not an answer", function()
        expect(escalates(Outcome.refused("model", { beat = "b1" }))).to.be(true)
        expect(escalates(Outcome.refused("content_filter", { beat = "b1" }))).to.be(true)
    end)

    it("escalates on a failure that asking again would not fix", function()
        expect(escalates(permanent_error())).to.be(true)
        expect(escalates(Outcome.err("call", "the provider said no"))).to.be(true)
    end)

    it("does not escalate on a retryable failure — that is retry's question", function()
        -- A busy store is not a model that could not manage the task, and a
        -- stronger llm has no bearing on it.
        expect(escalates(retryable_error())).to.be(false)
    end)

    it("does not escalate on a beat that came off, or on a planned stop", function()
        expect(escalates(Outcome.ok({ beat = "b1" }))).to.be(false)
        expect(escalates(Outcome.stopped("budget", "beats"))).to.be(false)
    end)
end)
