-- ctx_spec.lua — mlua-lspec unit tests for the ctx / beat POC
-- (core-loop-design.md: immutable composable ctx, single-argument beat).
--
-- Run via:
--   test_launch(code_file=".../knl/spec/ctx_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("knl") resolves
--
-- What this proves (core-loop-design.md checklist, pure-VM half):
--   1 open splits state opts from config, freezes the config onto the ctx,
--     and rejects unknown options loudly (no silent policy typos).
--   2 the ctx is immutable: assignment raises; config reads are direct
--     (memory-map: ctx.llm); state methods delegate to the kernel handle.
--   3 with derives: new ctx, same session, config delta applied; the
--     original is untouched; owner/store/session in the delta raise.
--   4 beat(ctx) is the whole primitive: gate (llm / budget), fold, record,
--     call, record + number + charge via append, tools stamped with the
--     kernel-assigned beat number.
--   5 escalation shape: knl.beat(ctx:with{ llm = strong }) uses the strong
--     llm for that beat only.
--
-- The Rust `knl` syscall bridge is not present in the pure lspec runner, so
-- a faithful Lua fake stands in below (mirroring bridge/knl.rs facts: append
-- stamps kernel-owned seq; appending a model_response assigns the number,
-- charges the usage; events/turns/exhausted read back).

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Fake `knl` bridge (installed as a global BEFORE require("knl"), which is
-- what the module captures as its syscall layer at load time).
-- ─────────────────────────────────────────────────────────────────────────────

local function charge_of(usage)
    if type(usage) ~= "table" then
        return 0
    end
    local total = 0
    for _, k in ipairs({ "input_tokens", "output_tokens", "thinking_tokens" }) do
        if type(usage[k]) == "number" then
            total = total + usage[k]
        end
    end
    return total
end

local function fake_session(opts)
    opts = opts or {}
    local s = {
        _events = {},
        _seq = 0,
        _turns = 0,
        _spent = 0,
        _budget = opts.budget and opts.budget.tokens or nil,
        owner = opts.owner or "anon",
        closed = false,
    }
    function s:append(ev)
        self._seq = self._seq + 1
        ev.seq = self._seq
        if ev.kind == "model_response" then
            self._turns = self._turns + 1
            self._spent = self._spent + charge_of(ev.usage)
        end
        self._events[#self._events + 1] = ev
        return ev.seq
    end
    function s:events()
        return self._events
    end
    function s:turns()
        return self._turns
    end
    function s:exhausted()
        return self._budget ~= nil and self._spent >= self._budget
    end
    function s:remaining()
        if self._budget == nil then
            return nil
        end
        return self._budget - self._spent
    end
    function s:close(_reason)
        self.closed = true
    end
    return s
end

knl = {
    open = function(o)
        return fake_session(o)
    end,
    resume = function(o)
        local s = fake_session(o)
        s.resumed_from = o and o.session
        return s
    end,
}

local K = require("knl")
local Outcome = K.Outcome

-- A minimal happy-path llm stub: plain text answer, fixed usage.
local function stub_llm(text)
    return function(_request)
        return {
            status = "ok",
            content = { { type = "text", text = text } },
            usage = { input_tokens = 10, output_tokens = 5 },
            stop_reason = "end_turn",
        }
    end
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("knl.open — state/config split", function()
    it("returns a ctx whose config fields read directly (memory map)", function()
        local llm = stub_llm("hi")
        local ctx = K.open({ owner = "spec", llm = llm, max_turns = 7 })
        expect(ctx.llm).to.be(llm)
        expect(ctx.max_turns).to.be(7)
        -- state field passes through the handle
        expect(ctx.owner).to.be("spec")
    end)

    it("rejects unknown options loudly", function()
        expect(function()
            K.open({ llm = stub_llm("x"), inptu = "typo" })
        end).to.fail()
    end)

    it("rejects config type errors at open time", function()
        expect(function()
            K.open({ llm = stub_llm("x"), filters = "not-a-table" })
        end).to.fail()
    end)
end)

describe("ctx immutability", function()
    it("assignment raises (__newindex guard)", function()
        local ctx = K.open({ llm = stub_llm("x") })
        expect(function()
            ctx.llm = stub_llm("mutated")
        end).to.fail()
    end)

    it("state methods delegate to the kernel handle", function()
        local ctx = K.open({ llm = stub_llm("x") })
        ctx:append({ kind = "msg_user", content = "hello" })
        local evs = ctx:events()
        expect(#evs).to.be(1)
        expect(evs[1].kind).to.be("msg_user")
        expect(evs[1].seq).to.be(1) -- kernel-owned stamp
    end)
end)

describe("ctx:with — composable derivation", function()
    it("derives a new ctx over the same session; the original is untouched", function()
        local weak, strong = stub_llm("weak"), stub_llm("strong")
        local ctx = K.open({ llm = weak, system = "base" })
        local ctx2 = ctx:with({ llm = strong })
        expect(ctx2.llm).to.be(strong)
        expect(ctx2.system).to.be("base") -- inherited
        expect(ctx.llm).to.be(weak) -- original intact
        -- same session: an append through one is visible through the other
        ctx2:append({ kind = "msg_user", content = "shared" })
        expect(#ctx:events()).to.be(1)
    end)

    it("rejects state keys in the delta (owner/store/session)", function()
        local ctx = K.open({ llm = stub_llm("x") })
        expect(function()
            ctx:with({ owner = "other" })
        end).to.fail()
        expect(function()
            ctx:with({ store = { sqlite = "p" } })
        end).to.fail()
    end)

    it("validates the merged config", function()
        local ctx = K.open({ llm = stub_llm("x") })
        expect(function()
            ctx:with({ tool_policy = "not-a-function" })
        end).to.fail()
    end)
end)

describe("knl.beat — the primitive", function()
    it("errors without an llm in the ctx", function()
        local ctx = K.open({ owner = "spec" })
        local o = K.beat(ctx)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("conf")
    end)

    it("errors on a non-ctx argument", function()
        local o = K.beat({ llm = stub_llm("x") }) -- a bare table, not a ctx
        expect(Outcome.is_error(o)).to.be(true)
    end)

    it("one beat: records request + model_response, kernel numbers it", function()
        local ctx = K.open({ llm = stub_llm("answer") })
        ctx:append({ kind = "msg_user", content = "q" })
        local o = K.beat(ctx)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(o.out.beat).to.be(1) -- kernel-assigned number, read back
        local evs = ctx:events()
        -- msg_user, request (write-ahead), model_response
        expect(evs[2].kind).to.be("request")
        expect(evs[3].kind).to.be("model_response")
    end)

    it("stops ok/budget_stopped when the budget is exhausted", function()
        local ctx = K.open({ llm = stub_llm("x"), budget = { tokens = 10 } })
        ctx:append({ kind = "msg_user", content = "q" })
        local first = K.beat(ctx) -- charges 15 > 10
        expect(Outcome.is_ok(first)).to.be(true)
        local second = K.beat(ctx)
        expect(Outcome.is_ok(second)).to.be(true)
        expect(second.out.budget_stopped).to.be(true)
    end)

    it("escalation: beat(ctx:with{llm=strong}) uses strong for that beat only", function()
        local called = {}
        local function tagged(tag)
            return function(_req)
                called[#called + 1] = tag
                return {
                    status = "ok",
                    content = { { type = "text", text = tag } },
                    usage = {},
                    stop_reason = "end_turn",
                }
            end
        end
        local ctx = K.open({ llm = tagged("weak") })
        ctx:append({ kind = "msg_user", content = "q" })
        K.beat(ctx)
        K.beat(ctx:with({ llm = tagged("strong") }))
        K.beat(ctx)
        expect(called[1]).to.be("weak")
        expect(called[2]).to.be("strong")
        expect(called[3]).to.be("weak") -- original policy untouched
    end)

    it("runs tools and stamps both pair halves with the beat number", function()
        local llm_with_tool = function(_req)
            return {
                status = "ok",
                content = {
                    { type = "text", text = "using a tool" },
                    { type = "tool_use", id = "c1", name = "echo", input = { s = "hi" } },
                },
                usage = {},
                stop_reason = "tool_use",
            }
        end
        local ctx = K.open({
            llm = llm_with_tool,
            tools = {
                echo = {
                    description = "echo",
                    input_schema = { type = "object" },
                    handler = function(args)
                        return args.s
                    end,
                },
            },
        })
        ctx:append({ kind = "msg_user", content = "q" })
        local o = K.beat(ctx)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(#o.out.tools).to.be(1)
        expect(o.out.tools[1].ok).to.be(true)
        local evs = ctx:events()
        local call, result
        for _, ev in ipairs(evs) do
            if ev.kind == "tool_call" then
                call = ev
            elseif ev.kind == "tool_result" then
                result = ev
            end
        end
        expect(call.turn).to.be(o.out.beat)
        expect(result.turn).to.be(o.out.beat)
        expect(result.ok).to.be(true)
    end)
end)

describe("knl shapes — data contracts, asserted in dev mode", function()
    local lshape = require("lshape")
    local shape = lshape.check

    local function in_dev(fn)
        local saved = shape.is_dev_mode
        shape.is_dev_mode = function()
            return true
        end
        local ok, err = pcall(fn)
        shape.is_dev_mode = saved
        if not ok then
            error(err, 0)
        end
    end

    it("exposes outcome / request / event contracts via M.shapes", function()
        expect(K.shapes.outcome).to.exist()
        expect(K.shapes.request).to.exist()
        expect(K.shapes.event).to.exist()
        expect(K.shapes.events.model_response).to.exist()
    end)

    it("a beat's Outcome validates against the outcome shape", function()
        local ctx = K.open({ llm = stub_llm("shaped") })
        ctx:append({ kind = "msg_user", content = "q" })
        local o = K.beat(ctx)
        local ok = shape.check(o, K.shapes.outcome)
        expect(ok).to.be(true)
    end)

    it("dev mode: a malformed known-kind event is rejected at append", function()
        in_dev(function()
            local ctx = K.open({ llm = stub_llm("x") })
            -- tool_result without call_id/ok: the per-kind contract fails loud
            expect(function()
                ctx:append({ kind = "tool_result", result = "r" })
            end).to.fail()
            -- an unknown kind passes on the base contract alone (open vocabulary)
            ctx:append({ kind = "run_started", payload = { note = "ok" } })
            -- but a kind-less event fails the base contract
            expect(function()
                ctx:append({ content = "no kind" })
            end).to.fail()
        end)
    end)

    it("dev mode: a filter that breaks the request shape fails loud", function()
        in_dev(function()
            local ctx = K.open({
                llm = stub_llm("x"),
                filters = {
                    function(_req)
                        return { messages = "not-an-array" }
                    end,
                },
            })
            ctx:append({ kind = "msg_user", content = "q" })
            expect(function()
                K.beat(ctx)
            end).to.fail()
        end)
    end)

    it("prod mode: the same malformed event passes through (no-op gate)", function()
        local ctx = K.open({ llm = stub_llm("x") })
        ctx:append({ kind = "tool_result", result = "r" }) -- no raise
        expect(#ctx:events()).to.be(1)
    end)
end)

describe("beat contract hardening (review findings)", function()
    it("a raising llm is Error('call'), not a raw raise", function()
        local ctx = K.open({
            llm = function()
                error("adapter exploded")
            end,
        })
        ctx:append({ kind = "msg_user", content = "q" })
        local o = K.beat(ctx)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("call")
        -- the failure is noted in the history
        local last = ctx:events()[#ctx:events()]
        expect(last.kind).to.be("model_call_failed")
    end)

    it("an append that fails is Error('state'), not a raw raise", function()
        local ctx = K.open({ llm = stub_llm("x") })
        ctx._handle.append = function()
            error("head conflict: expected 1, actual 2")
        end
        local o = K.beat(ctx)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("state")
    end)

    it("a missing usage defaults to an empty count on the record", function()
        local ctx = K.open({
            llm = function()
                return {
                    status = "ok",
                    content = { { type = "text", text = "hi" } },
                    stop_reason = "end_turn",
                }
            end,
        })
        ctx:append({ kind = "msg_user", content = "q" })
        local o = K.beat(ctx)
        expect(Outcome.is_ok(o)).to.be(true)
        local resp
        for _, ev in ipairs(ctx:events()) do
            if ev.kind == "model_response" then
                resp = ev
            end
        end
        expect(type(resp.usage)).to.be("table")
    end)

    it("a filter returning nil is Error('filter') in prod", function()
        local ctx = K.open({
            llm = stub_llm("x"),
            filters = {
                function(req)
                    req.system = "mutated"
                    -- forgets to return
                end,
            },
        })
        ctx:append({ kind = "msg_user", content = "q" })
        local o = K.beat(ctx)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("filter")
        -- nothing was recorded: the corrupt request never reached append
        expect(#ctx:events()).to.be(1)
    end)

    it("a raising tool_policy denies the call (fail-closed)", function()
        local ran = false
        local ctx = K.open({
            llm = (function()
                local sent = false
                return function()
                    if sent then
                        return { status = "ok", content = {}, usage = {}, stop_reason = "end_turn" }
                    end
                    sent = true
                    return {
                        status = "ok",
                        content = { { type = "tool_use", id = "c1", name = "danger", input = {} } },
                        usage = {},
                        stop_reason = "tool_use",
                    }
                end
            end)(),
            tools = {
                danger = {
                    handler = function()
                        ran = true
                        return "side effect"
                    end,
                },
            },
            tool_policy = function()
                error("policy bug")
            end,
        })
        ctx:append({ kind = "msg_user", content = "q" })
        local o = K.beat(ctx)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(ran).to.be(false) -- the handler never executed
        expect(o.out.tools[1].ok).to.be(false)
    end)
end)

describe("fold hardening (review findings)", function()
    it("empty messages carry the JSON-array tag across the boundary", function()
        local req = K.fold({}, {})
        expect(#req.messages).to.be(0)
        expect(getmetatable(req.messages).__jsontype).to.be("array")
    end)

    it("a dangling tool_use (crash mid-tool) is closed with a synthetic error result", function()
        local events = {
            { kind = "msg_user", content = "go", seq = 1 },
            {
                kind = "model_response",
                turn = 1,
                content = {
                    { type = "text", text = "using tools" },
                    { type = "tool_use", id = "c1", name = "t", input = {} },
                    { type = "tool_use", id = "c2", name = "t", input = {} },
                },
                seq = 2,
            },
            -- c1 was answered before the crash; c2 was not
            { kind = "tool_result", turn = 1, call_id = "c1", ok = true, result = "R", seq = 3 },
        }
        local req = K.fold(events, {})
        -- user "go" / assistant / user [c1 result + synthetic c2 result]
        expect(#req.messages).to.be(3)
        local closing = req.messages[3].content
        expect(#closing).to.be(2)
        expect(closing[1].tool_use_id).to.be("c1")
        expect(closing[2].tool_use_id).to.be("c2")
        expect(closing[2].is_error).to.be(true)
    end)

    it("an answered tool pair needs no repair (no synthetic results)", function()
        local events = {
            {
                kind = "model_response",
                turn = 1,
                content = { { type = "tool_use", id = "c1", name = "t", input = {} } },
                seq = 1,
            },
            { kind = "tool_result", turn = 1, call_id = "c1", ok = true, result = "R", seq = 2 },
            { kind = "model_response", turn = 2, content = { { type = "text", text = "done" } }, seq = 3 },
        }
        local req = K.fold(events, {})
        expect(#req.messages).to.be(3)
        expect(#req.messages[2].content).to.be(1) -- just the real result
    end)
end)

describe("knl.resume — config re-supplied per process", function()
    it("re-attaches config onto the resumed handle", function()
        local llm = stub_llm("resumed")
        local ctx = K.resume({
            store = { sqlite = "p.db" },
            session = "sess-1",
            llm = llm,
        })
        expect(ctx.llm).to.be(llm)
        expect(ctx.resumed_from).to.be("sess-1")
    end)
end)
