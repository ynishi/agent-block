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
-- a faithful Lua fake stands in below. It reproduces the facts the model
-- rests on, mirroring crates/agent-block-core/src/bridge/knl.rs:
--   * append overwrites the kernel-owned seq/epoch_ms and passes every other
--     field through untouched — `beat` included (there is no author field,
--     and no beat numbering: the kernel stores the id it is given);
--   * reserve is the only decision point: it deducts, or refuses with the
--     grant's tag and leaves the balance alone;
--   * query records the statement it was given and answers whatever the case
--     queued (no SQLite stands behind it — what a statement SELECTS is asked
--     in crates/agent-block/tests/fixtures/knl_beat_test.lua);
--   * new_beat_id mints a fresh, time-ordered id per call.
-- The e2e coverage against the *real* bridge lives in
-- crates/agent-block/tests/fixtures/knl_beat_test.lua.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Fake `knl` bridge (installed as a global BEFORE require("knl"), which is
-- what the module captures as its syscall layer at load time).
-- ─────────────────────────────────────────────────────────────────────────────

local uuid_counter = 0
local minted = 0

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

    -- The kernel's own kinds go in the same envelope as everybody's: what
    -- the kind is about lives under `data`.
    store({ kind = "session_opened", data = { scope_id = "scope-" .. id, owner = owner } })

    local s = {}

    -- Identity: the three readings the kernel answers. The fake carries the
    -- whole session surface because `knl.beat` asks for the whole surface
    -- before it treats a value as a session.
    function s:id()
        return id
    end

    function s:scope_id()
        return "scope-" .. id
    end

    function s:owner()
        return owner
    end

    -- The one write path. Nothing is numbered here and the budget does not
    -- move (mirrors the Rust Session::append): a `beat` the caller declared
    -- is stored exactly as given, and the kernel only insists it be a string.
    function s:append(event)
        assert(not closed, "knl: append: session is closed")
        assert(type(event) == "table", "knl: append: event must be a table")
        assert(type(event.kind) == "string", "knl: append: kind is required")
        assert(event.beat == nil or type(event.beat) == "string", "knl: append: beat must be a string")
        return store(event)
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

    -- The write IS the result: spend answers nothing, and a caller that
    -- wants the balance reads it with `remaining()` (the kernel's surface,
    -- mirrored here so the fake cannot promise more than the bridge does).
    function s:spend(n)
        assert(not closed, "knl: spend: session is closed")
        assert(type(n) == "number" and n >= 0, "knl: spend: amount must be non-negative")
        if remaining == nil then
            return
        end
        remaining = math.max(0, remaining - n)
    end

    function s:len()
        return #events
    end

    -- The one named fold. Nothing here folds anything — what `tail` answers
    -- is the kernel's — and the method is carried because the surface has it:
    -- a stand-in missing one is not a session.
    function s:view(_name, _opts)
        error("knl: view: validation: unknown view")
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
            store({ kind = "session_closed", data = { reason = reason or "closed" } })
            closed = true
        end
    end

    -- The SQL read, which is how token usage is read
    -- now: a view is a named function that runs one SELECT, and this fake
    -- records the statement rather than running one. What a statement
    -- selects is a question only a database can answer, and it is asked
    -- where there is one (knl_beat_test.lua inv11).
    s._queries = {}
    s._query_rows = {}

    function s:query(sql, params, opts)
        assert(not closed, "knl: query: session is closed")
        assert(type(sql) == "string", "knl: query: sql must be a string")
        self._queries[#self._queries + 1] = { sql = sql, params = params, opts = opts }
        return self._query_rows, false
    end

    return s
end

-- Global the module captures as `local syscall = knl` at load time. The
-- bridge publishes `open` / `resume` / `new_beat_id` / `error` / `api` and
-- no `session`: the bracket is the Lua module's, built on `open`.
knl = {
    open = fake_session,
    new_beat_id = function()
        minted = minted + 1
        return string.format("beat-%06d", minted)
    end,
}

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

-- Every llm_response's declared beat id, in seq order.
local function response_beats(s)
    local ids = {}
    for _, ev in ipairs(s:events()) do
        if ev.kind == "llm_response" then
            ids[#ids + 1] = ev.beat
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
                model_beat = ev.beat
            elseif ev.kind == "tool_call" then
                call_beats[#call_beats + 1] = ev.beat
            elseif ev.kind == "tool_result" then
                result_beats[#result_beats + 1] = ev.beat
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
