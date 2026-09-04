-- scope_spec.lua — mlua-lspec unit tests for the session-scope model of the
-- Lua kernel (knl.open + kernel-assigned turn numbering via ctx:append).
--
-- Run via:
--   test_launch(code_file=".../knl/spec/scope_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("knl") resolves
--
-- What this proves (scope-design.md rev 3 checklist, pure-VM half):
--   1 appending a model_response is counted by usage and gets a kernel turn
--     (the old usage=0 divergence, and the per-event scope stamp, are both
--     gone — the kernel numbers/charges/counts by kind).
--   2 turn numbering is kernel-owned: two successive responses are 1 then 2.
--   3 one model response with two tool_use blocks: both tool_call/tool_result
--     pairs share the response's kernel-assigned turn.
--
-- The Rust `knl` syscall bridge is not present in the pure lspec runner, so
-- a faithful Lua fake stands in below. It reproduces the facts the model
-- rests on, mirroring crates/agent-block-core/src/bridge/knl.rs:
--   * append overwrites the kernel-owned seq/epoch_ms and passes every other
--     field through untouched (there is no author field);
--   * appending a model_response assigns the turn (turns + 1), charges the
--     usage, and advances the counter — read back with s:turns();
--   * view("usage") counts every model_response in the session.
-- The e2e coverage against the *real* bridge lives in
-- crates/agent-block/tests/fixtures/knl_turn_test.lua.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Fake `knl` bridge (installed as a global BEFORE require("knl"), which is
-- what the module captures as its syscall layer at load time).
-- ─────────────────────────────────────────────────────────────────────────────

local uuid_counter = 0

local COUNTERS = { "input_tokens", "output_tokens", "thinking_tokens" }

local function charge_of(usage)
    if type(usage) ~= "table" then
        return 0
    end
    local total = 0
    for _, counter in ipairs(COUNTERS) do
        local n = usage[counter]
        if type(n) == "number" then
            total = total + n
        end
    end
    if total < 0 then
        total = 0
    end
    return total
end

local function fake_session(opts)
    opts = opts or {}
    local owner = opts.owner or "anon"
    local budget = opts.budget and opts.budget.tokens or nil
    uuid_counter = uuid_counter + 1
    local id = string.format("sess-%08d-0000-4000-8000-000000000000", uuid_counter)
    local events = {}
    local seq = 0
    local remaining = budget
    local closed = false
    local turns = 0

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

    -- Kernel-owned seq/epoch_ms overwrite any caller value; every other
    -- field passes through. There is no author.
    local function store(event)
        seq = seq + 1
        local rec = deep_copy(event)
        rec.seq = seq
        rec.epoch_ms = 1000 + seq
        events[#events + 1] = rec
        return seq
    end

    store({ kind = "run_started" })

    local s = {}

    function s:id()
        return id
    end

    function s:owner()
        return owner
    end

    -- The one write path. A model_response is numbered, charged and counted
    -- here (mirrors the Rust Session::append).
    function s:append(event)
        assert(not closed, "knl: append: session is closed")
        assert(type(event) == "table", "knl: append: event must be a table")
        assert(type(event.kind) == "string", "knl: append: kind is required")
        if event.kind == "model_response" then
            turns = turns + 1
            local rec = deep_copy(event)
            rec.turn = turns -- kernel-owned, overwrites any caller value
            local charged = charge_of(rec.usage)
            if remaining ~= nil then
                remaining = math.max(0, remaining - charged)
            end
            return store(rec)
        end
        return store(event)
    end

    function s:turns()
        return turns
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
            store({ kind = "run_finished", reason = reason or "closed" })
            closed = true
        end
    end

    -- Usage counts every model_response in the session (kind-keyed, no
    -- author) — the Rust view("usage") under the new model.
    function s:view(name)
        assert(name == "usage", "knl: view: unknown view")
        local u = { input_tokens = 0, output_tokens = 0, thinking_tokens = 0, model_calls = 0 }
        for _, e in ipairs(events) do
            if e.kind == "model_response" then
                u.model_calls = u.model_calls + 1
                if type(e.usage) == "table" then
                    for _, counter in ipairs(COUNTERS) do
                        local n = e.usage[counter]
                        if type(n) == "number" then
                            u[counter] = u[counter] + n
                        end
                    end
                end
            end
        end
        return u
    end

    return s
end

-- Global the module captures as `local syscall = knl` at load time. Both
-- names resolve to the same constructor, as the Rust bridge registers them.
knl = { open = fake_session, session = fake_session }

local kernel = require("knl")
local Outcome = kernel.Outcome

-- ─────────────────────────────────────────────────────────────────────────────
-- Backend / event helpers
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

-- Every model_response's turn number, in seq order.
local function response_turns(h)
    local turns = {}
    for _, ev in ipairs(h:events()) do
        if ev.kind == "model_response" then
            turns[#turns + 1] = ev.turn
        end
    end
    return turns
end

-- ─────────────────────────────────────────────────────────────────────────────
-- Tests
-- ─────────────────────────────────────────────────────────────────────────────

describe("knl turn (session-scope model)", function()
    it("counts a turn's model_response in usage and gives it a kernel turn", function()
        local h = kernel.open({ owner = "test", budget = { tokens = 100 } })
        expect(h:owner()).to.equal("test")
        h:append({ kind = "msg_user", content = "hi" })

        local o = kernel.turn({
            ctx = h,
            llm = stub(response("ok", { { type = "text", text = "hello" } }, {
                input_tokens = 10,
                output_tokens = 3,
            })),
        })
        expect(Outcome.is_ok(o)).to.equal(true)
        expect(o.out.turn).to.equal(1)

        local u = h:view("usage")
        expect(u.model_calls).to.equal(1)
        expect(u.input_tokens).to.equal(10)
        expect(u.output_tokens).to.equal(3)

        -- Charged: the append deducted the usage from the budget.
        expect(h:remaining()).to.equal(87)
        expect(h:turns()).to.equal(1)
    end)

    it("numbers successive turns 1 then 2 (kernel-owned)", function()
        local h = kernel.open({ budget = { tokens = 1000 } })

        kernel.turn({ ctx = h, llm = stub(response("ok")) })
        kernel.turn({ ctx = h, llm = stub(response("ok")) })

        local turns = response_turns(h)
        expect(#turns).to.equal(2)
        expect(turns[1]).to.equal(1)
        expect(turns[2]).to.equal(2)
    end)

    it("shares one turn number across a response's tool_call/tool_result pairs", function()
        local h = kernel.open({ budget = { tokens = 100 } })

        kernel.turn({
            ctx = h,
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
