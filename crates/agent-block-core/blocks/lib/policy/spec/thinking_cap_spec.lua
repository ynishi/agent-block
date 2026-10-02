-- thinking_cap_spec.lua — mlua-lspec unit tests for `policy.thinking_cap`,
-- the filter that sends the reasoning's stop point with every request.
--
-- Run via:
--   just test-lua thinking_cap
--
-- What this proves:
--   1 the stop point is the reply's room less what the call needs: the
--     window less the request's count, less call_reserve;
--   2 a cap the wire carries (`profile.max_output`) bounds that room, and
--     the window bounds it when the cap is larger than what is left;
--   3 `budget` is a ceiling on it, and only when it is the smaller number;
--   4 a request that leaves no room is left exactly as it is — there is no
--     stop point worth sending, and 0 is not one;
--   5 the conf's own thinking keys survive: the filter adds a budget, it does
--     not replace the caller's reasoning settings, and `thinking = true`
--     carries as `enabled`;
--   6 the request handed in is not changed — what comes back is a copy;
--   7 the bounds are loud: no room, a room whose port cannot answer both
--     questions, a room with no conf or one that does not turn reasoning on
--     (nothing, or `false`, or `enabled = false`), a call_reserve that is not
--     whole tokens (refused by the room) or absent from the room, an unknown
--     option, and — where the room is built — a port whose profile names no
--     window.

local describe, it, expect = lust.describe, lust.it, lust.expect

local policy = require("policy")

--- A Port whose window is `window`, whose wire cap is `max_output` (nil for
--- none), and which counts `tokens` for any request. Every call is recorded,
--- so a case can say the filter asked rather than assumed.
local function port_of(window, tokens, max_output)
    local asked = { profile = 0, count = 0 }
    return {
        asked = asked,
        profile = function(self)
            self.asked.profile = self.asked.profile + 1
            return { context_window = window, max_output = max_output }
        end,
        count = function(self)
            self.asked.count = self.asked.count + 1
            return tokens
        end,
    }
end

--- A conf with reasoning on, which is the least a conf may say here.
local function conf_on()
    return { thinking = true }
end

--- The request a filter is handed: the shape `knl.fold` answers.
local function request_of()
    return { messages = { { role = "user", content = "do it" } }, system = "SYS" }
end

describe("policy.thinking_cap — construction", function()
    it("answers a filter", function()
        local room = policy.room({ port = port_of(1000, 100), conf = conf_on(), call_reserve = 100 })
        local filter = policy.thinking_cap({ room = room })
        expect(type(filter)).to.be("function")
    end)

    it("refuses no room, and a port that cannot answer both questions is refused by the room", function()
        expect(function()
            policy.room({ port = { count = function() end }, conf = conf_on(), call_reserve = 100 })
        end).to.fail()
        expect(function()
            policy.room({ conf = conf_on(), call_reserve = 100 })
        end).to.fail()
        expect(function()
            policy.thinking_cap({})
        end).to.fail()
    end)

    it("needs a conf, and one that turns reasoning on", function()
        local port = port_of(1000, 100)
        expect(function()
            policy.thinking_cap({ room = policy.room({ port = port, call_reserve = 100 }) })
        end).to.fail()
        -- A conf that says nothing about thinking: the budget would be the
        -- thing that turned reasoning on, which is not this filter's to do.
        local silent = policy.room({ port = port, conf = {}, call_reserve = 100 })
        local ok, err = pcall(policy.thinking_cap, { room = silent })
        expect(ok).to.be(false)
        expect(tostring(err):find("does not turn reasoning on", 1, true) ~= nil).to.be(true)
        local off = policy.room({ port = port, conf = { thinking = false }, call_reserve = 100 })
        expect(function()
            policy.thinking_cap({ room = off })
        end).to.fail()
        local disabled = policy.room({ port = port, conf = { thinking = { enabled = false } }, call_reserve = 100 })
        expect(function()
            policy.thinking_cap({ room = disabled })
        end).to.fail()
        -- Either spelling of "on" is taken.
        local as_true = policy.room({ port = port, conf = { thinking = true }, call_reserve = 100 })
        expect(type(policy.thinking_cap({ room = as_true }))).to.be("function")
        local as_table = policy.room({ port = port, conf = { thinking = { effort = "high" } }, call_reserve = 100 })
        expect(type(policy.thinking_cap({ room = as_table }))).to.be("function")
    end)

    it("needs a room built with a call_reserve, and the room takes whole tokens for it", function()
        local port = port_of(1000, 100)
        local ok, err = pcall(policy.thinking_cap, { room = policy.room({ port = port, conf = conf_on() }) })
        expect(ok).to.be(false)
        expect(tostring(err):find("the room was built without call_reserve", 1, true) ~= nil).to.be(true)
        for _, bad in ipairs({ -1, 0.5, "some" }) do
            expect(function()
                policy.room({ port = port, conf = conf_on(), call_reserve = bad })
            end).to.fail()
        end
    end)

    it("refuses a budget that is not whole tokens, and an option it does not know", function()
        local room = policy.room({ port = port_of(1000, 100), conf = conf_on(), call_reserve = 100 })
        expect(function()
            policy.thinking_cap({ room = room, budget = 0 })
        end).to.fail()
        expect(function()
            policy.thinking_cap({ room = room, call_reserved = 100 })
        end).to.fail()
        -- `reserve` in particular: what the fold held back is the room's,
        -- not named here a second time.
        expect(function()
            policy.thinking_cap({ room = room, reserve = 200 })
        end).to.fail()
    end)
end)

