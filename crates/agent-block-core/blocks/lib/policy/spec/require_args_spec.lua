-- require_args_spec.lua — mlua-lspec unit tests for `policy.require_args`,
-- the wrapper that refuses a call whose required arguments did not all
-- arrive.
--
-- Run via:
--   just test-lua require_args
--
-- What this proves:
--   1 a call missing a required argument is refused as an answer, not a
--     raise, and the refusal names the argument and says why it is missing;
--   2 the inner handler is not reached at all in that case — the refusal is
--     this wrapper's, before the tool runs;
--   3 a call with every required argument passes through and the inner
--     result is what comes back, unchanged;
--   4 an entry whose schema declares no `required` is handed back as it is;
--   5 the map handed back is a new one, the caller's is untouched, and every
--     field beside the handler is preserved;
--   6 the bounds are loud: an option it does not know, a tool without a
--     handler.

local describe, it, expect = lust.describe, lust.it, lust.expect

local policy = require("policy")

--- A tools map with one entry: `required` are the argument names its schema
--- declares, and the handler records what it was called with.
local function tools_of(required)
    local seen = {}
    return {
        edit = {
            description = "edit something",
            input_schema = {
                type = "object",
                properties = { path = { type = "string" }, content = { type = "string" } },
                required = required,
            },
            handler = function(args)
                seen[#seen + 1] = args
                return { ok = true, applied = 1 }
            end,
        },
    },
        seen
end

describe("policy.require_args — construction", function()
    it("answers a bind, which takes a tools map", function()
        local bind = policy.require_args()
        expect(type(bind)).to.be("function")
        expect(type(bind(tools_of({ "path" })))).to.be("table")
    end)

    it("refuses an option it does not know — it has none", function()
        expect(function()
            policy.require_args({ max = 2 })
        end).to.fail()
    end)

    it("refuses a tools map with a handler-less entry", function()
        local bind = policy.require_args()
        expect(function()
            bind({ edit = { description = "no handler" } })
        end).to.fail()
    end)
end)

describe("policy.require_args — the check", function()
    it("refuses a call missing a required argument, naming it, without running the tool", function()
        local raw, seen = tools_of({ "path", "content" })
        local tools = policy.require_args()(raw)

        local res = tools.edit.handler({ path = "/work/lib.rs" })

        expect(res.ok).to.be(false)
        expect(res.reason).to.be("argument_missing")
        expect(res.missing).to.be("content")
        -- The refusal says why a whole-looking call can arrive without them.
        expect(res.error:find("content never arrived", 1, true) ~= nil).to.be(true)
        expect(res.error:find("runs out of room", 1, true) ~= nil).to.be(true)
        expect(res.error:find("smaller call", 1, true) ~= nil).to.be(true)
        -- Nothing reached the tool.
        expect(#seen).to.be(0)
    end)

    it("names the first argument that is missing when several are", function()
        local tools = policy.require_args()(tools_of({ "path", "content" }))
        expect(tools.edit.handler({}).missing).to.be("path")
    end)

    it("passes a whole call through and answers what the tool answered", function()
        local raw, seen = tools_of({ "path", "content" })
        local tools = policy.require_args()(raw)

        local res = tools.edit.handler({ path = "/work/lib.rs", content = "x" })

        expect(res.ok).to.be(true)
        expect(res.applied).to.be(1)
        expect(#seen).to.be(1)
        expect(seen[1].path).to.be("/work/lib.rs")
    end)

    it("leaves an entry whose schema declares no required list exactly as it is", function()
        local raw = {
            ask = {
                description = "no required arguments",
                input_schema = { type = "object" },
                handler = function()
                    return "answered"
                end,
            },
        }
        local tools = policy.require_args()(raw)
        -- The same entry, not a copy: there is nothing to check, so there is
        -- nothing between the call and the tool.
        expect(tools.ask).to.be(raw.ask)
        expect(tools.ask.handler({})).to.be("answered")
    end)

    it("leaves the caller's map and every field beside the handler alone", function()
        local raw = tools_of({ "path" })
        local handler = raw.edit.handler
        local checked = policy.require_args()(raw)

        expect(raw.edit.handler).to.be(handler)
        expect(checked).to_not.be(raw)
        expect(checked.edit.description).to.be(raw.edit.description)
        expect(checked.edit.input_schema).to.be(raw.edit.input_schema)
    end)
end)
