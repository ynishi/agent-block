-- split_spec.lua — mlua-lspec unit tests for `policy.split`, the window's
-- split between prompt and reply handed back as a value.
--
-- Run via:
--   just test-lua split
--
-- What this proves:
--   1 the four numbers come off the Port's profile: window, the wire's cap
--     (0 for none), the prompt's limit and what reserve held beyond the cap;
--   2 `room` is on the answer only when `used` is given, and it is the
--     reply's side: window - used, under the cap where there is one;
--   3 the numbers are the ones `window` and `thinking_cap` size by — the
--     same profile, the same arithmetic, so the reading matches the acting;
--   4 the bounds are loud: no port, a reserve or used that is not whole
--     tokens, an unknown option, a profile with no window.

local describe, it, expect = lust.describe, lust.it, lust.expect

local policy = require("policy")

--- A Port whose window is `window` and whose wire cap is `max_output` (nil
--- for none); `count` answers `tokens` for any request.
local function port_of(window, max_output, tokens)
    return {
        profile = function()
            return { context_window = window, max_output = max_output }
        end,
        count = function()
            return tokens or 0
        end,
    }
end

describe("policy.split — the numbers", function()
    it("reads window, cap, limit and held off the profile", function()
        -- No cap on the wire: the reserve is held whole, and the prompt may
        -- take the rest.
        local s = policy.split({ port = port_of(32768), reserve = 6144 })
        expect(s.window).to.be(32768)
        expect(s.max_output).to.be(0)
        expect(s.held).to.be(6144)
        expect(s.limit).to.be(32768 - 6144)
        expect(s.room).to.be(nil)
    end)

    it("holds back only what the cap does not already", function()
        -- A cap of 4096 already comes out of the window; a reserve of 6144
        -- holds 2048 beyond it, a reserve of 1024 holds nothing extra.
        local more = policy.split({ port = port_of(32768, 4096), reserve = 6144 })
        expect(more.max_output).to.be(4096)
        expect(more.held).to.be(2048)
        expect(more.limit).to.be(32768 - 4096 - 2048)
        local less = policy.split({ port = port_of(32768, 4096), reserve = 1024 })
        expect(less.held).to.be(0)
        expect(less.limit).to.be(32768 - 4096)
    end)

    it("takes no reserve as none", function()
        local s = policy.split({ port = port_of(1000) })
        expect(s.held).to.be(0)
        expect(s.limit).to.be(1000)
    end)

    it("answers the reply's room when told what the request costs", function()
        -- The motivating case for thinking_cap: 24,984 of 32k, no cap, so the
        -- reply has what the window has left.
        local s = policy.split({ port = port_of(32768), reserve = 6144, used = 24984 })
        expect(s.room).to.be(7784)
        -- Under a cap, the room is the cap when the window leaves more.
        expect(policy.split({ port = port_of(1000, 400), used = 100 }).room).to.be(400)
        -- And the window's remainder when it leaves less.
        expect(policy.split({ port = port_of(1000, 400), used = 800 }).room).to.be(200)
    end)

    it("matches what thinking_cap sizes by", function()
        -- The reading and the acting come off one arithmetic: the stop point
        -- thinking_cap sends is the room less call_reserve.
        local port = port_of(32768, nil, 24984)
        local conf = { thinking = true }
        local stop = policy.thinking_cap({ port = port, conf = conf, call_reserve = 3072 })({ messages = {} })
        local room = policy.split({ port = port, conf = conf, used = 24984 }).room
        expect(stop.thinking.budget_tokens).to.be(room - 3072)
    end)
end)

describe("policy.split — the bounds", function()
    it("needs a port that answers profile", function()
        expect(function()
            policy.split({})
        end).to.fail()
        expect(function()
            policy.split({ port = { count = function() end } })
        end).to.fail()
    end)

    it("takes whole tokens for reserve and used, and no option it does not know", function()
        local port = port_of(1000)
        for _, bad in ipairs({ -1, 0.5, "some" }) do
            expect(function()
                policy.split({ port = port, reserve = bad })
            end).to.fail()
            expect(function()
                policy.split({ port = port, used = bad })
            end).to.fail()
        end
        expect(function()
            policy.split({ port = port, call_reserve = 100 })
        end).to.fail()
    end)

    it("raises when the profile names no window, or a cap that leaves no room", function()
        expect(function()
            policy.split({
                port = {
                    profile = function()
                        return {}
                    end,
                },
            })
        end).to.fail()
        expect(function()
            policy.split({ port = port_of(1000, 1000) })
        end).to.fail()
    end)
end)