--- The filter over a room built from these, as every case below builds it.
local function cap(port, conf, call_reserve, budget)
    local room = policy.room({ port = port, conf = conf, call_reserve = call_reserve })
    return policy.thinking_cap({ room = room, budget = budget })
end

describe("policy.thinking_cap — the stop point", function()
    it("is the reply's room less what the call needs: window - count - call_reserve", function()
        -- 1000 window, 100 counted, 150 for the call: 1000 - 100 - 150.
        local filter = cap(port_of(1000, 100), conf_on(), 150)
        expect(filter(request_of()).thinking.budget_tokens).to.be(750)
    end)

    it("shrinks as the request grows, which is the whole point", function()
        local function stop_at(counted)
            local filter = cap(port_of(1000, counted), conf_on(), 100)
            return filter(request_of()).thinking.budget_tokens
        end
        expect(stop_at(100)).to.be(800)
        expect(stop_at(600)).to.be(300)
    end)

    it("is bounded by the cap the wire carries, when the window leaves more than that", function()
        -- 1000 window, 100 counted leaves 900; the wire caps the reply at 400,
        -- so the reasoning has 400 - 150 to stop in, not 900 - 150.
        local filter = cap(port_of(1000, 100, 400), conf_on(), 150)
        expect(filter(request_of()).thinking.budget_tokens).to.be(250)
    end)

    it("is bounded by the window, when the cap is more than what is left", function()
        -- 1000 window, 800 counted leaves 200; a cap of 400 is no bound on
        -- that, so it is 200 - 50.
        local filter = cap(port_of(1000, 800, 400), conf_on(), 50)
        expect(filter(request_of()).thinking.budget_tokens).to.be(150)
    end)

    it("is capped by budget, and only when budget is the smaller number", function()
        local function with_budget(budget)
            local filter = cap(port_of(1000, 100), conf_on(), 100, budget)
            return filter(request_of()).thinking.budget_tokens
        end
        expect(with_budget(256)).to.be(256)
        expect(with_budget(5000)).to.be(800)
    end)

    it("leaves the request alone when there is no room left to think in", function()
        local request = request_of()
        local filter = cap(port_of(1000, 900), conf_on(), 100)
        -- 1000 - 900 - 100 = 0: not a stop point, so nothing is sent.
        expect(filter(request)).to.be(request)
        expect(request.thinking).to.be(nil)

        local over = cap(port_of(1000, 950), conf_on(), 100)
        expect(over(request).thinking).to.be(nil)
    end)

    it("still fires on the most crowded beat, where the fold held back exactly the reply's room", function()
        -- The case the filter exists for: a 32k window, a prompt the fold let
        -- through at 24,984 against a reserve of 6,144, and a call that needs
        -- 3,072. The room is 7,784 and the reasoning stops at 4,712 — not at
        -- nothing, which is what subtracting the reserve a second time gave.
        local filter = cap(port_of(32768, 24984), conf_on(), 3072)
        expect(filter(request_of()).thinking.budget_tokens).to.be(4712)
    end)

    it("keeps the conf's own thinking keys and adds the budget to them", function()
        local conf = { thinking = { enabled = true, effort = "high", kwarg = "enable_thinking" } }
        local filter = cap(port_of(1000, 100), conf, 100)
        local thinking = filter(request_of()).thinking
        expect(thinking.enabled).to.be(true)
        expect(thinking.effort).to.be("high")
        expect(thinking.kwarg).to.be("enable_thinking")
        expect(thinking.budget_tokens).to.be(800)
    end)

    it("carries a `thinking = true` across as enabled", function()
        local filter = cap(port_of(1000, 100), { thinking = true }, 100)
        expect(filter(request_of()).thinking.enabled).to.be(true)
    end)

    it("does not change the request it was handed", function()
        local request = request_of()
        local filter = cap(port_of(1000, 100), conf_on(), 100)
        local out = filter(request)

        expect(request.thinking).to.be(nil)
        expect(out).to_not.be(request)
        -- Everything else is carried over.
        expect(out.system).to.be("SYS")
        expect(out.messages).to.be(request.messages)
    end)

    it("asks the port for the count on every request, and for the profile once — the room's reading", function()
        -- The room reads the profile once, where it is built (policy.room's
        -- header); the filter asks nothing more of it, and the count is per
        -- request.
        local port = port_of(1000, 100)
        local room = policy.room({ port = port, conf = conf_on(), call_reserve = 100 })
        expect(port.asked.profile).to.be(1)
        local filter = policy.thinking_cap({ room = room })
        filter(request_of())
        filter(request_of())
        expect(port.asked.profile).to.be(1)
        expect(port.asked.count).to.be(2)
    end)

    it("is refused where the room is built when the port's profile names no window", function()
        local blind = {
            profile = function()
                return {}
            end,
            count = function()
                return 100
            end,
        }
        local ok, err = pcall(policy.room, { port = blind, conf = conf_on(), call_reserve = 100 })
        expect(ok).to.be(false)
        expect(tostring(err):find("names no context_window", 1, true) ~= nil).to.be(true)
    end)
end)
