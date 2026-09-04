-- knl_turn_test.lua — the POC spec for the Lua kernel (knl.beat / session /
-- device / fold / Outcome). Runs in the full host, because the kernel drives
-- the real Rust `knl` syscall bridge (session / append / events / reserve /
-- spend / close / new_beat_id), which the pure lspec runner does not have.
--
-- Every case is an `assert` (a failure exits non-zero) plus a `[KNL] ...`
-- marker line the Rust harness can match. The final line is `[KNL] all_ok`.
--
-- Coverage maps to poc-knl-turn.md's invariants, restated for the
-- session/device surface (session-device-design.md: beat takes the state and
-- the policy as two arguments, and the loop is written by the caller):
--   inv1 Outcome 4 values + predicates + match loud fail
--   inv2 the beat runs (write-ahead: reservation, then the llm_request
--        event, then the llm_response)
--   inv3 the tool pair (known tool runs; unknown tool closes ok=false)
--   inv4 the llm_request event is in the store and readable via events()
--   inv5 budget stop (a refused reservation is `stopped`, made no call, and
--        a caller loop exits on it)
--   inv6 loop composition (a self-written while-beat loop settles on a
--        plain answer and is bounded by the caller's own cap)
--   inv7 fold's three-kind mapping + tool_result attribution
--   inv8 status comes from the backend (ok / refused / error round-trips)
--   inv9 the device is frozen, with-derivation swaps policy for one beat,
--        and every event of one beat carries the id that beat declared

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

-- The distinct `beat` ids in the session, in first-seen order. The kernel
-- numbers nothing: an id is an opaque string the shell declared.
local function distinct_beats(s)
    local seen, ids = {}, {}
    for _, e in ipairs(s:events()) do
        if e.beat ~= nil and not seen[e.beat] then
            seen[e.beat] = true
            ids[#ids + 1] = e.beat
        end
    end
    return ids
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
-- inv1 — Outcome: four values, predicates, exhaustive match
-- ---------------------------------------------------------------------------

do
    local ok = Outcome.ok({ beat = "b-1" })
    local refused = Outcome.refused("no", { beat = "b-1" })
    local err = Outcome.err("call", "boom")
    local stopped = Outcome.stopped("budget", "tokens")

    -- plain-data status tags, no metatable (survives the JSON boundary)
    assert(ok.status == "ok" and getmetatable(ok) == nil)
    assert(refused.status == "refused" and refused.reason == "no")
    assert(err.status == "error" and err.kind == "call" and err.detail == "boom")
    assert(stopped.status == "stopped" and stopped.reason == "budget" and stopped.tag == "tokens")

    assert(Outcome.is_ok(ok) and not Outcome.is_ok(refused) and not Outcome.is_ok(err))
    assert(Outcome.is_refused(refused) and not Outcome.is_refused(ok))
    assert(Outcome.is_error(err) and not Outcome.is_error(ok))
    assert(Outcome.is_stopped(stopped) and not Outcome.is_stopped(ok) and not Outcome.is_ok(stopped))

    -- match dispatches on status
    local arms = {
        ok = function()
            return "O"
        end,
        refused = function(o)
            return "R:" .. o.reason
        end,
        error = function()
            return "E"
        end,
        stopped = function(o)
            return "S:" .. o.reason
        end,
    }
    assert(Outcome.match(refused, arms) == "R:no")
    assert(Outcome.match(stopped, arms) == "S:budget")

    -- a missing arm is a loud failure, not a silent drop — and three arms
    -- are no longer enough now that a stop is its own status
    local complete = pcall(Outcome.match, ok, { ok = function() end })
    assert(not complete, "match with a missing arm must raise")
    local three = pcall(Outcome.match, ok, { ok = arms.ok, refused = arms.refused, error = arms.error })
    assert(not three, "match without the stopped arm must raise")

    mark("inv1_outcome")
end

-- ---------------------------------------------------------------------------
-- inv7 — fold: three-kind mapping + tool_result attribution (pure)
-- ---------------------------------------------------------------------------

do
    local events = {
        { kind = "session_opened", seq = 1 },
        { kind = "msg_user", content = "hi", seq = 2 },
        {
            kind = "llm_response",
            beat = "b1",
            content = { { type = "text", text = "a" }, tool_use("c1", "t", {}) },
            seq = 3,
        },
        { kind = "tool_call", beat = "b1", call_id = "c1", seq = 4 },
        { kind = "tool_result", beat = "b1", call_id = "c1", ok = true, result = "R", seq = 5 },
        { kind = "llm_response", beat = "b2", content = { { type = "text", text = "done" } }, seq = 6 },
        { kind = "note", seq = 7 },
    }
    local req = kernel.fold(events, {})

    -- user "hi" / assistant / user [tool_result] / assistant "done":
    -- session_opened, tool_call, note are skipped; the tool_result lands in
    -- the user turn right after the assistant it answers.
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
        { kind = "llm_response", beat = "b1", content = {}, seq = 1 },
        { kind = "tool_result", beat = "b1", call_id = "c1", ok = true, result = { x = 1 }, seq = 2 },
    }, {})
    assert(type(encoded.messages[2].content[1].content) == "string", "non-string result must be encoded")

    -- system and tools are composed from the device, not read from the history
    local composed = kernel.fold(
        {},
        kernel.device({
            system = "SYS",
            tools = { alpha = { description = "a", input_schema = { type = "object" } } },
        })
    )
    assert(composed.system == "SYS")
    assert(#composed.tools == 1 and composed.tools[1].name == "alpha")
    assert(composed.tools[1].input_schema.type == "object")

    mark("inv7_fold")
end

-- ---------------------------------------------------------------------------
-- inv2 + inv4 — the beat reserves, then records write-ahead; the llm_request
-- event is readable
-- ---------------------------------------------------------------------------

do
    local s = kernel.open({ owner = "test", budget = { amount = 100, tag = "tokens" } })
    local d = kernel.device({ llm = stub(response("ok", { { type = "text", text = "hello" } })) })
    s:append({ kind = "msg_user", content = "hi" })
    local o = kernel.beat(s, d)
    assert(Outcome.is_ok(o))

    -- the session records its own opening, the grant opens the ledger, the
    -- beat reserves before it calls, and the request is recorded before the
    -- llm_response (write-ahead)
    assert(
        kinds_of(s) == "session_opened,budget_granted,msg_user,budget_reserved,llm_request,llm_response",
        kinds_of(s)
    )

    -- inv4: the request that was sent is in the store, readable via events()
    local req_ev = first_of(s, "llm_request")
    assert(req_ev ~= nil and req_ev.request ~= nil)
    assert(#req_ev.request.messages == 1 and req_ev.request.messages[1].content == "hi")

    -- The beat named itself and stamped what it wrote: the id is an opaque
    -- string, the same one on the request and on the response, and the
    -- caller's own seed carries none.
    local resp = first_of(s, "llm_response")
    assert(type(o.out.beat) == "string", "beat id: " .. tostring(o.out.beat))
    assert(resp ~= nil and resp.beat == o.out.beat, "response beat: " .. tostring(resp and resp.beat))
    assert(req_ev.beat == o.out.beat, "request beat: " .. tostring(req_ev.beat))
    assert(first_of(s, "msg_user").beat == nil, "the caller's seed is not part of a beat")
    assert(s:view("usage").model_calls == 1, "usage did not count the response")
    -- The balance moved once, where the beat reserved it (one unit by
    -- default) — the appends themselves charge nothing, and the 7 tokens the
    -- usage view counted are a separate reading.
    assert(s:remaining() == 99, "reserved balance: " .. tostring(s:remaining()))

    mark("inv2_writeahead")
    mark("inv4_request_event")
    mark("inv_usage_counts_append")
end

-- ---------------------------------------------------------------------------
-- inv3 — the tool pair: a known tool runs, an unknown one closes ok=false
-- ---------------------------------------------------------------------------

do
    local ran = {}
    local s = kernel.open({ owner = "test", budget = { amount = 100, tag = "tokens" } })
    local d = kernel.device({
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
    local o = kernel.beat(s, d)
    assert(Outcome.is_ok(o))
    assert(#ran == 1 and ran[1] == "x", "handler did not run")
    assert(
        kinds_of(s) == "session_opened,budget_granted,budget_reserved,llm_request,llm_response,tool_call,tool_result",
        kinds_of(s)
    )

    local tr = first_of(s, "tool_result")
    assert(tr.ok == true and tr.result == "echoed:x" and tr.call_id == "c1")
    assert(#o.out.tools == 1 and o.out.tools[1].ok == true and o.out.tools[1].name == "echo")
    -- one beat, one id: the pair belongs to the response that asked for it
    assert(#distinct_beats(s) == 1 and distinct_beats(s)[1] == o.out.beat)
    assert(first_of(s, "tool_call").beat == o.out.beat and tr.beat == o.out.beat)

    -- unknown tool: the pair is still closed, ok=false, machine-minimal error
    local s2 = kernel.open({ owner = "test", budget = { amount = 100, tag = "tokens" } })
    local d2 = kernel.device({
        llm = stub(response("ok", { tool_use("c9", "ghost", {}) })),
        tools = {},
    })
    local o2 = kernel.beat(s2, d2)
    assert(Outcome.is_ok(o2))
    local tr2 = first_of(s2, "tool_result")
    assert(tr2 ~= nil and tr2.ok == false, "unknown tool must close the pair with ok=false")
    assert(tostring(tr2.result):find("not found", 1, true) ~= nil, tostring(tr2.result))

    -- a handler that raises also closes the pair ok=false
    local s3 = kernel.open({ owner = "test", budget = { amount = 100, tag = "tokens" } })
    local d3 = kernel.device({
        llm = stub(response("ok", { tool_use("c3", "boom", {}) })),
        tools = { boom = {
            handler = function()
                error("kaboom")
            end,
        } },
    })
    local o3 = kernel.beat(s3, d3)
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
    local so = kernel.open({ owner = "test", budget = { amount = 100, tag = "tokens" } })
    assert(Outcome.is_ok(kernel.beat(so, kernel.device({ llm = stub(response("ok")) }))))

    -- refused: the model answered (recorded) but refused to proceed
    local sr = kernel.open({ owner = "test", budget = { amount = 100, tag = "tokens" } })
    local dr = kernel.device({ llm = stub(response("refused", { { type = "text", text = "no" } })) })
    local before = sr:remaining()
    local orf = kernel.beat(sr, dr)
    assert(Outcome.is_refused(orf), "refused status must map to Refused")
    assert(first_of(sr, "llm_response") ~= nil, "a refusal is still a recorded response")
    -- A refusing beat reserved like any other: the allowance went before the
    -- call, and the recorded response added nothing on top of it.
    assert(sr:remaining() == before - 1, "refused beat balance: " .. tostring(sr:remaining()))

    -- error: the beat did not come off — no llm_response, a llm_call_failed
    local se = kernel.open({ owner = "test", budget = { amount = 100, tag = "tokens" } })
    local de = kernel.device({ llm = stub({ status = "error", detail = "boom", content = {}, usage = {} }) })
    local oe = kernel.beat(se, de)
    assert(Outcome.is_error(oe) and oe.kind == "call", "error status must map to Error(call)")
    assert(first_of(se, "llm_response") == nil, "an errored call must record no response")
    local failed = first_of(se, "llm_call_failed")
    assert(failed ~= nil, "an errored call is noted as a failed call")
    assert(type(failed.beat) == "string", "the note belongs to the beat that made the call")

    -- a transport failure (nil, err) is also Error(call)
    local st = kernel.open({ owner = "test", budget = { amount = 100, tag = "tokens" } })
    local dt = kernel.device({
        llm = function()
            return nil, "network down"
        end,
    })
    local ot = kernel.beat(st, dt)
    assert(Outcome.is_error(ot) and ot.kind == "call")

    mark("inv8_status_from_backend")
end

-- ---------------------------------------------------------------------------
-- inv5 — budget stop: a refused reservation is `stopped` with no call, and a
-- caller loop halts on it
-- ---------------------------------------------------------------------------

do
    -- beat-level: nothing left to reserve, so the beat stops before calling
    local s = kernel.open({ owner = "test", budget = { amount = 10, tag = "tokens" } })
    local d = kernel.device({ llm = stub(response("ok")) })
    s:spend(10)
    assert(s:exhausted())
    local o = kernel.beat(s, d)
    assert(Outcome.is_stopped(o), "a refused beat must be Stopped")
    assert(o.reason == "budget", "the stop names its cause: " .. tostring(o.reason))
    assert(o.tag == "tokens", "the refusal names its grant: " .. tostring(o.tag))
    assert(first_of(s, "llm_request") == nil, "no call is made once the budget is gone")

    -- the reservation policy is the caller's: one that asks for the whole
    -- grant leaves nothing for a second beat.
    local sp = kernel.open({ owner = "test", budget = { amount = 10, tag = "tokens" } })
    local dp = kernel.device({
        cost = function()
            return 10
        end,
        llm = stub(response("ok"), response("ok")),
    })
    assert(Outcome.is_ok(kernel.beat(sp, dp)) and sp:remaining() == 0)
    assert(Outcome.is_stopped(kernel.beat(sp, dp)))
    assert(count_of(sp, "llm_response") == 1, "the refused beat made no call")

    -- loop-level: one unit granted, one unit per beat, and the first answer
    -- asks for a tool — so the second beat is refused and the caller's own
    -- loop halts on it.
    local sl = kernel.open({ owner = "test", budget = { amount = 1, tag = "tokens" } })
    local dl = kernel.device({
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
    sl:append({ kind = "msg_user", content = "go" })
    local last
    while true do
        last = kernel.beat(sl, dl)
        if not Outcome.is_ok(last) then
            break
        end
        if not has_tool_use(last.out) then
            break
        end
    end
    assert(Outcome.is_stopped(last) and last.reason == "budget")
    assert(count_of(sl, "llm_response") == 1, "the second beat must not have called the model")

    mark("inv5_budget_stop")
end

-- ---------------------------------------------------------------------------
-- inv6 — loop composition: a self-written loop settles and is bounded
-- ---------------------------------------------------------------------------

do
    -- The cap is the loop's own local: knl has no max_turns config, because
    -- the loop is written here and the stopping guarantee is the budget's.
    local MAX_BEATS = 5

    -- settles: no tool_use, so the loop stops after one beat. The bracket is
    -- the canonical lifecycle: knl.session closes on the way out, and the
    -- boundary lands in the log as session_closed.
    local settled = kernel.session({ owner = "test", budget = { amount = 1000, tag = "tokens" } }, function(s)
        local d = kernel.device({ llm = stub(response("ok", { { type = "text", text = "done" } })) })
        s:append({ kind = "msg_user", content = "hi" })
        local calls = 0
        while calls < MAX_BEATS do
            local o = kernel.beat(s, d)
            assert(Outcome.is_ok(o))
            calls = calls + 1
            if not has_tool_use(o.out) then
                break
            end
        end
        assert(calls == 1)
        assert(count_of(s, "llm_response") == 1)
        return s
    end)
    assert(count_of(settled, "session_closed") == 1, "the bracket must lay down the session_closed record")
    -- One word for a normal scope exit: the bracket closes with the same
    -- reason `<close>` records, because leaving the scope is the same event
    -- whichever form wrote it.
    assert(
        first_of(settled, "session_closed").reason == "scope_exit",
        "close reason: " .. tostring(first_of(settled, "session_closed").reason)
    )

    -- bounded: the model never stops asking for a tool, so the caller's own
    -- cap — a local of the loop that uses it — stops it
    local CAP = 3
    local sb = kernel.open({ owner = "test", budget = { amount = 1000, tag = "tokens" } })
    local db = kernel.device({
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
    sb:append({ kind = "msg_user", content = "loop" })
    local n = 0
    while n < CAP do
        local o = kernel.beat(sb, db)
        assert(Outcome.is_ok(o))
        n = n + 1
        if not has_tool_use(o.out) then
            break
        end
    end
    assert(count_of(sb, "llm_response") == 3, "the caller cap must bound the number of calls")
    -- three beats, three ids, and every tool pair grouped under its own
    assert(#distinct_beats(sb) == 3, "three beats declared " .. #distinct_beats(sb) .. " ids")

    mark("inv6_loop_composition")
end

-- ---------------------------------------------------------------------------
-- inv9 — the device is frozen; with-derivation over the real bridge
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

    local s = kernel.open({ owner = "test", budget = { amount = 1000, tag = "tokens" } })
    local d = kernel.device({ llm = weak })
    s:append({ kind = "msg_user", content = "q" })

    -- mutating the device raises (it is a frozen value)
    local mutated = pcall(function()
        d.llm = strong
    end)
    assert(not mutated, "device assignment must raise")
    -- and state keys are not a device's to hold
    assert(not pcall(kernel.open, { llm = weak }), "knl.open must refuse a policy key")
    assert(not pcall(kernel.device, { owner = "test" }), "knl.device must refuse a state key")

    -- with-derivation: the strong llm serves exactly one beat, the original
    -- policy is untouched, and every beat writes the same session's history
    assert(Outcome.is_ok(kernel.beat(s, d)))
    assert(Outcome.is_ok(kernel.beat(s, d:with({ llm = strong }))))
    assert(Outcome.is_ok(kernel.beat(s, d)))
    assert(weak_called == 2 and strong_called == 1, weak_called .. "/" .. strong_called)
    assert(count_of(s, "llm_response") == 3, "all beats must land in the one session")
    assert(#distinct_beats(s) == 3, "three beats declared " .. #distinct_beats(s) .. " ids")

    mark("inv9_device_with")
end

-- ---------------------------------------------------------------------------
-- inv10 — the bridge's declared surface and the Lua registry agree (§9-m)
-- ---------------------------------------------------------------------------

do
    local api = knl.api()
    local function names_of(list)
        local set = {}
        for _, entry in ipairs(list) do
            set[entry.name] = true
        end
        return set
    end
    local declared_session, declared_module = names_of(api.session), names_of(api.module)

    -- every syscall the bridge declares has a shape entry here…
    for name in pairs(declared_session) do
        assert(kernel.shapes.session[name] ~= nil, "bridge session method without a shape: " .. name)
    end
    for name in pairs(declared_module) do
        assert(kernel.shapes.module[name] ~= nil, "bridge module function without a shape: " .. name)
    end
    -- …and no shape entry describes a syscall the bridge does not declare
    for name in pairs(kernel.shapes.session) do
        assert(declared_session[name], "stale session shape (bridge declares no such method): " .. name)
    end
    for name in pairs(kernel.shapes.module) do
        assert(declared_module[name], "stale module shape (bridge declares no such function): " .. name)
    end

    -- the bracket records the body's error as the boundary's detail
    local s_ref
    local ok, err = pcall(kernel.session, { owner = "test" }, function(s)
        s_ref = s
        error("boom-detail")
    end)
    assert(not ok and tostring(err):find("boom-detail", 1, true), "body error must propagate")
    local last = s_ref:events()[#s_ref:events()]
    assert(last.kind == "session_closed" and last.reason == "error", "close reason: " .. tostring(last.reason))
    assert(
        type(last.detail) == "string" and last.detail:find("boom-detail", 1, true),
        "detail must carry the body error"
    )

    mark("inv10_api_registry")
end

print("[KNL] all_ok")
