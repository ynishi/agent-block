-- room_spec.lua — mlua-lspec unit tests for `policy.room`, the window as one
-- value, and for the six policies reading it instead of `{ port, conf, ... }`.
--
-- Run via:
--   just test-lua room_spec
--
-- What this proves:
--   1 construction: the profile is read once and divided once — window,
--     max_output, limit, held, result_limit, beat_budget — with the defaults
--     the shares take, and `is_room` tells a room from a table;
--   2 the bounds are loud: a port that cannot count or answer a profile, a
--     reserve or call_reserve that is not whole tokens, a share outside
--     (0, 1], a beat_share under the result_share, a profile naming no window,
--     an unknown option, a session as one; and a room is frozen;
--   3 the readings: split (with and without `used`), reply under the wire's
--     cap, thinking_stop (under the room, under the caller's budget, nil when
--     nothing is left, refused on a room with no call_reserve), count and
--     count_text through the Port, limits_for_tools in the std.fs shape;
--   4 the six take `room` in place of the older form, refuse both at once,
--     and size by the room — the fold's limit, the cost, the result cap's
--     limit, the stop point, the window line; `policy.split` answers what
--     `room:split` answers.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")

--- A Port whose window is `window`, whose wire cap is `max_output` (nil for
--- none), and whose count is the length of every string content.
local function port_of(window, max_output)
    local asked = { profile = 0, count = 0 }
    return {
        asked = asked,
        profile = function(self)
            self.asked.profile = self.asked.profile + 1
            return { context_window = window, max_output = max_output }
        end,
        count = function(self, request)
            self.asked.count = self.asked.count + 1
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

local function request_of(text)
    return { messages = { { role = "user", content = text } }, system = "SYS" }
end

-- ─────────────────────────────────────────────────────────────────────────────
-- 1 construction
-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.room — construction", function()
    it("reads the profile once and divides the window once", function()
        local port = port_of(1000, 100)
        local room =
            policy.room({ port = port, reserve = 150, result_share = 0.3, beat_share = 0.5, call_reserve = 20 })
        expect(port.asked.profile).to.be(1)
        expect(room.window).to.be(1000)
        expect(room.max_output).to.be(100)
        -- held = reserve beyond the cap; limit = window - cap - held
        expect(room.held).to.be(50)
        expect(room.limit).to.be(850)
        expect(room.reserve).to.be(150)
        expect(room.result_share).to.be(0.3)
        expect(room.result_limit).to.be(255)
        expect(room.beat_share).to.be(0.5)
        expect(room.beat_budget).to.be(425)
        expect(room.call_reserve).to.be(20)
        expect(room.port).to.be(port)
        expect(policy.is_room(room)).to.be(true)
        expect(policy.is_room({ limit = 850 })).to.be(false)
        expect(policy.is_room(nil)).to.be(false)
    end)

    it("takes the defaults: no reserve, a quarter for one result, half for a beat, no call_reserve", function()
        local room = policy.room({ port = port_of(1000, 0) })
        expect(room.reserve).to.be(0)
        expect(room.held).to.be(0)
        expect(room.limit).to.be(1000)
        expect(room.result_limit).to.be(250)
        expect(room.beat_budget).to.be(500)
        expect(room.call_reserve).to.be(nil)
    end)
end)

