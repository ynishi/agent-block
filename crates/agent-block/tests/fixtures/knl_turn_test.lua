-- knl_turn_test.lua — the POC spec for the Lua kernel (knl.beat / ctx /
-- fold / Outcome). Runs in the full host, because the kernel drives the real
-- Rust `knl` syscall bridge (session / append / events / spend / close),
-- which the pure lspec runner does not have.
--
-- Every case is an `assert` (a failure exits non-zero) plus a `[KNL] ...`
-- marker line the Rust harness can match. The final line is `[KNL] all_ok`.
--
-- Coverage maps to poc-knl-turn.md's invariants, restated for the beat/ctx
-- surface (core-loop-design.md: run is gone — a loop is written by the
-- caller on the spot):
--   inv1 Outcome 3-values + predicates + match loud fail
--   inv2 the beat turns (write-ahead: request event before model_response)
--   inv3 the tool pair (known tool runs; unknown tool closes ok=false)
--   inv4 the request event is in the store and readable via events()
--   inv5 budget stop (beat-level Ok, and a caller loop stops on it)
--   inv6 loop composition (a self-written while-beat loop settles on a
--        plain answer and is bounded by the caller's own cap)
--   inv7 fold's three-kind mapping + tool_result attribution
--   inv8 status comes from the backend (ok / refused / error round-trips)
--   inv9 ctx is immutable and with-derivation swaps policy for one beat

-- `knl` (global) is the Rust syscall bridge; `kernel` (local) is the Lua
-- module under test. They share the name deliberately (design §0.5).
local kernel = require("knl")
local Outcome = kernel.Outcome

-- ---------------------------------------------------------------------------
-- helpers
-- ---------------------------------------------------------------------------

-- A backend stub that hands back queued responses. A queued function is
-- called with the request instead (a case makes the backend fail that way).
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

-- A backend response: `{ status, content, usage, stop_reason }`, the shape the
-- POC stub returns and the kernel reads the status off.
local function response(status, blocks, usage, stop_reason)
    return {
        status = status,
        content = blocks or { { type = "text", text = "ok" } },
        usage = usage or { input_tokens = 5, output_tokens = 2 },
        stop_reason = stop_reason,
    }
end

local function tool_use(id, name, input)
    return { type = "tool_use", id = id, name = name, input = input or {} }
end

-- The recorded kinds in seq order, as one comparable string.
local function kinds_of(s)
    local names = {}
    for _, e in ipairs(s:events()) do
        names[#names + 1] = e.kind
    end
    return table.concat(names, ",")
end

-- The first event of `kind`, or nil.
local function first_of(s, kind)
    for _, e in ipairs(s:events()) do
        if e.kind == kind then
            return e
        end
    end
    return nil
end

-- How many events of `kind` the session holds.
local function count_of(s, kind)
    local n = 0
    for _, e in ipairs(s:events()) do
        if e.kind == kind then
            n = n + 1
        end
    end
    return n
end

-- Whether an Ok outcome's response asks for at least one tool (loop helper —
-- run is gone from the module, so the fixture composes its own).
local function has_tool_use(out)
    for _, block in ipairs(out.content or {}) do
        if block.type == "tool_use" then
            return true
        end
    end
    return false
end

local function mark(label)
    print("[KNL] " .. label .. "=ok")
end

-- ---------------------------------------------------------------------------
-- inv1 — Outcome: three values, predicates, exhaustive match
-- ---------------------------------------------------------------------------

do
    local ok = Outcome.ok({ beat = 1 })
    local refused = Outcome.refused("no", { beat = 1 })
    local err = Outcome.err("call", "boom")

    -- plain-data status tags, no metatable (survives the JSON boundary)
    assert(ok.status == "ok" and getmetatable(ok) == nil)
    assert(refused.status == "refused" and refused.reason == "no")
    assert(err.status == "error" and err.kind == "call" and err.detail == "boom")

    assert(Outcome.is_ok(ok) and not Outcome.is_ok(refused) and not Outcome.is_ok(err))
    assert(Outcome.is_refused(refused) and not Outcome.is_refused(ok))
    assert(Outcome.is_error(err) and not Outcome.is_error(ok))

    -- match dispatches on status
    local seen = Outcome.match(refused, {
        ok = function()
            return "O"
        end,
        refused = function(o)
            return "R:" .. o.reason
        end,
        error = function()
            return "E"
        end,
    })
    assert(seen == "R:no", seen)

    -- a missing arm is a loud failure, not a silent drop
    local complete, _ = pcall(Outcome.match, ok, { ok = function() end })
    assert(not complete, "match with a missing arm must raise")

    mark("inv1_outcome")
end

-- ---------------------------------------------------------------------------
-- inv7 — fold: three-kind mapping + tool_result attribution (pure)
-- ---------------------------------------------------------------------------

do
    local events = {
        { kind = "run_started", seq = 1 },
        { kind = "msg_user", content = "hi", seq = 2 },
        {
            kind = "model_response",
            turn = 1,
            content = { { type = "text", text = "a" }, tool_use("c1", "t", {}) },
            seq = 3,
        },
        { kind = "tool_call", turn = 1, call_id = "c1", seq = 4 },
        { kind = "tool_result", turn = 1, call_id = "c1", ok = true, result = "R", seq = 5 },
        { kind = "model_response", turn = 2, content = { { type = "text", text = "done" } }, seq = 6 },
        { kind = "note", seq = 7 },
    }
    local req = kernel.fold(events, {})

    -- user "hi" / assistant / user [tool_result] / assistant "done":
    -- run_started, tool_call, note are skipped; the tool_result lands in the
    -- user turn right after the assistant it answers.
    assert(#req.messages == 4, "fold produced " .. #req.messages .. " messages")
    assert(req.messages[1].role == "user" and req.messages[1].content == "hi")
    assert(req.messages[2].role == "assistant")
    assert(req.messages[3].role == "user")
    assert(req.messages[3].content[1].type == "tool_result")
    assert(req.messages[3].content[1].tool_use_id == "c1")
    assert(req.messages[3].content[1].content == "R")
    assert(req.messages[4].role == "assistant")

    -- a non-string tool_result is JSON-encoded
    local encoded = kernel.fold({
        { kind = "model_response", turn = 1, content = {}, seq = 1 },
        { kind = "tool_result", turn = 1, call_id = "c1", ok = true, result = { x = 1 }, seq = 2 },
    }, {})
    assert(type(encoded.messages[2].content[1].content) == "string", "non-string result must be encoded")

    -- system and tools are composed from the config, not read from the history
    local composed = kernel.fold({}, {
        system = "SYS",
        tools = { alpha = { description = "a", input_schema = { type = "object" } } },
    })
    assert(composed.system == "SYS")
    assert(#composed.tools == 1 and composed.tools[1].name == "alpha")
    assert(composed.tools[1].input_schema.type == "object")

    mark("inv7_fold")
end

-- ---------------------------------------------------------------------------
-- inv2 + inv4 — the beat turns write-ahead; the request event is readable
-- ---------------------------------------------------------------------------

do
    local ctx = kernel.open({
        owner = "test",
        budget = { tokens = 100 },
        llm = stub(response("ok", { { type = "text", text = "hello" } })),
    })
    ctx:append({ kind = "msg_user", content = "hi" })
    local o = kernel.beat(ctx)
    assert(Outcome.is_ok(o))

    -- request is recorded before the model_response (write-ahead)
    assert(kinds_of(ctx) == "run_started,msg_user,request,model_response", kinds_of(ctx))

    -- inv4: the request that was sent is in the store, readable via events()
    local req_ev = first_of(ctx, "request")
    assert(req_ev ~= nil and req_ev.request ~= nil)
    assert(#req_ev.request.messages == 1 and req_ev.request.messages[1].content == "hi")

    -- The appended model_response was numbered and counted by the kernel:
    -- beat 1, and usage counts it (no author, no Lua-side spend).
    local resp = first_of(ctx, "model_response")
    assert(resp ~= nil and resp.turn == 1, "kernel number: " .. tostring(resp and resp.turn))
    assert(o.out.beat == 1, "out beat: " .. tostring(o.out.beat))
    assert(ctx:turns() == 1, "ctx:turns(): " .. tostring(ctx:turns()))
    assert(ctx:view("usage").model_calls == 1, "usage did not count the response")
    assert(ctx:remaining() < 100, "the response was not charged")

    mark("inv2_writeahead")
    mark("inv4_request_event")
    mark("inv_usage_counts_append")
end

-- ---------------------------------------------------------------------------
-- inv3 — the tool pair: a known tool runs, an unknown one closes ok=false
-- ---------------------------------------------------------------------------

do
    local ran = {}
    local ctx = kernel.open({
        owner = "test",
        budget = { tokens = 100 },
        llm = stub(response("ok", { tool_use("c1", "echo", { v = "x" }) })),
        tools = {
            echo = {
                description = "e",
                input_schema = { type = "object" },
                handler = function(args)
                    ran[#ran + 1] = args.v
                    return "echoed:" .. tostring(args.v)
                end,
            },
        },
    })
    local o = kernel.beat(ctx)
    assert(Outcome.is_ok(o))
    assert(#ran == 1 and ran[1] == "x", "handler did not run")
    assert(kinds_of(ctx) == "run_started,request,model_response,tool_call,tool_result", kinds_of(ctx))

    local tr = first_of(ctx, "tool_result")
    assert(tr.ok == true and tr.result == "echoed:x" and tr.call_id == "c1")
    assert(#o.out.tools == 1 and o.out.tools[1].ok == true and o.out.tools[1].name == "echo")

    -- unknown tool: the pair is still closed, ok=false, machine-minimal error
    local ctx2 = kernel.open({
        owner = "test",
        budget = { tokens = 100 },
        llm = stub(response("ok", { tool_use("c9", "ghost", {}) })),
        tools = {},
    })
    local o2 = kernel.beat(ctx2)
    assert(Outcome.is_ok(o2))
    local tr2 = first_of(ctx2, "tool_result")
    assert(tr2 ~= nil and tr2.ok == false, "unknown tool must close the pair with ok=false")
    assert(tostring(tr2.result):find("not found", 1, true) ~= nil, tostring(tr2.result))

    -- a handler that raises also closes the pair ok=false
    local ctx3 = kernel.open({
        owner = "test",
        budget = { tokens = 100 },
        llm = stub(response("ok", { tool_use("c3", "boom", {}) })),
        tools = { boom = {
            handler = function()
                error("kaboom")
            end,
        } },
    })
    local o3 = kernel.beat(ctx3)
    assert(Outcome.is_ok(o3))
    local tr3 = first_of(ctx3, "tool_result")
    assert(tr3.ok == false and tostring(tr3.result):find("raised", 1, true) ~= nil, tostring(tr3.result))

    mark("inv3_tool_pair")
end

-- ---------------------------------------------------------------------------
-- inv8 — status comes from the backend (kernel loads it, does not invent it)
-- ---------------------------------------------------------------------------

do
    -- ok
    local co = kernel.open({ owner = "test", budget = { tokens = 100 }, llm = stub(response("ok")) })
    assert(Outcome.is_ok(kernel.beat(co)))

    -- refused: the model answered (recorded + charged) but refused to proceed
    local cr = kernel.open({
        owner = "test",
        budget = { tokens = 100 },
        llm = stub(response("refused", { { type = "text", text = "no" } })),
    })
    local before = cr:remaining()
    local orf = kernel.beat(cr)
    assert(Outcome.is_refused(orf), "refused status must map to Refused")
    assert(first_of(cr, "model_response") ~= nil, "a refusal is still a recorded response")
    assert(cr:remaining() < before, "a refusal still costs its tokens")

    -- error: the beat did not come off — no model_response, a model_call_failed
    local ce = kernel.open({
        owner = "test",
        budget = { tokens = 100 },
        llm = stub({ status = "error", detail = "boom", content = {}, usage = {} }),
    })
    local oe = kernel.beat(ce)
    assert(Outcome.is_error(oe) and oe.kind == "call", "error status must map to Error(call)")
    assert(first_of(ce, "model_response") == nil, "an errored call must record no response")
    assert(first_of(ce, "model_call_failed") ~= nil, "an errored call is noted as a failed call")

    -- a transport failure (nil, err) is also Error(call)
    local ct = kernel.open({
        owner = "test",
        budget = { tokens = 100 },
        llm = function()
            return nil, "network down"
        end,
    })
    local ot = kernel.beat(ct)
    assert(Outcome.is_error(ot) and ot.kind == "call")

    mark("inv8_status_from_backend")
end

-- ---------------------------------------------------------------------------
-- inv5 — budget stop: beat returns Ok(budget), and a caller loop halts on it
-- ---------------------------------------------------------------------------

do
    -- beat-level: an exhausted session stops at the gate, before any call
    local ctx = kernel.open({ owner = "test", budget = { tokens = 10 }, llm = stub(response("ok")) })
    ctx:spend(10)
    assert(ctx:exhausted())
    local o = kernel.beat(ctx)
    assert(Outcome.is_ok(o) and o.out.budget_stopped == true, "exhausted beat must be Ok(budget_stopped)")
    assert(first_of(ctx, "request") == nil, "no call is made once the budget is gone")

    -- loop-level: the first call exhausts the budget and asks for a tool, so
    -- the second beat stops at the gate — the caller's own loop halts on it.
    local cl = kernel.open({
        owner = "test",
        budget = { tokens = 10 },
        llm = stub(
            response("ok", { tool_use("c1", "noop", {}) }, { input_tokens = 100 }),
            response("ok", { { type = "text", text = "unreached" } })
        ),
        tools = { noop = {
            handler = function()
                return "done"
            end,
        } },
    })
    cl:append({ kind = "msg_user", content = "go" })
    local last
    while true do
        last = kernel.beat(cl)
        if not Outcome.is_ok(last) then
            break
        end
        if last.out.budget_stopped or not has_tool_use(last.out) then
            break
        end
    end
    assert(Outcome.is_ok(last) and last.out.budget_stopped == true)
    assert(count_of(cl, "model_response") == 1, "the second beat must not have called the model")

    mark("inv5_budget_stop")
end

-- ---------------------------------------------------------------------------
-- inv6 — loop composition: a self-written loop settles and is bounded
-- ---------------------------------------------------------------------------

do
    -- settles: no tool_use, so the loop stops after one beat, and close
    -- lays down the run_finished record
    local ctx = kernel.open({
        owner = "test",
        max_turns = 5,
        budget = { tokens = 1000 },
        llm = stub(response("ok", { { type = "text", text = "done" } })),
    })
    ctx:append({ kind = "msg_user", content = "hi" })
    local calls = 0
    while calls < ctx.max_turns do
        local o = kernel.beat(ctx)
        assert(Outcome.is_ok(o) and not o.out.budget_stopped)
        calls = calls + 1
        if not has_tool_use(o.out) then
            break
        end
    end
    assert(calls == 1)
    assert(count_of(ctx, "model_response") == 1)
    ctx:close("done")
    assert(count_of(ctx, "run_finished") == 1, "close must lay down the run_finished record")

    -- bounded: the model never stops asking for a tool, so the caller's own
    -- cap (ctx.max_turns, a config field read off the ctx) stops the loop
    local cb = kernel.open({
        owner = "test",
        max_turns = 3,
        budget = { tokens = 1000 },
        llm = stub(
            response("ok", { tool_use("a", "noop", {}) }),
            response("ok", { tool_use("b", "noop", {}) }),
            response("ok", { tool_use("c", "noop", {}) })
        ),
        tools = { noop = {
            handler = function()
                return "r"
            end,
        } },
    })
    cb:append({ kind = "msg_user", content = "loop" })
    local n = 0
    while n < cb.max_turns do
        local o = kernel.beat(cb)
        assert(Outcome.is_ok(o))
        n = n + 1
        if not has_tool_use(o.out) then
            break
        end
    end
    assert(count_of(cb, "model_response") == 3, "the caller cap must bound the number of calls")

    mark("inv6_loop_composition")
end

-- ---------------------------------------------------------------------------
-- inv9 — ctx immutability + with-derivation over the real bridge
-- ---------------------------------------------------------------------------

do
    local weak_called, strong_called = 0, 0
    local weak = function(_req)
        weak_called = weak_called + 1
        return response("ok", { { type = "text", text = "weak" } })
    end
    local strong = function(_req)
        strong_called = strong_called + 1
        return response("ok", { { type = "text", text = "strong" } })
    end

    local ctx = kernel.open({ owner = "test", budget = { tokens = 1000 }, llm = weak })
    ctx:append({ kind = "msg_user", content = "q" })

    -- mutating the ctx raises (immutable handle)
    local mutated = pcall(function()
        ctx.llm = strong
    end)
    assert(not mutated, "ctx assignment must raise")

    -- with-derivation: the strong llm serves exactly one beat, the original
    -- policy is untouched, and both write the same session's history
    assert(Outcome.is_ok(kernel.beat(ctx)))
    assert(Outcome.is_ok(kernel.beat(ctx:with({ llm = strong }))))
    assert(Outcome.is_ok(kernel.beat(ctx)))
    assert(weak_called == 2 and strong_called == 1, weak_called .. "/" .. strong_called)
    assert(count_of(ctx, "model_response") == 3, "all beats must land in the one session")
    assert(ctx:turns() == 3)

    mark("inv9_ctx_with")
end

print("[KNL] all_ok")
