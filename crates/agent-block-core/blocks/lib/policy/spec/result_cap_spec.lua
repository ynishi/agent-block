-- result_cap_spec.lua — mlua-lspec unit tests for `policy.result_cap`, the
-- wrapper that refuses a tool result larger than a share of the model's
-- window.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/result_cap_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- What this proves:
--   1 a result within the share passes through untouched — the same value,
--     not a copy of it;
--   2 a result over the share is refused as an answer, not a raise, and the
--     refusal names the size, the limit and what to do instead;
--   3 the limit is read off the ROOM — the Port's window less the answer's
--     room, times the room's `result_share` — so the same tool is capped
--     differently on two models;
--   4 the map handed back is a new one and the caller's is untouched —
--     including every field beside the handler;
--   5 a string result is measured as the string, a table as the JSON the
--     kernel would render it into;
--   6 the bounds are loud: no room, an unknown option, a tool without a
--     handler — and, where the room is built, a port with no count or no
--     profile and a share outside (0, 1].

local describe, it, expect = lust.describe, lust.it, lust.expect

-- The cap measures a table result as the JSON the kernel would render it
-- into; the shared encoder `support` installs (knl/spec/json_stub.lua) grows
-- with the value, which is what this spec needs of it.
require("policy.spec.support")
local policy = require("policy")

--- A Port whose window is `window`, whose answer takes `output`, and which
--- counts one token per four characters of the request's content.
local function port_of(window, output)
    return {
        profile = function()
            return { context_window = window, max_output = output }
        end,
        count = function(_, request)
            local n = 0
            for _, message in ipairs(request.messages) do
                n = n + #tostring(message.content)
            end
            return math.ceil(n / 4)
        end,
    }
end

--- One tool that answers whatever it is told to.
local function tool_of(answer)
    return {
        read = {
            description = "read something",
            input_schema = { type = "object" },
            handler = function()
                return answer
            end,
        },
    }
end

describe("policy.result_cap — construction", function()
    it("answers a bind, which takes a tools map", function()
        local bind = policy.result_cap({ room = policy.room({ port = port_of(1000, 100) }) })
        expect(type(bind)).to.be("function")
        expect(type(bind(tool_of("x")))).to.be("table")
    end)

    it("refuses no room, and a port that cannot answer both questions is refused by the room", function()
        expect(function()
            policy.room({ port = { count = function() end } })
        end).to.fail()
        expect(function()
            policy.room({ port = { profile = function() end } })
        end).to.fail()
        expect(function()
            policy.result_cap({})
        end).to.fail()
    end)

    it("the room refuses a share outside (0, 1], and result_cap an option it does not know", function()
        for _, bad in ipairs({ 0, -0.5, 1.5, "half" }) do
            expect(function()
                policy.room({ port = port_of(1000, 100), result_share = bad })
            end).to.fail()
        end
        expect(function()
            policy.result_cap({ room = policy.room({ port = port_of(1000, 100) }), shair = 0.5 })
        end).to.fail()
    end)

    it("refuses a tools map with a handler-less entry", function()
        local bind = policy.result_cap({ room = policy.room({ port = port_of(1000, 100) }) })
        expect(function()
            bind({ read = { description = "no handler" } })
        end).to.fail()
    end)
end)

describe("policy.result_cap — the cap", function()
    -- window 1000 less 100 of room is 900; a quarter of that is 225 tokens,
    -- which at four characters to the token is 900 characters.
    local port = port_of(1000, 100)
    local room = policy.room({ port = port })

    it("passes a result within the share through as it is", function()
        local answer = { content = string.rep("a", 400), lines = 4 }
        local tools = policy.result_cap({ room = room })(tool_of(answer))
        expect(tools.read.handler({})).to.be(answer)
    end)

    it("refuses one over the share, as an answer and not a raise", function()
        local tools = policy.result_cap({ room = room })(tool_of(string.rep("a", 4000)))
        local res = tools.read.handler({})
        expect(res.ok).to.be(false)
        expect(res.reason).to.be("result_too_large")
        expect(res.tokens > res.limit).to.be(true)
        expect(res.limit).to.be(225)
        -- The refusal says what to do, not only that something was wrong.
        expect(res.error:find("read", 1, true) ~= nil).to.be(true)
        expect(res.error:find("smaller piece", 1, true) ~= nil).to.be(true)
    end)

    it("reads the limit off the room's port, so the same tool caps differently per model", function()
        local answer = string.rep("a", 4000)
        local narrow = policy.result_cap({ room = policy.room({ port = port_of(1000, 100) }) })(tool_of(answer))
        local wide = policy.result_cap({ room = policy.room({ port = port_of(1000000, 100) }) })(tool_of(answer))
        expect(narrow.read.handler({}).ok).to.be(false)
        expect(wide.read.handler({})).to.be(answer)
    end)

    it("takes the share the room was built with", function()
        local answer = string.rep("a", 1200) -- 300 tokens
        local quarter = policy.result_cap({ room = room })(tool_of(answer))
        local half = policy.result_cap({ room = policy.room({ port = port, result_share = 0.5 }) })(tool_of(answer))
        expect(quarter.read.handler({}).ok).to.be(false)
        expect(half.read.handler({})).to.be(answer)
    end)

    it("measures a table as the JSON the kernel would render", function()
        -- Short as a Lua table, long once encoded: the encoding is what
        -- reaches the request, so it is what the cap has to look at.
        local tools = policy.result_cap({ room = room })(tool_of({ content = string.rep("b", 4000) }))
        expect(tools.read.handler({}).ok).to.be(false)
    end)

    it("leaves the caller's map and every field beside the handler alone", function()
        local original = tool_of(string.rep("a", 4000))
        local handler = original.read.handler
        local capped = policy.result_cap({ room = room })(original)
        expect(original.read.handler).to.be(handler)
        expect(capped).to_not.be(original)
        expect(capped.read.description).to.be(original.read.description)
        expect(capped.read.input_schema).to.be(original.read.input_schema)
    end)
end)
