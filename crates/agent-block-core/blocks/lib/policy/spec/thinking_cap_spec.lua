-- thinking_cap_spec.lua — mlua-lspec unit tests for `policy.thinking_cap`,
-- the filter that sends the reasoning's stop point with every request.
--
-- Run via:
--   just test-lua thinking_cap
--
-- What this proves:
--   1 the stop point is the room the request leaves, less what the call
--     needs: window - count - reserve - call_reserve;
--   2 `budget` is a ceiling on it, and only when it is the smaller number;
--   3 a request that leaves no room is left exactly as it is — there is no
--     stop point worth sending, and 0 is not one;
--   4 the conf's own thinking keys survive: the filter adds a budget, it does
--     not replace the caller's reasoning settings, and `thinking = false`
--     stays off;
--   5 the request handed in is not changed — what comes back is a copy;
--   6 the bounds are loud: no port, a reserve or call_reserve that is not
--     whole tokens, no call_reserve at all, an unknown option, and a port
--     whose profile names no window.

local describe, it, expect = lust.describe, lust.it, lust.expect

local policy = require("policy")

--- A Port whose window is `window` and which counts `tokens` for any request.
--- Every call is recorded, so a case can say the filter asked rather than
--- assumed.
local function port_of(window, tokens)
    local asked = { profile = 0, count = 0 }
    return {
        asked = asked,
        profile = function(self)
            self.asked.profile = self.asked.profile + 1
            return { context_window = window }
        end,
        count = function(self)
            self.asked.count = self.asked.count + 1
            return tokens
        end,
    }
end

--- The request a filter is handed: the shape `knl.fold` answers.
local function request_of()
    return { messages = { { role = "user", content = "do it" } }, system = "SYS" }
end

describe("policy.thinking_cap — construction", function()
    it("answers a filter", function()
        local filter = policy.thinking_cap({ port = port_of(1000, 100), call_reserve = 100 })
        expect(type(filter)).to.be("function")
    end)

    it("refuses a port that cannot answer both questions", function()
        expect(function()
            policy.thinking_cap({ port = { count = function() end }, call_reserve = 100 })
        end).to.fail()
        expect(function()
            policy.thinking_cap({ call_reserve = 100 })
        end).to.fail()
    end)

    it("needs a call_reserve, and takes whole tokens for it and for reserve", function()
        local port = port_of(1000, 100)
        expect(function()
            policy.thinking_cap({ port = port })
        end).to.fail()
        for _, bad in ipairs({ -1, 0.5, "some" }) do
            expect(function()
                policy.thinking_cap({ port = port, call_reserve = bad })
            end).to.fail()
            expect(function()
                policy.thinking_cap({ port = port, call_reserve = 100, reserve = bad })
            end).to.fail()
        end
    end)

    it("refuses a budget that is not whole tokens, and an option it does not know", function()
        local port = port_of(1000, 100)
        expect(function()
            policy.thinking_cap({ port = port, call_reserve = 100, budget = 0 })
        end).to.fail()
        expect(function()
            policy.thinking_cap({ port = port, call_reserve = 100, call_reserved = 100 })
        end).to.fail()
    end)
end)

describe("policy.thinking_cap — the stop point", function()
    it("is the room the request leaves, less what the call needs", function()
        -- 1000 window, 100 counted, 200 held back for the reply, 150 for the
        -- call: 1000 - 100 - 200 - 150.
        local filter = policy.thinking_cap({
            port = port_of(1000, 100),
            reserve = 200,
            call_reserve = 150,
        })
        expect(filter(request_of()).thinking.budget_tokens).to.be(550)
    end)

    it("shrinks as the request grows, which is the whole point", function()
        local function stop_at(counted)
            local filter = policy.thinking_cap({ port = port_of(1000, counted), reserve = 100, call_reserve = 100 })
            return filter(request_of()).thinking.budget_tokens
        end
        expect(stop_at(100)).to.be(700)
        expect(stop_at(600)).to.be(200)
    end)

    it("takes reserve as 0 when the caller holds nothing back", function()
        local filter = policy.thinking_cap({ port = port_of(1000, 100), call_reserve = 100 })
        expect(filter(request_of()).thinking.budget_tokens).to.be(800)
    end)

    it("is capped by budget, and only when budget is the smaller number", function()
        local function with_budget(budget)
            local filter = policy.thinking_cap({
                port = port_of(1000, 100),
                reserve = 100,
                call_reserve = 100,
                budget = budget,
            })
            return filter(request_of()).thinking.budget_tokens
        end
        expect(with_budget(256)).to.be(256)
        expect(with_budget(5000)).to.be(700)
    end)

    it("leaves the request alone when there is no room left to think in", function()
        local request = request_of()
        local filter = policy.thinking_cap({ port = port_of(1000, 900), reserve = 50, call_reserve = 50 })
        -- 1000 - 900 - 50 - 50 = 0: not a stop point, so nothing is sent.
        expect(filter(request)).to.be(request)
        expect(request.thinking).to.be(nil)

        local over = policy.thinking_cap({ port = port_of(1000, 900), reserve = 100, call_reserve = 100 })
        expect(over(request).thinking).to.be(nil)
    end)

    it("keeps the conf's own thinking keys and adds the budget to them", function()
        local filter = policy.thinking_cap({
            port = port_of(1000, 100),
            conf = { thinking = { enabled = true, effort = "high", kwarg = "enable_thinking" } },
            reserve = 100,
            call_reserve = 100,
        })
        local thinking = filter(request_of()).thinking
        expect(thinking.enabled).to.be(true)
        expect(thinking.effort).to.be("high")
        expect(thinking.kwarg).to.be("enable_thinking")
        expect(thinking.budget_tokens).to.be(700)
    end)

    it("carries a `thinking = false` across rather than turning reasoning back on", function()
        local filter = policy.thinking_cap({
            port = port_of(1000, 100),
            conf = { thinking = false },
            call_reserve = 100,
        })
        expect(filter(request_of()).thinking.enabled).to.be(false)
    end)

    it("does not change the request it was handed", function()
        local request = request_of()
        local filter = policy.thinking_cap({ port = port_of(1000, 100), call_reserve = 100 })
        local out = filter(request)

        expect(request.thinking).to.be(nil)
        expect(out).to_not.be(request)
        -- Everything else is carried over.
        expect(out.system).to.be("SYS")
        expect(out.messages).to.be(request.messages)
    end)

    it("asks the port for the window and the count on every request", function()
        local port = port_of(1000, 100)
        local filter = policy.thinking_cap({ port = port, call_reserve = 100 })
        filter(request_of())
        filter(request_of())
        expect(port.asked.profile).to.be(2)
        expect(port.asked.count).to.be(2)
    end)

    it("raises when the port's profile names no window", function()
        local blind = {
            profile = function()
                return {}
            end,
            count = function()
                return 100
            end,
        }
        local filter = policy.thinking_cap({ port = blind, call_reserve = 100 })
        expect(function()
            filter(request_of())
        end).to.fail()
    end)
end)
