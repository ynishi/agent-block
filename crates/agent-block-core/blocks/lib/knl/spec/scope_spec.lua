-- scope_spec.lua — mlua-lspec unit tests for the session-scope model of the
-- Lua kernel (knl.open + kernel-assigned beat numbering via ctx:append).
--
-- Run via:
--   test_launch(code_file=".../knl/spec/scope_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("knl") resolves
--
-- What this proves (scope-design.md rev 3 checklist, pure-VM half):
--   1 appending a model_response is counted by usage and gets a kernel beat
--     (the old usage=0 divergence, and the per-event scope stamp, are both
--     gone — the kernel numbers/counts by kind), while the balance moved
--     only where the beat reserved it.
--   2 beat numbering is kernel-owned: two successive responses are 1 then 2.
--   3 one model response with two tool_use blocks: both tool_call/tool_result
--     pairs share the response's kernel-assigned beat.
--
-- The Rust `knl` syscall bridge is not present in the pure lspec runner, so
-- a faithful Lua fake stands in below. It reproduces the facts the model
-- rests on, mirroring crates/agent-block-core/src/bridge/knl.rs:
--   * append overwrites the kernel-owned seq/epoch_ms and passes every other
--     field through untouched (there is no author field);
--   * appending a model_response assigns the beat (beats + 1) and advances
--     the counter — read back with s:beats() — and moves no balance;
--   * reserve is the only decision point: it deducts, or refuses with the
--     grant's tag and leaves the balance alone;
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

local function fake_session(opts)
    opts = opts or {}
    local owner = opts.owner or "anon"
    local grant = opts.budget or {}
    uuid_counter = uuid_counter + 1
    local id = string.format("sess-%08d-0000-4000-8000-000000000000", uuid_counter)
    local events = {}
    local seq = 0
    local remaining = grant.amount
    local tag = grant.tag
    local closed = false
    local beats = 0

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

    -- The one write path. A model_response is numbered and counted here,
    -- and nothing is charged (mirrors the Rust Session::append).
    function s:append(event)
        assert(not closed, "knl: append: session is closed")
        assert(type(event) == "table", "knl: append: event must be a table")
        assert(type(event.kind) == "string", "knl: append: kind is required")
        if event.kind == "model_response" then
            beats = beats + 1
            local rec = deep_copy(event)
            rec.beat = beats -- kernel-owned, overwrites any caller value
            return store(rec)
        end
        return store(event)
    end

    function s:beats()
        return beats
    end

    -- The decision point: allow and deduct, or refuse (naming the grant)
    -- and leave the balance exactly where it was.
    function s:reserve(n)
        assert(not closed, "knl: reserve: session is closed")
        assert(type(n) == "number" and n >= 0, "knl: reserve: amount must be non-negative")
        if remaining == nil then
            return true
        end
        if remaining < n then
            return false, tag
        end
        remaining = remaining - n
        return true
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

-- Every model_response's beat number, in seq order.
local function response_beats(h)
    local numbers = {}
    for _, ev in ipairs(h:events()) do
        if ev.kind == "model_response" then
            numbers[#numbers + 1] = ev.beat
        end
    end
    return numbers
end

-- ─────────────────────────────────────────────────────────────────────────────
-- Tests
-- ─────────────────────────────────────────────────────────────────────────────

describe("knl beat (session-scope model)", function()
    it("counts a beat's model_response in usage and gives it a kernel number", function()
        local h = kernel.open({
            owner = "test",
            budget = { amount = 100, tag = "tokens" },
            llm = stub(response("ok", { { type = "text", text = "hello" } }, {
                input_tokens = 10,
                output_tokens = 3,
            })),
        })
        expect(h:owner()).to.equal("test")
        h:append({ kind = "msg_user", content = "hi" })

        local o = kernel.beat(h)
        expect(Outcome.is_ok(o)).to.equal(true)
        expect(o.out.beat).to.equal(1)

        local u = h:view("usage")
        expect(u.model_calls).to.equal(1)
        expect(u.input_tokens).to.equal(10)
        expect(u.output_tokens).to.equal(3)

        -- Reserved, not charged: the beat took one unit before the call and
        -- the appends moved nothing. The usage above is the other reading.
        expect(h:remaining()).to.equal(99)
        expect(h:beats()).to.equal(1)
    end)

    it("numbers successive beats 1 then 2 (kernel-owned)", function()
        local h = kernel.open({
            budget = { amount = 1000, tag = "tokens" },
            llm = stub(response("ok"), response("ok")),
        })

        kernel.beat(h)
        kernel.beat(h)

        local numbers = response_beats(h)
        expect(#numbers).to.equal(2)
        expect(numbers[1]).to.equal(1)
        expect(numbers[2]).to.equal(2)
    end)

    it("shares one beat number across a response's tool_call/tool_result pairs", function()
        local h = kernel.open({
            budget = { amount = 100, tag = "tokens" },
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

        kernel.beat(h)

        local model_beat
        local call_beats, result_beats = {}, {}
        for _, ev in ipairs(h:events()) do
            if ev.kind == "model_response" then
                model_beat = ev.beat
            elseif ev.kind == "tool_call" then
                call_beats[#call_beats + 1] = ev.beat
            elseif ev.kind == "tool_result" then
                result_beats[#result_beats + 1] = ev.beat
            end
        end

        expect(model_beat).to.equal(1)
        expect(#call_beats).to.equal(2)
        expect(#result_beats).to.equal(2)
        expect(call_beats[1]).to.equal(1)
        expect(call_beats[2]).to.equal(1)
        expect(result_beats[1]).to.equal(1)
        expect(result_beats[2]).to.equal(1)
    end)
end)
