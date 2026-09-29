-- scope_spec.lua — mlua-lspec unit tests for the session-scope model of the
-- Lua kernel (knl.open + the beat ids the shell declares).
--
-- Run via:
--   test_launch(code_file=".../knl/spec/scope_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("knl") resolves
--
-- What this proves (the scope model and declared beat ids, pure-VM half):
--   1 a beat's llm_response lands with the counts the provider reported, and
--     the reading of them is a query view (`knl.views.usage`) rather than a
--     kernel built-in — while the balance moved only where the beat reserved
--     it, which is the other, unrelated reading.
--   2 beat ids are the shell's: two successive beats carry two distinct
--     opaque strings, and the session is never asked to number anything.
--   3 one model response with two tool_use blocks: both tool_call/tool_result
--     pairs carry the same declared id as the response.
--
-- The Rust `knl` syscall bridge is not present in the pure lspec runner, so
-- the shared stand-in (knl/spec/fake_bridge.lua, which says which bridge
-- facts it mirrors) is installed below. This file asks it for the kernel's
-- lifecycle writes (`session_opened` / `session_closed`) and a clock, so the
-- log a beat leaves reads the way the kernel's does around it; the ledger
-- writes stay off, because the balance is read here with `remaining()`.
-- The e2e coverage against the *real* bridge lives in
-- crates/agent-block/tests/fixtures/knl_beat_test.lua.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- The fake bridge, installed as the global `knl` BEFORE require("knl"), which
-- is what the module captures as its syscall layer at load time. The clock is
-- a counter: the kernel stamps `epoch_ms`, and nothing here reads the time.
local now_ms = 1000
require("knl.spec.fake_bridge").install({
    writes = { lifecycle = true },
    clock = function()
        now_ms = now_ms + 1
        return now_ms
    end,
})

-- The three counts a provider reports, each named by the usage view's statement.
local COUNTERS = { "input_tokens", "output_tokens", "thinking_tokens" }

local kernel = require("knl")
local Outcome = kernel.Outcome

-- ─────────────────────────────────────────────────────────────────────────────
-- llm / event helpers
-- ─────────────────────────────────────────────────────────────────────────────

local function stub(...)
    local queue = { ... }
    return function(req)
        local next_response = table.remove(queue, 1)
        assert(next_response ~= nil, "stub ran more often than the case queued")
        if type(next_response) == "function" then
            return next_response(req)
        end
        return next_response
    end
end

local function response(status, blocks, usage, stop_reason)
    return {
        status = status,
        content = blocks or { { type = "text", text = "ok" } },
        usage = usage or { input_tokens = 10, output_tokens = 3 },
        stop_reason = stop_reason,
    }
end

local function tool_use(id, name, input)
    return { type = "tool_use", id = id, name = name, input = input or {} }
end

-- The beat id an event was stamped with: a label in the envelope.
local function beat_of(ev)
    return ev.meta ~= nil and ev.meta.beat or nil
end

-- Every llm_response's declared beat id, in seq order.
local function response_beats(s)
    local ids = {}
    for _, ev in ipairs(s:events()) do
        if ev.kind == "llm_response" then
            ids[#ids + 1] = beat_of(ev)
        end
    end
    return ids
end

-- ─────────────────────────────────────────────────────────────────────────────
-- Tests
-- ─────────────────────────────────────────────────────────────────────────────

describe("knl beat (session-scope model)", function()
    it("records a beat's llm_response with its counts, and reads them as a query view", function()
        local s = kernel.open({
            owner = "test",
            budget = { amount = 100, tag = "beats" },
        })
        local d = kernel.device({
            llm = stub(response("ok", { { type = "text", text = "hello" } }, {
                input_tokens = 10,
                output_tokens = 3,
            })),
        })
        expect(s:owner()).to.equal("test")
        s:append({ kind = "msg_user", data = { content = "hi" } })

        local o = kernel.beat(s, d)
        expect(Outcome.is_ok(o)).to.equal(true)
        expect(type(o.out.beat)).to.equal("string")

        -- The counts the provider reported are on the response event, which
        -- is what the accounting reads.
        local recorded
        for _, ev in ipairs(s:events()) do
            if ev.kind == "llm_response" then
                recorded = ev
            end
        end
        expect(recorded.data.usage.input_tokens).to.equal(10)
        expect(recorded.data.usage.output_tokens).to.equal(3)

        -- And reading them is a query, not a kernel view: `knl.views.usage`
        -- runs one SELECT over the llm_response records, naming the streams
        -- with $sessions and summing each counter out of the payload. The
        -- rows themselves are a database's answer and are checked where
        -- there is one (knl_beat_test.lua inv11).
        expect(type(kernel.views.usage)).to.equal("function")
        kernel.views.usage(s)
        expect(#s._queries).to.equal(1)
        local sql = s._queries[1].sql
        expect(sql:match("^%s*SELECT") ~= nil).to.equal(true)
        expect(sql:find("$sessions", 1, true) ~= nil).to.equal(true)
        expect(sql:find("llm_response", 1, true) ~= nil).to.equal(true)
        for _, counter in ipairs(COUNTERS) do
            expect(sql:find("$.usage." .. counter, 1, true) ~= nil).to.equal(true)
        end

        -- Reserved, and only there: the beat took one unit before the call
        -- and the appends did not move the budget. The counts above are the
        -- other reading.
        expect(s:remaining()).to.equal(99)
        -- The kernel does not number beats: nothing to read back.
        expect(s.beats).to.equal(nil)
        expect(response_beats(s)[1]).to.equal(o.out.beat)
    end)

    it("gives two successive beats two distinct ids (shell-declared)", function()
        local s = kernel.open({ budget = { amount = 1000, tag = "beats" } })
        local d = kernel.device({ llm = stub(response("ok"), response("ok")) })

        kernel.beat(s, d)
        kernel.beat(s, d)

        local ids = response_beats(s)
        expect(#ids).to.equal(2)
        expect(type(ids[1])).to.equal("string")
        expect(type(ids[2])).to.equal("string")
        expect(ids[1] == ids[2]).to.equal(false)
    end)

    it("shares one beat id across a response's tool_call/tool_result pairs", function()
        local s = kernel.open({ budget = { amount = 100, tag = "beats" } })
        local d = kernel.device({
            llm = stub(response("ok", {
                tool_use("a", "noop", {}),
                tool_use("b", "noop", {}),
            })),
            tools = {
                noop = {
                    handler = function()
                        return "r"
                    end,
                },
            },
        })

        local o = kernel.beat(s, d)
        expect(Outcome.is_ok(o)).to.equal(true)

        local model_beat
        local call_beats, result_beats = {}, {}
        for _, ev in ipairs(s:events()) do
            if ev.kind == "llm_response" then
                model_beat = beat_of(ev)
            elseif ev.kind == "tool_call" then
                call_beats[#call_beats + 1] = beat_of(ev)
            elseif ev.kind == "tool_result" then
                result_beats[#result_beats + 1] = beat_of(ev)
            end
        end

        expect(model_beat).to.equal(o.out.beat)
        expect(#call_beats).to.equal(2)
        expect(#result_beats).to.equal(2)
        expect(call_beats[1]).to.equal(model_beat)
        expect(call_beats[2]).to.equal(model_beat)
        expect(result_beats[1]).to.equal(model_beat)
        expect(result_beats[2]).to.equal(model_beat)
    end)
end)
