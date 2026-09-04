-- knl_turn_test.lua — the POC spec for the Lua kernel (knl.turn / knl.run /
-- fold / Outcome). Runs in the full host, because the kernel drives the real
-- Rust `knl` syscall bridge (session / append / events / call / spend /
-- close), which the pure lspec runner does not have.
--
-- Every case is an `assert` (a failure exits non-zero) plus a `[KNL] ...`
-- marker line the Rust harness can match. The final line is `[KNL] all_ok`.
--
-- Coverage maps to poc-knl-turn.md's eight invariants:
--   inv1 Outcome 3-values + predicates + match loud fail
--   inv2 the beat turns (write-ahead: request event before model_response)
--   inv3 the tool pair (known tool runs; unknown tool closes ok=false)
--   inv4 the request event is in the store and readable via events()
--   inv5 budget stop (turn-level Ok, and run stops on it)
--   inv6 infinite prevention (run with no budget/max_turns is Error(conf))
--   inv7 fold's three-kind mapping + tool_result turn attribution
--   inv8 status comes from the backend (ok / refused / error round-trips)

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

local function mark(label)
    print("[KNL] " .. label .. "=ok")
end

-- ---------------------------------------------------------------------------
-- inv1 — Outcome: three values, predicates, exhaustive match
-- ---------------------------------------------------------------------------

