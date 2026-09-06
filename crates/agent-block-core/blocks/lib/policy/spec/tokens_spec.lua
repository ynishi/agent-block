-- tokens_spec.lua — mlua-lspec unit tests for `policy.tokens`, the cost that
-- reserves a beat's request in tokens.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/tokens_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- What this proves:
--   1 the cost answers the Port's count for the request, and the conf the
--     policy was built with is what the Port is asked with;
--   2 the kernel's floor holds — a request the Port counts at zero still
--     costs one, because a beat that could ask for nothing is how a run
--     stops being finite;
--   3 a Port that cannot count is refused at construction, and one that
--     answers something that is not a whole number is refused at the call;
--   4 driven through a real beat: a grant tagged "tokens" is drawn down by
--     the request's count, and a request that would overrun it is
--     `stopped("budget")` with no call made.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")

--- A Port that counts one token per message and remembers what it was asked.
local function counting_port()
    local port = { asked = {} }
    function port:count(request, conf)
        self.asked[#self.asked + 1] = { request = request, conf = conf }
        return #request.messages
    end
    return port
end

describe("policy.tokens — construction", function()
    it("answers a cost, which is what a device takes", function()
        local cost = policy.tokens({ port = counting_port() })
        expect(type(cost)).to.be("function")
        local d = kernel.device({ llm = support.always(support.text("x")), cost = cost })
        expect(d.cost).to.be(cost)
    end)

    it("refuses a port that cannot count, an unknown option, and a conf that is not a table", function()
        expect(function()
            policy.tokens({ port = {} })
        end).to.fail()
        expect(function()
            policy.tokens({})
        end).to.fail()
        expect(function()
            policy.tokens({ port = counting_port(), prot = 1 })
        end).to.fail()
        expect(function()
            policy.tokens({ port = counting_port(), conf = "m" })
        end).to.fail()
    end)
end)

describe("policy.tokens — the count", function()
    it("is the port's, asked with the conf the policy was built with", function()
        local port = counting_port()
        local conf = { model = "m" }
        local cost = policy.tokens({ port = port, conf = conf })
        expect(cost({ messages = { {}, {}, {} } })).to.be(3)
        expect(port.asked[1].conf).to.be(conf)
    end)

    it("never answers below one — an empty request still spends a beat", function()
        local cost = policy.tokens({ port = counting_port() })
        expect(cost({ messages = {} })).to.be(1)
    end)

    it("refuses a count that is not a whole number >= 0", function()
        for _, bad in ipairs({ -1, 1.5, "3", nil }) do
            local port = {
                count = function()
                    return bad
                end,
            }
            local cost = policy.tokens({ port = port })
            expect(function()
                cost({ messages = {} })
            end).to.fail()
        end
    end)
end)

describe("policy.tokens — driving a real beat", function()
    it("draws a grant tagged tokens down by the request's count, and stops before it overruns", function()
        local port = counting_port()
        local device = kernel.device({
            llm = support.always(support.text("ok")),
            cost = policy.tokens({ port = port }),
        })
        -- Each beat's request grows by two messages (the seed, then each
        -- exchange), so the costs run 1, 3, 5 …; a grant of 4 covers the
        -- first two and refuses the third before any call.
        kernel.session({ owner = "u", budget = { amount = 4, tag = "tokens" } }, function(s)
            s:append({ kind = "msg_user", data = { content = "seed" } })
            local first = kernel.beat(s, device)
            expect(first.status).to.be("ok")
            local second = kernel.beat(s, device)
            expect(second.status).to.be("ok")
            local third = kernel.beat(s, device)
            expect(third.status).to.be("stopped")
            expect(third.reason).to.be("budget")
            expect(#port.asked >= 3).to.be(true)
        end)
    end)
end)
