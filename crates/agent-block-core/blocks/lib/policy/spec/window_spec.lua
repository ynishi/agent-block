-- window_spec.lua — mlua-lspec unit tests for `policy.window`, the fold that
-- keeps the last n beats.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/window_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- What this proves:
--   1 the request is the last `tail` beats and nothing earlier — the beats
--     before the window are gone, and so is anything unstamped that preceded
--     them (the caller's first seed among it);
--   2 A BEAT IS NEVER CUT IN HALF. The slice starts at the first event of a
--     beat, so an assistant message and the tool results answering it stay
--     together and `knl.fold`'s crash repair (a synthetic is_error result for
--     an unanswered tool_use) never fires on a windowed history;
--   3 the folding itself is the kernel's: the windowed request is exactly
--     `knl.fold` over the slice, not a second message assembly;
--   4 `system` and `tools` are untouched — they come off the device on every
--     fold and were never in the log;
--   5 a log with `tail` beats or fewer is not cut at all;
--   6 the bounds are loud in prod as well as dev, and a session in the opts
--     is refused as the typo it is.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")

-- ─────────────────────────────────────────────────────────────────────────────
-- Hand-built histories. The fold is pure, so a case that is about WHICH events
-- reach it says so by handing it exactly those events.
-- ─────────────────────────────────────────────────────────────────────────────

--- One beat that answered with text.
local function answered(id, body, seq)
    return {
        { kind = "llm_request", beat = id, data = { request = { messages = {} } }, seq = seq },
        {
            kind = "llm_response",
            beat = id,
            data = { content = { { type = "text", text = body } }, usage = {} },
            seq = seq + 1,
        },
    }
end

