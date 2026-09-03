-- scope_spec.lua — mlua-lspec unit tests for the scope-handle model of the
-- Lua kernel (knl.open / knl.usage + scope-keyed turn numbering).
--
-- Run via:
--   test_launch(code_file=".../knl/spec/scope_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("knl") resolves
--
-- What this proves (scope-design.md §6 POC checklist):
--   1 turn's model_response counts under its scope — the old usage=0
--     divergence is gone (and the author-keyed view still scores it 0,
--     which is the bug the scope model replaces).
--   2 turn numbering is scope-keyed: two successive responses are 1 then 2.
--   3 every event written through the handle carries the handle's scope.
--   4 a carried-over model_response (foreign scope) is excluded from usage
--     and is NOT re-stamped (lineage, §4).
--   5 one model response with two tool_use blocks: both tool_call/tool_result
--     pairs share the response's turn number.
--
-- The Rust `knl` syscall bridge is not present in the pure lspec runner, so
-- a faithful Lua fake stands in below. It reproduces the one fact the model
-- rests on: `append` overwrites the kernel-owned seq/epoch_ms/author and
-- passes every other field (scope included) through untouched — exactly
-- what crates/agent-block-core/src/bridge/knl.rs does. It also serves an
-- author-keyed view("usage"), the Rust view the scope projection replaces,
-- so the divergence and its fix can be shown side by side.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Fake `knl` bridge (installed as a global BEFORE require("knl"), which is
-- what the module captures as its syscall layer at load time).
-- ─────────────────────────────────────────────────────────────────────────────

local uuid_counter = 0

local function fake_session(opts)
    opts = opts or {}
    local budget = opts.budget and opts.budget.tokens or nil
    uuid_counter = uuid_counter + 1
    local id = string.format("scope-%08d-0000-4000-8000-000000000000", uuid_counter)
    local events = {}
    local seq = 0
    local remaining = budget
    local closed = false

    local function deep_copy(v)
        if type(v) ~= "table" then
            return v
        end
        local out = {}
        for k, val in pairs(v) do
            out[k] = deep_copy(val)
        end
        return out
    end

    -- Kernel-owned seq/epoch_ms/author overwrite any caller value; every
    -- other field (scope included) passes through — mirrors the Rust bridge.
    local function store(event, author)
        seq = seq + 1
        local rec = deep_copy(event)
        rec.seq = seq
        rec.epoch_ms = 1000 + seq
        rec.author = author
        events[#events + 1] = rec
        return seq
    end

    store({ kind = "run_started" }, "kernel")

    local s = {}

    function s:id()
        return id
    end

    -- The write path via the userdata method: author is "caller" (stamped
    -- from the path), scope and the rest pass through.
    function s:append(event)
        assert(not closed, "knl: append: session is closed")
        assert(type(event) == "table", "knl: append: event must be a table")
        assert(type(event.kind) == "string", "knl: append: kind is required")
        return store(event, "caller")
    end

    function s:events(from)
        from = from or 0
        local out = {}
        for _, e in ipairs(events) do
            if e.seq >= from then
                out[#out + 1] = deep_copy(e)
            end
        end
        return out
    end

    function s:spend(n)
        assert(not closed, "knl: spend: session is closed")
        assert(type(n) == "number" and n >= 0, "knl: spend: amount must be non-negative")
        if remaining == nil then
            return nil
        end
        remaining = math.max(0, remaining - n)
        return remaining
    end

    function s:remaining()
        return remaining
    end

    function s:exhausted()
        if remaining == nil then
            return false
        end
        return remaining <= 0
    end

    function s:close(reason)
        if not closed then
            store({ kind = "run_finished", reason = reason or "closed" }, "kernel")
            closed = true
        end
    end

    -- Author-keyed usage: the Rust view("usage") the scope model replaces.
    -- Counts only kernel-authored model_responses, so a handle-written
    -- response (author="caller") scores 0 here — the confused-deputy bug.
    function s:view(name)
        assert(name == "usage", "knl: view: unknown view")
        local u = { input_tokens = 0, output_tokens = 0, thinking_tokens = 0, model_calls = 0 }
        for _, e in ipairs(events) do
            if e.kind == "model_response" and e.author == "kernel" then
                u.model_calls = u.model_calls + 1
            end
        end
        return u
    end

    return s
end

-- Global the module captures as `local syscall = knl` at load time.
knl = { session = fake_session }

local kernel = require("knl")
local Outcome = kernel.Outcome

-- ─────────────────────────────────────────────────────────────────────────────
-- Backend / event helpers
-- ─────────────────────────────────────────────────────────────────────────────

-- A backend stub handing back queued responses (a queued function is called
-- with the request instead).
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

-- Every model_response's turn number, in seq order.
local function response_turns(h, scope)
    local turns = {}
    for _, ev in ipairs(h:events()) do
        if ev.kind == "model_response" and (scope == nil or ev.scope == scope) then
            turns[#turns + 1] = ev.turn
        end
    end
    return turns
end

-- ─────────────────────────────────────────────────────────────────────────────
-- Tests
-- ─────────────────────────────────────────────────────────────────────────────

describe("knl scope handle", function()
    it("counts turn's model_response under its own scope (the usage=0 divergence is gone)", function()
        local h = kernel.open({ budget = { tokens = 100 } })
        h:append({ kind = "msg_user", content = "hi" })

        local o = kernel.turn({
            ctx = h,
            backend = stub(response("ok", { { type = "text", text = "hello" } }, {
                input_tokens = 10,
                output_tokens = 3,
            })),
        })
        expect(Outcome.is_ok(o)).to.equal(true)

        local u = kernel.usage(h)
        expect(u.model_calls).to.equal(1)
        expect(u.input_tokens).to.equal(10)
        expect(u.output_tokens).to.equal(3)

        -- The divergence, made concrete: the author-keyed view (what the Rust
        -- view("usage") does) scores the very same response 0, because turn
        -- wrote it through the plain path and it reads back author="caller".
        -- Keying on scope is what makes the count right.
        expect(h.ctx:view("usage").model_calls).to.equal(0)
    end)

    it("numbers successive turns 1 then 2 (scope-keyed)", function()
        local h = kernel.open({ budget = { tokens = 1000 } })

        kernel.turn({ ctx = h, backend = stub(response("ok")) })
        kernel.turn({ ctx = h, backend = stub(response("ok")) })

        local turns = response_turns(h)
        expect(#turns).to.equal(2)
        expect(turns[1]).to.equal(1)
        expect(turns[2]).to.equal(2)
    end)

    it("stamps every event written through the handle with the handle's scope", function()
        local h = kernel.open({ budget = { tokens = 100 } })
        h:append({ kind = "msg_user", content = "hi" })

        kernel.turn({
            ctx = h,
            backend = stub(response("ok", { tool_use("c1", "echo", { v = 1 }) })),
            tools = {
                echo = {
                    handler = function()
                        return "r"
                    end,
                },
            },
        })

        -- run_started / run_finished are the kernel's own writes (not through
        -- the handle). Everything the handle wrote — msg_user, request,
        -- model_response, tool_call, tool_result — carries this scope.
        local kernel_written = { run_started = true, run_finished = true }
        local checked = 0
        for _, ev in ipairs(h:events()) do
            if not kernel_written[ev.kind] then
                expect(ev.scope).to.equal(h.scope)
                checked = checked + 1
            end
        end
        expect(checked).to.equal(5)
    end)

    it("excludes a carried-over model_response (foreign scope) and does not re-stamp it", function()
        local h = kernel.open({ budget = { tokens = 100 } })

        -- A carried-over response keeping its origin scope (lineage, §4).
        h:append({
            kind = "model_response",
            turn = 1,
            content = { { type = "text", text = "from before" } },
            usage = { input_tokens = 9000, output_tokens = 9000 },
            scope = "other-scope-id",
        })

        -- One real turn under this scope.
        kernel.turn({
            ctx = h,
            backend = stub(response("ok", nil, { input_tokens = 10, output_tokens = 3 })),
        })

        -- Only the real call counts; the carried 9000 tokens are not summed.
        local u = kernel.usage(h)
        expect(u.model_calls).to.equal(1)
        expect(u.input_tokens).to.equal(10)

        -- Lineage kept: the carried event still carries its origin scope,
        -- un-re-stamped.
        local carried
        for _, ev in ipairs(h:events()) do
            if ev.kind == "model_response" and ev.scope == "other-scope-id" then
                carried = ev
            end
        end
        expect(carried ~= nil).to.equal(true)
        expect(carried.scope).to.equal("other-scope-id")

        -- And this scope's own response is numbered 1: the foreign one did
        -- not advance the counter.
        local mine_turns = response_turns(h, h.scope)
        expect(#mine_turns).to.equal(1)
        expect(mine_turns[1]).to.equal(1)
    end)

    it("shares one turn number across a response's tool_call/tool_result pairs", function()
        local h = kernel.open({ budget = { tokens = 100 } })

        kernel.turn({
            ctx = h,
            backend = stub(response("ok", {
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

        local model_turn
        local call_turns, result_turns = {}, {}
        for _, ev in ipairs(h:events()) do
            if ev.kind == "model_response" then
                model_turn = ev.turn
            elseif ev.kind == "tool_call" then
                call_turns[#call_turns + 1] = ev.turn
            elseif ev.kind == "tool_result" then
                result_turns[#result_turns + 1] = ev.turn
            end
        end

        expect(model_turn).to.equal(1)
        expect(#call_turns).to.equal(2)
        expect(#result_turns).to.equal(2)
        expect(call_turns[1]).to.equal(1)
        expect(call_turns[2]).to.equal(1)
        expect(result_turns[1]).to.equal(1)
        expect(result_turns[2]).to.equal(1)
    end)
end)
