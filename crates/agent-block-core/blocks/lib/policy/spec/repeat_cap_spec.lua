-- repeat_cap_spec.lua — mlua-lspec unit tests for `policy.repeat_cap`, the
-- wrapper that refuses the same tool call past a count when nothing has
-- changed in between.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/repeat_cap_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- What this proves:
--   1 a call within the count passes through to the handler untouched, and
--     the count includes the call being answered (the kernel records the
--     `tool_call` first), so `max = 2` answers twice and refuses the third;
--   2 the refusal is a return value naming the tool, the times and the
--     limit — not a raise — so the kernel records it and the model reads it;
--   3 the count is read off the LOG: different arguments are different
--     calls, the same arguments in a different key order are the same call,
--     and a fresh binder over the same log refuses alike;
--   4 a successful result of a tool in `resets` starts the count over for
--     every call; a reset tool that refused (`result.ok == false`) does not;
--   5 the map handed back is a new one and the caller's is untouched;
--   6 the bounds are loud: a bad max, a bad resets entry, an unknown
--     option, a tool without a handler, a binder given no session.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- `call_key` encodes table-valued arguments with `std.json.encode`; the pure
-- spec environment may not have `std`, so a small encoder stands in.
if rawget(_G, "std") == nil then
    local function encode(value)
        local t = type(value)
        if t == "string" then
            return '"' .. value .. '"'
        elseif t ~= "table" then
            return tostring(value)
        end
        local keys = {}
        for k in pairs(value) do
            keys[#keys + 1] = tostring(k)
        end
        table.sort(keys)
        local parts = {}
        for _, k in ipairs(keys) do
            parts[#parts + 1] = k .. ":" .. encode(value[k])
        end
        return "{" .. table.concat(parts, ",") .. "}"
    end
    _G.std = { json = { encode = encode } }
end

local support = require("policy.spec.support")
local policy = require("policy")

--- A session with `n` recorded calls of `name` with `args` (the kernel's
--- shape: `tool_call` then `tool_result`, one pair per call).
local function called(session, name, args, n, result)
    for i = 1, n do
        local id = name .. "-" .. tostring(#session:events() + i)
        session:append({ kind = "tool_call", beat = "b", data = { call_id = id, name = name, args = args } })
        session:append({
            kind = "tool_result",
            beat = "b",
            data = { call_id = id, ok = true, result = result or { ok = true } },
        })
    end
end

--- Tools whose handler reports it was reached.
local function tools_seen()
    local hits = {}
    local tools = {
        read = {
            description = "read a range",
            handler = function(args)
                hits[#hits + 1] = "read:" .. tostring(args.start_line)
                return { ok = true, content = "..." }
            end,
        },
        edit = {
            description = "edit",
            handler = function()
                hits[#hits + 1] = "edit"
                return { ok = true }
            end,
        },
    }
    return tools, hits
end

--- The kernel's order, in miniature: record the call, then run the handler.
local function invoke(session, tools, name, args)
    local id = name .. "-" .. tostring(#session:events() + 1)
    session:append({ kind = "tool_call", beat = "b", data = { call_id = id, name = name, args = args } })
    local result = tools[name].handler(args)
    session:append({ kind = "tool_result", beat = "b", data = { call_id = id, ok = true, result = result } })
    return result
end

describe("policy.repeat_cap — construction", function()
    it("answers a binder, and the binder a wrapper", function()
        local bind = policy.repeat_cap({ max = 2, resets = { "edit" } })
        expect(type(bind)).to.be("function")
        local wrap = bind(support.session())
        expect(type(wrap)).to.be("function")
        expect(type(policy.repeat_cap()(support.session()))).to.be("function")
    end)

    it("refuses a bad max, a bad resets entry, an unknown option, and no session", function()
        expect(function()
            policy.repeat_cap({ max = 0 })
        end).to.fail()
        expect(function()
            policy.repeat_cap({ max = 1.5 })
        end).to.fail()
        expect(function()
            policy.repeat_cap({ resets = { "" } })
        end).to.fail()
        expect(function()
            policy.repeat_cap({ maxx = 2 })
        end).to.fail()
        expect(function()
            policy.repeat_cap({ session = support.session() })
        end).to.fail()
        expect(function()
            policy.repeat_cap()(nil)
        end).to.fail()
    end)

    it("refuses a tool without a handler, and hands back a new map", function()
        local wrap = policy.repeat_cap()(support.session())
        expect(function()
            wrap({ bad = { description = "no handler" } })
        end).to.fail()
        local tools = tools_seen()
        local capped = wrap(tools)
        expect(capped ~= tools).to.be(true)
        expect(capped.read ~= tools.read).to.be(true)
        expect(capped.read.description).to.be("read a range")
        expect(type(tools.read.handler)).to.be("function")
    end)
end)

describe("policy.repeat_cap — the count", function()
    it("answers max times and refuses the next, with a return value that says so", function()
        local session = support.session()
        local tools, hits = tools_seen()
        local capped = policy.repeat_cap({ max = 2 })(session)(tools)
        local args = { path = "a.rs", start_line = 10, end_line = 40 }
        expect(invoke(session, capped, "read", args).ok).to.be(true)
        expect(invoke(session, capped, "read", args).ok).to.be(true)
        local third = invoke(session, capped, "read", args)
        expect(third.ok).to.be(false)
        expect(third.reason).to.be("repeated")
        expect(third.times).to.be(3)
        expect(third.max).to.be(2)
        expect(third.error:find("'read'", 1, true) ~= nil).to.be(true)
        expect(third.error:find("2 times", 1, true) ~= nil).to.be(true)
        expect(#hits).to.be(2)
    end)

    it("counts different arguments as different calls, and key order as the same call", function()
        local session = support.session()
        local tools, hits = tools_seen()
        local capped = policy.repeat_cap({ max = 1 })(session)(tools)
        expect(invoke(session, capped, "read", { path = "a.rs", start_line = 1, end_line = 5 }).ok).to.be(true)
        expect(invoke(session, capped, "read", { path = "a.rs", start_line = 6, end_line = 9 }).ok).to.be(true)
        expect(invoke(session, capped, "read", { end_line = 5, start_line = 1, path = "a.rs" }).ok).to.be(false)
        expect(#hits).to.be(2)
    end)

    it("reads the log, so a fresh binder over the same log refuses alike", function()
        local session = support.session()
        called(session, "read", { path = "a.rs" }, 2)
        local tools, hits = tools_seen()
        local capped = policy.repeat_cap({ max = 2 })(session)(tools)
        expect(invoke(session, capped, "read", { path = "a.rs" }).ok).to.be(false)
        expect(#hits).to.be(0)
    end)

    it("starts over after a successful reset tool, and not after one that refused", function()
        local session = support.session()
        local tools, hits = tools_seen()
        local capped = policy.repeat_cap({ max = 1, resets = { "edit" } })(session)(tools)
        local args = { path = "a.rs", start_line = 1, end_line = 5 }
        expect(invoke(session, capped, "read", args).ok).to.be(true)
        expect(invoke(session, capped, "read", args).ok).to.be(false)
        -- The edit that landed: the file changed, the read is new again.
        expect(invoke(session, capped, "edit", { path = "a.rs" }).ok).to.be(true)
        expect(invoke(session, capped, "read", args).ok).to.be(true)
        expect(invoke(session, capped, "read", args).ok).to.be(false)
        -- An edit the tool refused changed nothing: no reset.
        called(session, "edit", { path = "a.rs", bad = true }, 1, { ok = false, reason = "search_not_found" })
        expect(invoke(session, capped, "read", args).ok).to.be(false)
        expect(#hits).to.be(3)
    end)

    it("leaves calls of a tool not in resets counting on", function()
        local session = support.session()
        local tools = tools_seen()
        local capped = policy.repeat_cap({ max = 1, resets = { "edit" } })(session)(tools)
        expect(invoke(session, capped, "read", { path = "a.rs" }).ok).to.be(true)
        called(session, "other", { x = 1 }, 1)
        expect(invoke(session, capped, "read", { path = "a.rs" }).ok).to.be(false)
    end)
end)