--- One beat that called a tool and recorded the pair.
local function called(id, call_id, seq)
    return {
        { kind = "llm_request", beat = id, data = { request = { messages = {} } }, seq = seq },
        {
            kind = "llm_response",
            beat = id,
            data = {
                content = { { type = "tool_use", id = call_id, name = "t", input = {} } },
                usage = {},
            },
            seq = seq + 1,
        },
        {
            kind = "tool_call",
            beat = id,
            data = { call_id = call_id, name = "t", args = {} },
            seq = seq + 2,
        },
        {
            kind = "tool_result",
            beat = id,
            data = { call_id = call_id, ok = true, result = "R-" .. call_id },
            seq = seq + 3,
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

--- A seed the caller wrote: no beat, so it is log and not beat.
local function seed(text, seq)
    return { { kind = "msg_user", data = { content = text }, seq = seq } }
end

--- Every text a request's messages carry, as one searchable string.
local function rendered(request)
    local parts = {}
    for _, message in ipairs(request.messages) do
        local content = message.content
        if type(content) == "string" then
            parts[#parts + 1] = content
        else
            for _, block in ipairs(content or {}) do
                parts[#parts + 1] = tostring(block.text or block.content or block.type)
            end
        end
    end
    return table.concat(parts, "|")
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.window — construction", function()
    it("answers a fold, which is what a device takes", function()
        local fold = policy.window({ tail = 2 })
        expect(type(fold)).to.be("function")
        local d = kernel.device({ llm = support.always(support.text("x")), fold = fold })
        expect(d.fold).to.be(fold)
    end)

    it("insists on a whole number of beats, in prod as well as dev", function()
        for _, bad in ipairs({ 0, -1, 1.5 }) do
            expect(function()
                policy.window({ tail = bad })
            end).to.fail()
        end
        expect(function()
            policy.window({})
        end).to.fail()
        expect(function()
            policy.window()
        end).to.fail()
    end)

    it("refuses an option it does not know, a session most of all", function()
        expect(function()
            policy.window({ tail = 2, tial = 3 })
        end).to.fail()
        local ok, err = pcall(policy.window, { tail = 2, session = support.session() })
        expect(ok).to.be(false)
        expect(tostring(err):find("an argument", 1, true) ~= nil).to.be(true)
    end)
end)

describe("policy.window — the slice", function()
    it("keeps the last `tail` beats and drops what came before", function()
        local events =
            concat(seed("first", 1), answered("b1", "one", 2), answered("b2", "two", 4), answered("b3", "three", 6))
        local request = policy.window({ tail = 2 })(events, {})
        local text = rendered(request)
        expect(text:find("two", 1, true) ~= nil).to.be(true)
        expect(text:find("three", 1, true) ~= nil).to.be(true)
        expect(text:find("one", 1, true)).to.be(nil)
        -- and the seed that preceded the window went with it: a request that
        -- kept the opening line and skipped the middle would say something
        -- the log does not.
        expect(text:find("first", 1, true)).to.be(nil)
    end)

    it("folds the slice exactly as the kernel folds it (one implementation)", function()
        local events = concat(seed("first", 1), answered("b1", "one", 2), answered("b2", "two", 4))
        local windowed = policy.window({ tail = 1 })(events, {})
        -- The same events, folded by the kernel directly: the window's only
        -- job is choosing them.
        local direct = kernel.fold({ events[4], events[5] }, {})
        expect(#windowed.messages).to.be(#direct.messages)
        expect(rendered(windowed)).to.be(rendered(direct))
    end)

    it("never cuts a beat in half — the pair stays with the response", function()
        -- Three tool-calling beats. A slice by event COUNT would land inside
        -- one of them and leave a tool_result with no assistant message to
        -- answer, or an assistant whose tool_use nothing answers — the state
        -- knl.fold repairs with a synthetic is_error result. Slicing by beat
        -- cannot produce either.
        local events = concat(called("b1", "c1", 1), called("b2", "c2", 5), called("b3", "c3", 9))
        local request = policy.window({ tail = 1 })(events, {})

        expect(#request.messages).to.be(2)
        expect(request.messages[1].role).to.be("assistant")
        expect(request.messages[1].content[1].type).to.be("tool_use")
        expect(request.messages[1].content[1].id).to.be("c3")
        expect(request.messages[2].role).to.be("user")
        expect(#request.messages[2].content).to.be(1)
        expect(request.messages[2].content[1].tool_use_id).to.be("c3")
        -- no repair fired: nothing was interrupted, because nothing was split
        expect(request.messages[2].content[1].is_error).to.be(nil)
    end)

    it("keeps every beat when the log holds `tail` of them or fewer", function()
        local events = concat(seed("first", 1), answered("b1", "one", 2), answered("b2", "two", 4))
        local windowed = policy.window({ tail = 5 })(events, {})
        local whole = kernel.fold(events, {})
        expect(#windowed.messages).to.be(#whole.messages)
        expect(rendered(windowed)).to.be(rendered(whole))
        expect(rendered(windowed):find("first", 1, true) ~= nil).to.be(true)
    end)

    it("folds a log with no beats yet — the seed alone", function()
        local request = policy.window({ tail = 2 })(seed("hello", 1), {})
        expect(#request.messages).to.be(1)
        expect(request.messages[1].content).to.be("hello")
    end)

    it("folds an empty log into an empty, still array-tagged, message list", function()
        local request = policy.window({ tail = 2 })({}, {})
        expect(#request.messages).to.be(0)
        expect(getmetatable(request.messages).__jsontype).to.be("array")
    end)

    it("leaves system and tools alone (they come off the device, not the log)", function()
        local device = kernel.device({ system = "SYS", tools = support.tool("echo", "ok") })
        local events = concat(answered("b1", "one", 1), answered("b2", "two", 3))
        local request = policy.window({ tail = 1 })(events, device)
        expect(request.system).to.be("SYS")
        expect(#request.tools).to.be(1)
        expect(request.tools[1].name).to.be("echo")
    end)
end)

describe("policy.window — driving real beats", function()
    it("bounds what the third beat sends, and the record says so", function()
        local session = support.session()
        support.seed(session, "q")
        local device = kernel.device({
            llm = support.queue(support.text("one"), support.text("two"), support.text("three")),
            fold = policy.window({ tail = 1 }),
        })

        for _ = 1, 3 do
            expect(kernel.Outcome.is_ok(kernel.beat(session, device))).to.be(true)
        end

        -- The request the third beat actually sent is a fact in the log.
        local sent = {}
        for _, ev in ipairs(session:events()) do
            if ev.kind == "llm_request" then
                sent[#sent + 1] = ev.data.request
            end
        end
        expect(#sent).to.be(3)
        -- The first beat saw only the seed; the third saw only the beat
        -- before it, seed included in neither's window but the first.
        expect(rendered(sent[1])).to.be("q")
        expect(rendered(sent[3])).to.be("two")
    end)
end)