do
    local ok = Outcome.ok({ turn = 1 })
    local refused = Outcome.refused("no", { turn = 1 })
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
-- inv7 — fold: three-kind mapping + tool_result turn attribution (pure)
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

    -- system and tools are composed from conf, not read from the history
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
    local s = knl.open({ owner = "test", budget = { tokens = 100 } })
    s:append({ kind = "msg_user", content = "hi" })
    local o = kernel.turn({ ctx = s, backend = stub(response("ok", { { type = "text", text = "hello" } })) })
    assert(Outcome.is_ok(o))

    -- request is recorded before the model_response (write-ahead, §2 [3][4])
    assert(kinds_of(s) == "run_started,msg_user,request,model_response", kinds_of(s))

    -- inv4: the request that was sent is in the store, readable via events()
    local req_ev = first_of(s, "request")
    assert(req_ev ~= nil and req_ev.request ~= nil)
    assert(#req_ev.request.messages == 1 and req_ev.request.messages[1].content == "hi")

    -- The appended model_response was numbered and counted by the kernel:
    -- turn 1, and usage counts it (no author, no Lua-side spend).
    local resp = first_of(s, "model_response")
    assert(resp ~= nil and resp.turn == 1, "kernel turn: " .. tostring(resp and resp.turn))
    assert(o.out.turn == 1, "out turn: " .. tostring(o.out.turn))
    assert(s:turns() == 1, "s:turns(): " .. tostring(s:turns()))
    assert(s:view("usage").model_calls == 1, "usage did not count the response")
    assert(s:remaining() < 100, "the response was not charged")

    mark("inv2_writeahead")
    mark("inv4_request_event")
    mark("inv_usage_counts_append")
end

-- ---------------------------------------------------------------------------
-- inv3 — the tool pair: a known tool runs, an unknown one closes ok=false
-- ---------------------------------------------------------------------------

do
    local ran = {}
    local s = knl.open({ owner = "test", budget = { tokens = 100 } })
    local o = kernel.turn({
        ctx = s,
        backend = stub(response("ok", { tool_use("c1", "echo", { v = "x" }) })),
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
    assert(Outcome.is_ok(o))
    assert(#ran == 1 and ran[1] == "x", "handler did not run")
    assert(kinds_of(s) == "run_started,request,model_response,tool_call,tool_result", kinds_of(s))

    local tr = first_of(s, "tool_result")
    assert(tr.ok == true and tr.result == "echoed:x" and tr.call_id == "c1")
    assert(#o.out.tools == 1 and o.out.tools[1].ok == true and o.out.tools[1].name == "echo")

    -- unknown tool: the pair is still closed, ok=false, machine-minimal error
    local s2 = knl.open({ owner = "test", budget = { tokens = 100 } })
    local o2 = kernel.turn({
        ctx = s2,
        backend = stub(response("ok", { tool_use("c9", "ghost", {}) })),
        tools = {},
    })
    assert(Outcome.is_ok(o2))
    local tr2 = first_of(s2, "tool_result")
    assert(tr2 ~= nil and tr2.ok == false, "unknown tool must close the pair with ok=false")
    assert(tostring(tr2.result):find("not found", 1, true) ~= nil, tostring(tr2.result))

    -- a handler that raises also closes the pair ok=false
    local s3 = knl.open({ owner = "test", budget = { tokens = 100 } })
    local o3 = kernel.turn({
        ctx = s3,
        backend = stub(response("ok", { tool_use("c3", "boom", {}) })),
        tools = { boom = {
            handler = function()
                error("kaboom")
            end,
        } },
    })
    assert(Outcome.is_ok(o3))
    local tr3 = first_of(s3, "tool_result")
    assert(tr3.ok == false and tostring(tr3.result):find("raised", 1, true) ~= nil, tostring(tr3.result))

    mark("inv3_tool_pair")
end

-- ---------------------------------------------------------------------------
-- inv8 — status comes from the backend (kernel loads it, does not invent it)
-- ---------------------------------------------------------------------------

do
    -- ok
    local so = knl.open({ owner = "test", budget = { tokens = 100 } })
    assert(Outcome.is_ok(kernel.turn({ ctx = so, backend = stub(response("ok")) })))

    -- refused: the model answered (recorded + charged) but refused to proceed
    local sr = knl.open({ owner = "test", budget = { tokens = 100 } })
    local before = sr:remaining()
    local orf = kernel.turn({ ctx = sr, backend = stub(response("refused", { { type = "text", text = "no" } })) })
    assert(Outcome.is_refused(orf), "refused status must map to Refused")
    assert(first_of(sr, "model_response") ~= nil, "a refusal is still a recorded response")
    assert(sr:remaining() < before, "a refusal still costs its tokens")

    -- error: the beat did not come off — no model_response, a model_call_failed
    local se = knl.open({ owner = "test", budget = { tokens = 100 } })
    local oe = kernel.turn({
        ctx = se,
        backend = stub({ status = "error", detail = "boom", content = {}, usage = {} }),
    })
    assert(Outcome.is_error(oe) and oe.kind == "call", "error status must map to Error(call)")
    assert(first_of(se, "model_response") == nil, "an errored call must record no response")
    assert(first_of(se, "model_call_failed") ~= nil, "an errored call is noted as a failed call")

    -- a transport failure (nil, err) is also Error(call)
    local st = knl.open({ owner = "test", budget = { tokens = 100 } })
    local ot = kernel.turn({
        ctx = st,
        backend = function()
            return nil, "network down"
        end,
    })
    assert(Outcome.is_error(ot) and ot.kind == "call")

    mark("inv8_status_from_backend")
end

-- ---------------------------------------------------------------------------
-- inv5 — budget stop: turn returns Ok(budget), and run halts on it
-- ---------------------------------------------------------------------------

do
    -- turn-level: an exhausted session stops at the gate, before any call
    local s = knl.open({ owner = "test", budget = { tokens = 10 } })
    s:spend(10)
    assert(s:exhausted())
    local o = kernel.turn({ ctx = s, backend = stub(response("ok")) })
    assert(Outcome.is_ok(o) and o.out.budget_stopped == true, "exhausted turn must be Ok(budget_stopped)")
    assert(first_of(s, "request") == nil, "no call is made once the budget is gone")

    -- run-level: the first call exhausts the budget and asks for a tool, so
    -- the second beat stops at the gate and the run returns Ok.
    local orun, srun = kernel.run({
        budget = { tokens = 10 },
        input = "go",
        backend = stub(
            response("ok", { tool_use("c1", "noop", {}) }, { input_tokens = 100 }),
            response("ok", { { type = "text", text = "unreached" } })
        ),
        tools = { noop = {
            handler = function()
                return "done"
            end,
        } },
    })
    assert(Outcome.is_ok(orun) and orun.out.budget_stopped == true)
    assert(count_of(srun, "model_response") == 1, "the second beat must not have called the model")

    mark("inv5_budget_stop")
end

-- ---------------------------------------------------------------------------
-- inv6 — infinite prevention: run needs a budget or max_turns
-- ---------------------------------------------------------------------------

do
    local o = kernel.run({ backend = stub(response("ok")) })
    assert(Outcome.is_error(o) and o.kind == "conf", "run with no bound must be Error(conf)")
    mark("inv6_infinite_prevention")
end

-- ---------------------------------------------------------------------------
-- run driver — settle on a plain answer, and be bounded by max_turns
-- ---------------------------------------------------------------------------

do
    -- settles: no tool_use, so run stops after one beat
    local o, s = kernel.run({
        max_turns = 5,
        input = "hi",
        backend = stub(response("ok", { { type = "text", text = "done" } })),
    })
    assert(Outcome.is_ok(o) and not o.out.budget_stopped)
    assert(count_of(s, "model_response") == 1)
    -- run closed the session it opened; reads still work
    assert(count_of(s, "run_finished") == 1, "run must close the session it opened")

    -- bounded: the model never stops asking for a tool, so max_turns caps it
    local o2, s2 = kernel.run({
        max_turns = 3,
        input = "loop",
        backend = stub(
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
    assert(Outcome.is_ok(o2))
    assert(count_of(s2, "model_response") == 3, "max_turns must cap the number of calls")

    mark("run_driver")
end

print("[KNL] all_ok")