describe("policy.room — the bounds", function()
    it("refuses a port that cannot count or answer a profile, and a profile naming no window", function()
        local ok, err = pcall(policy.room, { port = {} })
        expect(ok).to.be(false)
        expect(tostring(err):find("port must answer count", 1, true)).to.exist()
        local blind = {
            profile = function()
                return {}
            end,
            count = function()
                return 0
            end,
        }
        local ok2, err2 = pcall(policy.room, { port = blind })
        expect(ok2).to.be(false)
        expect(tostring(err2):find("names no context_window", 1, true)).to.exist()
    end)

    it(
        "holds reserve and call_reserve to whole tokens, and the shares to (0, 1] with beat_share >= result_share",
        function()
            local port = port_of(1000, 0)
            local ok, err = pcall(policy.room, { port = port, reserve = 1.5 })
            expect(ok).to.be(false)
            expect(tostring(err):find("reserve must be a whole number", 1, true)).to.exist()
            local ok2, err2 = pcall(policy.room, { port = port, call_reserve = -1 })
            expect(ok2).to.be(false)
            expect(tostring(err2):find("call_reserve must be a whole number", 1, true)).to.exist()
            local ok3, err3 = pcall(policy.room, { port = port, result_share = 0 })
            expect(ok3).to.be(false)
            expect(tostring(err3):find("result_share must be a number in (0, 1]", 1, true)).to.exist()
            local ok4, err4 = pcall(policy.room, { port = port, beat_share = 1.5 })
            expect(ok4).to.be(false)
            expect(tostring(err4):find("beat_share must be a number in (0, 1]", 1, true)).to.exist()
            local ok5, err5 = pcall(policy.room, { port = port, result_share = 0.5, beat_share = 0.4 })
            expect(ok5).to.be(false)
            expect(tostring(err5):find("beat_share (0.4) is under result_share (0.5)", 1, true)).to.exist()
        end
    )

    it("refuses an option it does not know, a session most of all, and a reserve that leaves no room", function()
        local ok, err = pcall(policy.room, { port = port_of(1000, 0), shar = 0.5 })
        expect(ok).to.be(false)
        expect(tostring(err):find("unknown option 'shar'", 1, true)).to.exist()
        local ok2, err2 = pcall(policy.room, { port = port_of(1000, 0), session = support.session() })
        expect(ok2).to.be(false)
        expect(tostring(err2):find("a session is an argument, never an option", 1, true)).to.exist()
        local ok3, err3 = pcall(policy.room, { port = port_of(1000, 0), reserve = 1000 })
        expect(ok3).to.be(false)
        expect(tostring(err3):find("leaves no room", 1, true)).to.exist()
    end)

    it("is frozen", function()
        local room = policy.room({ port = port_of(1000, 0) })
        local ok, err = pcall(function()
            room.limit = 1
        end)
        expect(ok).to.be(false)
        expect(tostring(err):find("a room is frozen", 1, true)).to.exist()
        expect(room.limit).to.be(1000)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- 3 the readings
-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.room — the readings", function()
    it("answers the split as a value, and the reply's room once the request is known", function()
        local room = policy.room({ port = port_of(32768, 0), reserve = 6144 })
        local s = room:split()
        expect(s).to.equal({ window = 32768, max_output = 0, limit = 26624, held = 6144 })
        expect(room:split(24984).room).to.be(7784)
        expect(room:reply(24984)).to.be(7784)
        -- Under the wire's cap where there is one, and negative past the window.
        local capped = policy.room({ port = port_of(32768, 4096) })
        expect(capped:reply(1000)).to.be(4096)
        expect(capped:reply(32768 + 10)).to.be(-10)
        expect(policy.split({ port = port_of(32768, 0), reserve = 6144, used = 24984 })).to.equal(room:split(24984))
    end)

    it(
        "answers where the reasoning stops: under the room, under the caller's budget, nil when nothing is left",
        function()
            local room = policy.room({ port = port_of(1000, 0), call_reserve = 100 })
            expect(room:thinking_stop(400)).to.be(500)
            expect(room:thinking_stop(400, 120)).to.be(120)
            expect(room:thinking_stop(950)).to.be(nil)
            local none = policy.room({ port = port_of(1000, 0) })
            local ok, err = pcall(none.thinking_stop, none, 400)
            expect(ok).to.be(false)
            expect(tostring(err):find("built without call_reserve", 1, true)).to.exist()
        end
    )

    it("counts through the Port, and hands std.fs its limits", function()
        local port = port_of(1000, 0)
        local room = policy.room({ port = port, result_share = 0.1 })
        expect(room:count(request_of("abcd"))).to.be(4)
        expect(room:count_text("abcdef")).to.be(6)
        local limits = room:limits_for_tools()
        expect(limits.result_tokens).to.be(100)
        expect(limits.count("xyz")).to.be(3)
        expect(port.asked.count).to.be(3)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- 4 the six, over a room
-- ─────────────────────────────────────────────────────────────────────────────

describe("the policies read the room", function()
    local function beat(id, body, seq)
        return {
            { kind = "llm_request", meta = { beat = id }, data = { request = { messages = {} } }, seq = seq },
            {
                kind = "llm_response",
                meta = { beat = id },
                data = { content = { { type = "text", text = body } }, usage = {} },
                seq = seq + 1,
            },
        }
    end
    local function concat(...)
        local out = {}
        for _, list in ipairs({ ... }) do
            for _, ev in ipairs(list) do
                out[#out + 1] = ev
            end
        end
        return out
    end

    it("window{ room } sizes the fold by the room's limit and reports it", function()
        -- Assistant text is a block, not a string, so the port counts the seed
        -- only: the room's limit is what the report names.
        local room = policy.room({ port = port_of(100, 10), reserve = 20 })
        local fold = policy.window({ room = room, keep_seed = true })
        local events = concat(
            { { kind = "msg_user", data = { content = "task" }, seq = 1 } },
            beat("b1", "one", 2),
            beat("b2", "two", 4)
        )
        local request, report = fold(events, {})
        expect(#request.messages).to.be(3)
        expect(report.limit).to.be(room.limit)
        expect(report.reserve).to.be(room.held)
        expect(report.after).to.be(4)
        local ok, err = pcall(policy.window, { room = room, fit = { port = port_of(100, 0) } })
        expect(ok).to.be(false)
        expect(tostring(err):find("pass `room` or `fit`, not both", 1, true)).to.exist()
        local ok2, err2 = pcall(policy.window, { room = { limit = 1 } })
        expect(ok2).to.be(false)
        expect(tostring(err2):find("room must be a value from policy.room", 1, true)).to.exist()
    end)

    it("tokens{ room } costs a beat by the room's count", function()
        local cost = policy.tokens({ room = policy.room({ port = port_of(100, 0) }) })
        expect(cost(request_of("abcdef"))).to.be(6)
        expect(cost({ messages = {} })).to.be(1)
        expect(pcall(policy.tokens, { room = policy.room({ port = port_of(100, 0) }), port = port_of(100, 0) })).to.be(
            false
        )
    end)

    it("result_cap{ room } holds one result to the room's result_limit", function()
        local room = policy.room({ port = port_of(100, 0), result_share = 0.1 })
        local tools = policy.result_cap({ room = room })(support.tool("t", string.rep("x", 12)))
        local answer = tools.t.handler({})
        expect(answer.ok).to.be(false)
        expect(answer.reason).to.be("result_too_large")
        expect(answer.limit).to.be(10)
        expect(answer.tokens).to.be(12)
        local fits = policy.result_cap({ room = room })(support.tool("t", string.rep("x", 10)))
        expect(fits.t.handler({})).to.be(string.rep("x", 10))
        expect(pcall(policy.result_cap, { room = room, share = 0.5 })).to.be(false)
    end)

    it("thinking_cap{ room } sends the room's stop point, and needs a room with call_reserve", function()
        local port = port_of(1000, 0)
        local room = policy.room({ port = port, conf = { thinking = true }, call_reserve = 100 })
        local filter = policy.thinking_cap({ room = room })
        local out = filter(request_of(string.rep("x", 400)))
        expect(out.thinking.budget_tokens).to.be(500)
        expect(out.thinking.enabled).to.be(true)
        local under = policy.thinking_cap({ room = room, budget = 50 })(request_of(string.rep("x", 400)))
        expect(under.thinking.budget_tokens).to.be(50)
        local ok, err = pcall(policy.thinking_cap, { room = policy.room({ port = port, conf = { thinking = true } }) })
        expect(ok).to.be(false)
        expect(tostring(err):find("built without call_reserve", 1, true)).to.exist()
        local ok2, err2 = pcall(policy.thinking_cap, { room = policy.room({ port = port, call_reserve = 1 }) })
        expect(ok2).to.be(false)
        expect(tostring(err2):find("does not turn reasoning on", 1, true)).to.exist()
        expect(pcall(policy.thinking_cap, { room = room, call_reserve = 5 })).to.be(false)
    end)

    it("room_note{ room } says what the request took off the room's numbers", function()
        local room = policy.room({ port = port_of(100, 0) })
        local out = policy.room_note({ room = room })(request_of(string.rep("x", 40)))
        expect(out.messages[2].content).to.be("[window] 40 of 100 tokens used; 60 left for the reply")
        expect(pcall(policy.room_note, { room = room, conf = {} })).to.be(false)
    end)
end)
