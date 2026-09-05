-- knl_beat_test.lua — the POC spec for the Lua kernel (knl.beat / session /
-- device / fold / Outcome). Runs in the full host, because the kernel drives
-- the real Rust `knl` syscall bridge (session / append / events / reserve /
-- spend / close / new_beat_id), which the pure lspec runner does not have.
--
-- Every case is an `assert` (a failure exits non-zero) plus a `[KNL] ...`
-- marker line the Rust harness can match. The final line is `[KNL] all_ok`.
--
-- Coverage, on the session / device surface (beat takes the state and the
-- policy as two arguments, and the loop is written by the caller):
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
--   inv8 status comes from the llm (ok / refused round-trip, and a call that
--        did not come off is Error("call") — there is no third status)
--   inv9 the device is frozen, with-derivation swaps policy for one beat,
--        and every event of one beat carries the id that beat declared
--  inv10 the bridge's declared surface and the Lua registry agree in both
--        directions — methods, module functions and the error
--        vocabulary — and a real raise reads back classified
--  inv11 the SQL read: the published read schema matches the
--        shell's declaration, the four predefined views answer what the beat
--        wrote (token usage among them — it is a query view, not a kernel
--        built-in), a write is refused as "validation", and one statement
--        reads across a named set of sessions
--  inv12 the stored shape: the envelope is closed
--        (a stray top-level key is refused as "validation"), `meta` is
--        shallow (a nested one is refused the same way), and a label given
--        in `meta` comes back verbatim
--  inv13 `policy` is reachable as an embedded lib, its `window` fold bounds
--        what a beat sends, and its `stagnation` predicate stops a caller's
--        loop that would otherwise run to the budget
--  inv14 a session opened from a session: the allocation moves units out of
--        the parent's balance in one write, the child beats on its own, and
--        `knl.views.tree` reads the edge back out of the log
--  inv15 `supervisor.parallel`: two children of one parent run AT ONCE on
--        `std.task` (the nursery the pure spec runner does not have), the
--        results come back aligned by index, one sibling raising leaves the
--        other's untouched, both edges close, and the parent's ledger carries
--        both allocations

-- `knl` (global) is the Rust syscall bridge; `kernel` (local) is the Lua
-- module under test. They share the name deliberately: the Lua kernel is the
-- shell's face of the same kernel the bridge exposes.
local kernel = require("knl")
local Outcome = kernel.Outcome

-- ---------------------------------------------------------------------------
-- helpers
-- ---------------------------------------------------------------------------

-- An llm stub that hands back queued answers. A queued function is called
-- with the request instead (a case makes the call fail that way).
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

-- An `llm_result`: `{ status, content, usage, stop_reason, refusal? }` — the
-- shape the POC stub returns and the kernel reads the status off. A refusal
-- carries the class that refused, because that is what beat reports.
local function response(status, blocks, usage, stop_reason, refusal)
    return {
        status = status,
        content = blocks or { { type = "text", text = "ok" } },
        usage = usage or { input_tokens = 5, output_tokens = 2 },
        stop_reason = stop_reason,
        refusal = refusal or (status == "refused" and { kind = "model" } or nil),
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
-- there is no loop in the module, so the fixture composes its own).
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
    local stopped = Outcome.stopped("budget", "beats")

    -- plain-data status tags, no metatable (survives the JSON boundary)
    assert(ok.status == "ok" and getmetatable(ok) == nil)
    assert(refused.status == "refused" and refused.reason == "no")
    assert(err.status == "error" and err.kind == "call" and err.detail == "boom")
    assert(stopped.status == "stopped" and stopped.reason == "budget" and stopped.tag == "beats")

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
    -- The stored shape: one envelope (kind / beat / meta / data) with the
    -- kind's own content under `data`.
    local events = {
        { kind = "session_opened", seq = 1 },
        { kind = "msg_user", data = { content = "hi" }, seq = 2 },
        {
            kind = "llm_response",
            beat = "b1",
            data = { content = { { type = "text", text = "a" }, tool_use("c1", "t", {}) }, usage = {} },
            seq = 3,
        },
        { kind = "tool_call", beat = "b1", data = { call_id = "c1", name = "t", args = {} }, seq = 4 },
        { kind = "tool_result", beat = "b1", data = { call_id = "c1", ok = true, result = "R" }, seq = 5 },
        {
            kind = "llm_response",
            beat = "b2",
            data = { content = { { type = "text", text = "done" } }, usage = {} },
            seq = 6,
        },
        { kind = "note", seq = 7 },
    }
    local req = kernel.fold(events, {})

    -- user "hi" / assistant / user [tool_result] / assistant "done":
    -- session_opened, tool_call, note are skipped; the tool_result lands in
    -- the user message right after the assistant it answers.
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
        { kind = "llm_response", beat = "b1", data = { content = {}, usage = {} }, seq = 1 },
        { kind = "tool_result", beat = "b1", data = { call_id = "c1", ok = true, result = { x = 1 } }, seq = 2 },
    }, {})
    assert(type(encoded.messages[2].content[1].content) == "string", "non-string result must be encoded")

    -- system and tools are composed from the device, not read from the history
    local composed = kernel.fold(
        {},
        kernel.device({
            system = "SYS",
            tools = {
                alpha = {
                    description = "a",
                    input_schema = { type = "object" },
                    handler = function()
                        return "a"
                    end,
                },
            },
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
    local s = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
    local d = kernel.device({ llm = stub(response("ok", { { type = "text", text = "hello" } })) })
    s:append({ kind = "msg_user", data = { content = "hi" } })
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
    assert(req_ev ~= nil and req_ev.data ~= nil and req_ev.data.request ~= nil)
    assert(#req_ev.data.request.messages == 1 and req_ev.data.request.messages[1].content == "hi")

    -- The beat named itself and stamped what it wrote: the id is an opaque
    -- string, the same one on the request and on the response, and the
    -- caller's own seed carries none.
    local resp = first_of(s, "llm_response")
    assert(type(o.out.beat) == "string", "beat id: " .. tostring(o.out.beat))
    assert(resp ~= nil and resp.beat == o.out.beat, "response beat: " .. tostring(resp and resp.beat))
    assert(req_ev.beat == o.out.beat, "request beat: " .. tostring(req_ev.beat))
    assert(first_of(s, "msg_user").beat == nil, "the caller's seed is not part of a beat")

    -- The accounting is a query view now (`knl.views.usage`), and it counts
    -- the same thing the kernel's built-in used to: the responses this
    -- session recorded, with the counts the provider reported for them.
    local usage_rows = kernel.views.usage(s)
    assert(#usage_rows == 1, "one stream that answered, " .. #usage_rows .. " rows")
    assert(usage_rows[1].calls == 1, "usage did not count the response: " .. tostring(usage_rows[1].calls))
    assert(usage_rows[1].stream == s:id(), "the row names the stream: " .. tostring(usage_rows[1].stream))
    assert(usage_rows[1].input_tokens == 5, "input: " .. tostring(usage_rows[1].input_tokens))
    assert(usage_rows[1].output_tokens == 2, "output: " .. tostring(usage_rows[1].output_tokens))
    -- The stub reported no thinking tokens; a missing counter is 0, never nil.
    assert(usage_rows[1].thinking_tokens == 0, "thinking: " .. tostring(usage_rows[1].thinking_tokens))

    -- And the kernel does not serve that reading itself any longer: the
    -- built-in reads are `events` and `tail`, so asking it for "usage" is an
    -- unknown view — refused in the caller's own class, "validation".
    local served, raised = pcall(s.view, s, "usage")
    assert(
        not served,
        'the kernel still answers s:view("usage"): the Rust half of this round '
            .. "(removing the built-in usage view) has not landed yet — this one assertion is the only "
            .. "thing here that depends on it"
    )
    local refused = knl.error(raised)
    assert(
        refused.kind == "validation",
        's:view("usage") must be refused as an unknown view; kind: ' .. tostring(refused.kind)
    )

    -- The balance moved once, where the beat reserved it (one unit by
    -- default) — the appends themselves do not move the budget, and the 7
    -- tokens the usage view counted are a separate reading.
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
    local s = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
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
    assert(tr.data.ok == true and tr.data.result == "echoed:x" and tr.data.call_id == "c1")
    assert(#o.out.tools == 1 and o.out.tools[1].ok == true and o.out.tools[1].name == "echo")
    -- one beat, one id: the pair belongs to the response that asked for it
    assert(#distinct_beats(s) == 1 and distinct_beats(s)[1] == o.out.beat)
    assert(first_of(s, "tool_call").beat == o.out.beat and tr.beat == o.out.beat)

    -- unknown tool: the pair is still closed, ok=false, machine-minimal error
    local s2 = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
    local d2 = kernel.device({
        llm = stub(response("ok", { tool_use("c9", "ghost", {}) })),
        tools = {},
    })
    local o2 = kernel.beat(s2, d2)
    assert(Outcome.is_ok(o2))
    local tr2 = first_of(s2, "tool_result")
    assert(tr2 ~= nil and tr2.data.ok == false, "unknown tool must close the pair with ok=false")
    assert(tostring(tr2.data.result):find("not found", 1, true) ~= nil, tostring(tr2.data.result))

    -- a handler that raises also closes the pair ok=false
    local s3 = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
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
    assert(tr3.data.ok == false and tostring(tr3.data.result):find("raised", 1, true) ~= nil, tostring(tr3.data.result))

    mark("inv3_tool_pair")
end

-- ---------------------------------------------------------------------------
-- inv8 — status comes from the llm (the kernel loads it, does not invent it),
-- and there are only two: a call that did not come off is Error("call")
-- ---------------------------------------------------------------------------

do
    -- ok
    local so = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
    assert(Outcome.is_ok(kernel.beat(so, kernel.device({ llm = stub(response("ok")) }))))

    -- refused: the model answered (recorded) but refused to proceed, and the
    -- reason beat reports is the class the adapter classified it as
    local sr = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
    local dr = kernel.device({
        llm = stub(response("refused", { { type = "text", text = "no" } }, nil, "refusal", { kind = "model" })),
    })
    local before = sr:remaining()
    local orf = kernel.beat(sr, dr)
    assert(Outcome.is_refused(orf), "refused status must map to Refused")
    assert(orf.reason == "model", "the refusal names its class: " .. tostring(orf.reason))
    assert(first_of(sr, "llm_response") ~= nil, "a refusal is still a recorded response")
    -- A refusing beat reserved like any other: the allowance went before the
    -- call, and the recorded response added nothing on top of it.
    assert(sr:remaining() == before - 1, "refused beat balance: " .. tostring(sr:remaining()))

    -- a failed call: the beat did not come off — no llm_response, a
    -- llm_call_failed. There is no "error" status to return; the contract is
    -- a result, or nil and an error.
    local se = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
    local de = kernel.device({
        llm = function()
            return nil, "the provider said no"
        end,
    })
    local oe = kernel.beat(se, de)
    assert(Outcome.is_error(oe) and oe.kind == "call", "a failed call must map to Error(call)")
    assert(first_of(se, "llm_response") == nil, "a failed call must record no response")
    local failed = first_of(se, "llm_call_failed")
    assert(failed ~= nil, "a failed call is noted as a failed call")
    assert(type(failed.beat) == "string", "the note belongs to the beat that made the call")

    -- and an answer that is not an llm_result at all is the same ending: a
    -- third status is a broken adapter, not a branch the kernel reads.
    local sx = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
    local dx = kernel.device({
        llm = function()
            return { status = "error", content = {}, usage = {} }
        end,
    })
    local ox = kernel.beat(sx, dx)
    assert(Outcome.is_error(ox) and ox.kind == "call", "an unknown status must map to Error(call)")
    assert(first_of(sx, "llm_call_failed") ~= nil, "the broken contract is noted as a failed call")

    mark("inv8_status_from_llm")
end

-- ---------------------------------------------------------------------------
-- inv5 — budget stop: a refused reservation is `stopped` with no call, and a
-- caller loop halts on it
-- ---------------------------------------------------------------------------

do
    -- beat-level: nothing left to reserve, so the beat stops before calling
    local s = kernel.open({ owner = "test", budget = { amount = 10, tag = "beats" } })
    local d = kernel.device({ llm = stub(response("ok")) })
    s:spend(10)
    assert(s:exhausted())
    local o = kernel.beat(s, d)
    assert(Outcome.is_stopped(o), "a refused beat must be Stopped")
    assert(o.reason == "budget", "the stop names its cause: " .. tostring(o.reason))
    assert(o.tag == "beats", "the refusal names its grant: " .. tostring(o.tag))
    assert(first_of(s, "llm_request") == nil, "no call is made once the budget is gone")

    -- the reservation policy is the caller's: one that asks for the whole
    -- grant leaves nothing for a second beat.
    local sp = kernel.open({ owner = "test", budget = { amount = 10, tag = "beats" } })
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
    local sl = kernel.open({ owner = "test", budget = { amount = 1, tag = "beats" } })
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
    sl:append({ kind = "msg_user", data = { content = "go" } })
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
    -- The cap is the loop's own local: knl has no beat cap of its own,
    -- because the loop is written here and the stopping guarantee is the
    -- budget's.
    local MAX_BEATS = 5

    -- settles: no tool_use, so the loop stops after one beat. The bracket is
    -- the canonical lifecycle: knl.session closes on the way out, and the
    -- boundary lands in the log as session_closed.
    local settled = kernel.session({ owner = "test", budget = { amount = 1000, tag = "beats" } }, function(s)
        local d = kernel.device({ llm = stub(response("ok", { { type = "text", text = "done" } })) })
        s:append({ kind = "msg_user", data = { content = "hi" } })
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
        first_of(settled, "session_closed").data.reason == "scope_exit",
        "close reason: " .. tostring(first_of(settled, "session_closed").data.reason)
    )

    -- bounded: the model never stops asking for a tool, so the caller's own
    -- cap — a local of the loop that uses it — stops it
    local CAP = 3
    local sb = kernel.open({ owner = "test", budget = { amount = 1000, tag = "beats" } })
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
    sb:append({ kind = "msg_user", data = { content = "loop" } })
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

    local s = kernel.open({ owner = "test", budget = { amount = 1000, tag = "beats" } })
    local d = kernel.device({ llm = weak })
    s:append({ kind = "msg_user", data = { content = "q" } })

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
-- inv10 — the bridge's declared surface and the Lua registry agree
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

    -- The names agreeing is the weaker half. The shapes are the other one,
    -- and they are not two declarations any more: `knl_types` is generated at
    -- host start from the Rust argument and return types in `bridge/knl.rs`,
    -- and the registry above points at it. What is checked here is that the
    -- pointing is complete in both directions —
    --
    --   (a) every shape in the registry is one the generated module hands
    --       out, so a hand-written shape reappearing there goes red;
    --   (b) every type the module generates is referenced by some entry, so a
    --       type nobody declares goes red as well.
    --
    -- Identity, not equality: each generated type is a distinct table (a bare
    -- primitive is named with `:describe`, so `SessionId` and `Owner` are two
    -- values rather than one `T.string`).
    local knl_types = require("knl_types")
    local type_of, unused = {}, {}
    for name, shape in pairs(knl_types) do
        type_of[shape] = name
        unused[name] = true
    end
    assert(next(unused) ~= nil, "the generated types module is empty")

    local function is_shape(v)
        return type(v) == "table" and rawget(v, "kind") ~= nil
    end

    local function account_for(where, value)
        if not is_shape(value) then
            -- Prose: the arguments and returns no schema can state (a live
            -- session handle, a pair whose second value may be absent).
            return
        end
        local name = type_of[value]
        assert(name ~= nil, "hand-written shape in the bridge registry at " .. where)
        unused[name] = nil
    end

    for _, registry in pairs({ kernel.shapes.session, kernel.shapes.module }) do
        for name, entry in pairs(registry) do
            if type(entry.args) == "table" then
                for i, arg in ipairs(entry.args) do
                    account_for(name .. ".args[" .. i .. "]", arg)
                end
            end
            account_for(name .. ".returns", entry.returns)
        end
    end

    local orphans = {}
    for name in pairs(unused) do
        orphans[#orphans + 1] = name
    end
    table.sort(orphans)
    assert(#orphans == 0, "generated types nothing declares: " .. table.concat(orphans, ", "))

    -- And the module the kernel publishes as text is the module that was
    -- loaded, so a tool reading `knl.api().types` reads what is in force.
    assert(type(api.types) == "string" and #api.types > 0, "knl.api() publishes no types module")
    local rebuilt = assert(load(api.types, "knl_types(published)"))()
    for name in pairs(knl_types) do
        assert(rebuilt[name] ~= nil, "the published types text is missing " .. name)
    end

    -- The failure vocabulary is the same list on both sides. The
    -- kernel publishes it (`knl.api().errors`, built from KnlError::KINDS)
    -- and the shell closes its `error` shape on its own declaration; a
    -- class added to one and not the other goes red here, in both
    -- directions, exactly like the two method registries above.
    local declared_kinds, published_kinds = {}, {}
    for _, kind in ipairs(kernel.shapes.error_kinds) do
        declared_kinds[kind] = true
    end
    for _, kind in ipairs(api.errors) do
        published_kinds[kind] = true
        assert(declared_kinds[kind], "the kernel publishes an error class the shell does not declare: " .. kind)
    end
    for kind in pairs(declared_kinds) do
        assert(published_kinds[kind], "the shell declares an error class the kernel does not publish: " .. kind)
    end

    -- And a real raise, read back: appending to a closed session is the
    -- `closed` class, attributed to the method that refused, and not
    -- something to ask again about. The table still renders as the message
    -- it was read from, so code that only printed or searched the raise
    -- reads exactly what it read before.
    local ended = kernel.open({ owner = "test" })
    ended:close("done")
    local wrote, raised = pcall(ended.append, ended, { kind = "msg_user", data = { content = "after" } })
    assert(not wrote, "an append after close must raise")
    local read = knl.error(raised)
    assert(read.kind == "closed", "kind: " .. tostring(read.kind))
    assert(read.method == "append", "method: " .. tostring(read.method))
    assert(read.retryable == false, "closed is not a class to retry")
    assert(type(read.message) == "string" and #read.message > 0, "message: " .. tostring(read.message))
    assert(tostring(read):find(tostring(raised), 1, true), "__tostring must give the original text back")

    -- the bracket records the body's error as the boundary's detail
    local s_ref
    local ok, err = pcall(kernel.session, { owner = "test" }, function(s)
        s_ref = s
        error("boom-detail")
    end)
    assert(not ok and tostring(err):find("boom-detail", 1, true), "body error must propagate")
    local last = s_ref:events()[#s_ref:events()]
    assert(
        last.kind == "session_closed" and last.data.reason == "error",
        "close reason: " .. tostring(last.data.reason)
    )
    assert(
        type(last.data.detail) == "string" and last.data.detail:find("boom-detail", 1, true),
        "detail must carry the body error"
    )

    mark("inv10_api_registry")
end

-- ---------------------------------------------------------------------------
-- inv11 — the SQL read: the published schema, the four
-- predefined views over a real store, the refusal of anything that is not a
-- read, and one statement spanning a set of sessions
-- ---------------------------------------------------------------------------

do
    -- (a) one schema, declared on both sides. The kernel publishes the
    -- columns a caller writes SQL against (`knl.api().schema`) and the shell
    -- declares the same table as data (`knl.shapes.schema`); a column added
    -- to one and not the other goes red here, in both directions — the same
    -- arrangement inv10 has for the syscall registries.
    local published_schema = knl.api().schema
    assert(type(published_schema) == "table", "the kernel publishes no read schema")
    assert(
        published_schema.table == kernel.shapes.schema.table,
        "table name: " .. tostring(published_schema.table) .. " / " .. tostring(kernel.shapes.schema.table)
    )

    -- name -> pk, so the comparison is about the columns and their keys and
    -- not about the order they happen to be listed in.
    local function pk_by_name(columns)
        local out = {}
        for _, column in ipairs(columns) do
            out[column.name] = (column.pk == true)
        end
        return out
    end
    local published_cols = pk_by_name(published_schema.columns)
    local declared_cols = pk_by_name(kernel.shapes.schema.columns)
    for name, pk in pairs(published_cols) do
        assert(declared_cols[name] ~= nil, "the kernel publishes a column the shell does not declare: " .. name)
        assert(declared_cols[name] == pk, "pk flag differs for column " .. name)
    end
    for name in pairs(declared_cols) do
        assert(published_cols[name] ~= nil, "the shell declares a column the kernel does not publish: " .. name)
    end

    -- (b) the log-shaped views over a session that ran one beat with a tool
    -- call (`usage`, the fourth, is read against a real store in inv2 above)
    local s = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
    local d = kernel.device({
        llm = stub(response("ok", { tool_use("c1", "echo", { v = "x" }) })),
        tools = {
            echo = {
                description = "e",
                handler = function(args)
                    return "echoed:" .. tostring(args.v)
                end,
            },
        },
    })
    s:append({ kind = "msg_user", data = { content = "hi" } })
    local beat_out = kernel.beat(s, d)
    assert(Outcome.is_ok(beat_out))

    -- beats: one row for the one beat, holding both halves of the call it
    -- made. The seed message carries no beat and is not part of any.
    local beat_rows = kernel.views.beats(s)
    assert(#beat_rows == 1, "one beat, " .. #beat_rows .. " rows")
    assert(beat_rows[1].beat == beat_out.out.beat, "the row names the beat: " .. tostring(beat_rows[1].beat))
    local kinds = tostring(beat_rows[1].kinds)
    assert(kinds:find("llm_request", 1, true) ~= nil, "kinds: " .. kinds)
    assert(kinds:find("llm_response", 1, true) ~= nil, "kinds: " .. kinds)
    assert(beat_rows[1].seq_from <= beat_rows[1].seq_to, "seq range: " .. kinds)

    -- tool_pairs: the call and the result that answered it, joined on the
    -- call id, with `ok` read back as a boolean (SQLite has no such type)
    local pair_rows = kernel.views.tool_pairs(s)
    assert(#pair_rows == 1, "one answered call, " .. #pair_rows .. " rows")
    assert(pair_rows[1].call_id == "c1", "call_id: " .. tostring(pair_rows[1].call_id))
    assert(pair_rows[1].name == "echo", "name: " .. tostring(pair_rows[1].name))
    assert(pair_rows[1].ok == true, "ok: " .. tostring(pair_rows[1].ok))
    assert(pair_rows[1].beat == beat_out.out.beat, "the pair belongs to its beat")

    -- ledger: the grant that opened the account, then the reservation the
    -- beat made before it called — in seq order, with the grant's own tag
    local ledger_rows = kernel.views.ledger(s)
    assert(#ledger_rows == 2, "grant + reservation, got " .. #ledger_rows)
    assert(ledger_rows[1].kind == "budget_granted", "first: " .. tostring(ledger_rows[1].kind))
    assert(ledger_rows[2].kind == "budget_reserved", "second: " .. tostring(ledger_rows[2].kind))
    assert(ledger_rows[1].amount == 100, "granted: " .. tostring(ledger_rows[1].amount))
    assert(ledger_rows[2].amount == 1, "reserved: " .. tostring(ledger_rows[2].amount))
    assert(ledger_rows[1].tag == "beats", "tag: " .. tostring(ledger_rows[1].tag))

    -- (b2) the ledger view's `data` paths are the kernel's own field names.
    --
    -- The columns are one half of the read contract and these are the other:
    -- everything a `budget_*` event is ABOUT lives inside the `data` column,
    -- and `LEDGER_SQL` reaches it with `json_extract(data, '$.amount')` —
    -- the Rust `FIELD_AMOUNT` constant, spelled again in SQL, in another
    -- language, in another file, with nothing holding the two together. A
    -- rename on the Rust side would have left this view answering NULL for
    -- every row and no test saying so.
    --
    -- `knl.api().fields` publishes those names from the constants
    -- themselves, so the check is: read the row BY THE PUBLISHED NAME and
    -- expect the value the grant was made with. It fails from either side —
    -- rename the constant and the lookup misses the column, change the
    -- `json_extract` path and the column is NULL.
    local fields = knl.api().fields
    assert(type(fields) == "table", "knl.api() publishes no data field names")
    local granted_row = ledger_rows[1]
    assert(
        granted_row[fields.amount] == 100,
        "the ledger's amount path is not the kernel's " .. tostring(fields.amount)
    )
    assert(granted_row[fields.tag] == "beats", "the ledger's tag path is not the kernel's " .. tostring(fields.tag))
    -- The refusal's own extra path, on a reservation the balance cannot take.
    local small = kernel.open({ owner = "test", budget = { amount = 1, tag = "beats" } })
    assert(small:reserve(99) == false, "a reservation past the balance must be refused")
    local refused_rows = kernel.views.ledger(small)
    local refusal_row = refused_rows[#refused_rows]
    assert(refusal_row.kind == "budget_refused", "last: " .. tostring(refusal_row.kind))
    assert(refusal_row[fields.amount] == 99, "the refusal records what was asked for")
    -- And the two the tree view reads, published beside them: a supervisor's
    -- `json_extract(data, '$.parent')` is the same string the kernel writes.
    for _, name in ipairs({ "parent", "open_children", "scope_id", "owner", "reason", "remaining" }) do
        assert(type(fields[name]) == "string" and #fields[name] > 0, "no published name for data." .. name)
    end
    small:close("done")

    -- (c) a query reads. A write does not reach the store through it, and
    -- the refusal is the caller's class: the argument did not hold up.
    local wrote, raised = pcall(s.query, s, "INSERT INTO events (stream, seq) VALUES ('x', 1)")
    assert(not wrote, "a write must not go through query")
    local refusal = knl.error(raised)
    assert(refusal.kind == "validation", "kind: " .. tostring(refusal.kind))

    -- (d) one statement, a set of sessions. Two sessions on one file store,
    -- and the same view reads across both when the caller names them —
    -- which is what `opts.sessions` is for (a session tree, or sessions
    -- that were split and are being read back together).
    local path = os.tmpname()
    local store = { sqlite = path }
    local sa = kernel.open({ owner = "test", budget = { amount = 10, tag = "beats" }, store = store })
    local sb = kernel.open({ owner = "test", budget = { amount = 10, tag = "beats" }, store = store })
    assert(sa:id() ~= sb:id(), "two opens on one store are two streams")
    assert(Outcome.is_ok(kernel.beat(sa, kernel.device({ llm = stub(response("ok")) }))))
    assert(Outcome.is_ok(kernel.beat(sb, kernel.device({ llm = stub(response("ok")) }))))

    local own = kernel.views.beats(sa)
    assert(#own == 1, "the default set is the session's own stream: " .. #own)
    local both = kernel.views.beats(sa, { sessions = { sa:id(), sb:id() } })
    assert(#both == 2, "a named set reads both streams: " .. #both)
    local seen = {}
    for _, row in ipairs(both) do
        seen[row.beat] = true
    end
    assert(seen[own[1].beat], "the set includes the session's own beat")

    sa:close("done")
    sb:close("done")
    os.remove(path)

    -- (e) the read's own failure class is in the one vocabulary both sides
    -- publish (inv10 holds the whole list in both directions; this is the
    -- class this round added).
    local api_kinds = {}
    for _, kind in ipairs(knl.api().errors) do
        api_kinds[kind] = true
    end
    local shell_kinds = {}
    for _, kind in ipairs(kernel.shapes.error_kinds) do
        shell_kinds[kind] = true
    end
    assert(api_kinds.timeout, "the kernel must publish the timeout class")
    assert(shell_kinds.timeout, "the shell must declare the timeout class")

    mark("inv11_sql_views")
end

-- ---------------------------------------------------------------------------
-- inv12 — the stored shape: the envelope is
-- closed, `meta` is shallow, and a label given rides through verbatim
-- ---------------------------------------------------------------------------

do
    local s = kernel.open({ owner = "test" })

    -- (a) `{ kind, beat?, meta?, data? }` and nothing else. The old form —
    -- a kind's own fields sitting at the top level — is not stored under a
    -- different name, it is refused: the caller's argument did not hold up,
    -- which is the "validation" class.
    local stray, stray_raise = pcall(s.append, s, { kind = "msg_user", content = "x" })
    assert(not stray, "a stray top-level key must not be stored")
    assert(
        knl.error(stray_raise).kind == "validation",
        "a stray top-level key is a validation failure; kind: " .. tostring(knl.error(stray_raise).kind)
    )

    -- (b) `meta` is labels, not a second `data`. A nested value would make
    -- it structure that a view could read and a schema change could break,
    -- which is exactly what `data` exists to hold instead.
    local nested, nested_raise = pcall(s.append, s, {
        kind = "msg_user",
        meta = { label = { deep = true } },
        data = { content = "x" },
    })
    assert(not nested, "a nested meta must not be stored")
    assert(
        knl.error(nested_raise).kind == "validation",
        "a nested meta is a validation failure; kind: " .. tostring(knl.error(nested_raise).kind)
    )

    -- (c) and a label that IS shallow comes back as it went in — the half of
    -- the envelope a view can read without ever being broken by a change to
    -- what a kind carries.
    s:append({ kind = "msg_user", meta = { label = "seed" }, data = { content = "x" } })
    local seeded = first_of(s, "msg_user")
    assert(seeded ~= nil, "the well-formed seed was not stored")
    assert(type(seeded.meta) == "table", "meta: " .. tostring(seeded.meta))
    assert(seeded.meta.label == "seed", "meta.label: " .. tostring(seeded.meta.label))
    assert(seeded.data.content == "x", "data.content: " .. tostring(seeded.data.content))

    s:close("done")

    mark("inv12_stored_shape")
end

-- ---------------------------------------------------------------------------
-- inv13 — the `policy` lib, embedded: a windowed fold in the device and a
-- stagnation predicate in the loop
-- ---------------------------------------------------------------------------

do
    -- Reachable by name in the full host, which is the half of this that only
    -- a host can prove: the module is baked into the binary (EMBEDDED_LIBS),
    -- not found on a search path a spec runner set up.
    local policy = require("policy")
    for _, name in ipairs({ "window", "carry", "stagnation", "retry", "escalate", "shapes" }) do
        assert(policy[name] ~= nil, "the embedded policy lib is missing " .. name)
    end

    local s = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
    s:append({ kind = "msg_user", data = { content = "go" } })

    -- A model that never stops making the same call: without the predicate
    -- this loop runs until the caller's cap or the grant stops it.
    local at = 0
    local d = kernel.device({
        llm = function(_req)
            at = at + 1
            return response("ok", { tool_use("c" .. at, "search", { q = "same" }) }, nil, "tool_use")
        end,
        tools = {
            search = {
                handler = function()
                    return "no results"
                end,
            },
        },
        fold = policy.window({ tail = 2 }),
    })
    local stalled = policy.stagnation({ same = 2 })

    local CAP = 10
    local beats, why = 0, nil
    while beats < CAP do
        local o = kernel.beat(s, d)
        assert(Outcome.is_ok(o), "the stubbed beat must come off")
        beats = beats + 1
        why = stalled(s)
        if why ~= nil then
            break
        end
    end

    -- The predicate stopped it, and it stopped it at the second beat — not the
    -- cap, and nowhere near the grant.
    assert(why == "repeated", "stagnation reason: " .. tostring(why))
    assert(beats == 2, "the predicate must stop the loop at two beats, not " .. beats)
    assert(s:remaining() == 98, "remaining: " .. tostring(s:remaining()))

    -- And the window is doing its work in the same device. Two more beats by
    -- hand, so the fourth folds a log holding three beats: `tail = 2` keeps the
    -- last two of them and the seed falls outside the window entirely.
    assert(Outcome.is_ok(kernel.beat(s, d)))
    assert(Outcome.is_ok(kernel.beat(s, d)))
    local sent = {}
    for _, e in ipairs(s:events()) do
        if e.kind == "llm_request" then
            sent[#sent + 1] = e.data.request
        end
    end
    assert(#sent == 4, "four beats, " .. #sent .. " requests")
    local windowed = sent[4]
    assert(#windowed.messages == 4, "two beats fold to four messages, got " .. #windowed.messages)
    assert(windowed.messages[1].role == "assistant", "the seed is outside the window")
    -- the unwindowed request the first beat sent still had it
    assert(sent[1].messages[1].content == "go", "the first beat saw the whole log")

    s:close("done")

    mark("inv13_policy_embedded")
end

-- ---------------------------------------------------------------------------
-- inv14 — a session opened from a session: the allocation is one write, the
-- child beats on its own budget, and the tree is read back out of the log
-- ---------------------------------------------------------------------------

do
    local parent = kernel.open({ owner = "test", budget = { amount = 100, tag = "beats" } })
    local child = kernel.open({
        owner = "worker",
        parent = parent,
        budget = { from_parent = 10 },
    })

    -- (a) the units moved: the child holds exactly what the parent paid, and
    -- the parent's balance fell by it. Nothing was created and nothing lost.
    assert(child:id() ~= parent:id(), "a child is a stream of its own")
    assert(child:remaining() == 10, "child: " .. tostring(child:remaining()))
    assert(parent:remaining() == 90, "parent: " .. tostring(parent:remaining()))

    -- (b) it is an ordinary session — a beat runs in it, on its own budget
    child:append({ kind = "msg_user", data = { content = "go" } })
    local out = kernel.beat(child, kernel.device({ llm = stub(response("ok")) }))
    assert(Outcome.is_ok(out), "the child's beat: " .. tostring(out.status))
    assert(child:remaining() == 9, "the beat spent the child's own: " .. tostring(child:remaining()))
    assert(parent:remaining() == 90, "and not the parent's: " .. tostring(parent:remaining()))

    -- (c) the child's log says where it came from
    local opened = child:events()[1]
    assert(opened.kind == "session_opened", opened.kind)
    assert(opened.data.parent == parent:id(), "opened.data.parent: " .. tostring(opened.data.parent))

    -- (d) and the parent's ledger says what it paid, and to whom: the grant
    -- that opened the account, then the reservation that opened the child
    local ledger_rows = kernel.views.ledger(parent)
    assert(#ledger_rows == 2, "grant + allocation, got " .. #ledger_rows)
    assert(ledger_rows[2].kind == "budget_reserved", "second: " .. tostring(ledger_rows[2].kind))
    assert(ledger_rows[2].amount == 10, "allocated: " .. tostring(ledger_rows[2].amount))

    child:close("done")

    -- (e) the tree, discovered from the root rather than named: one recursive
    -- SELECT over `session_opened.data.parent`, and the edge is in it
    local rows = kernel.views.tree(parent)
    assert(#rows == 2, "the parent and its child, got " .. #rows)
    local by_id = {}
    for _, row in ipairs(rows) do
        by_id[row.session] = row
    end
    local root = by_id[parent:id()]
    assert(root ~= nil, "the root is in its own subtree")
    assert(root.parent == nil, "the root records no parent: " .. tostring(root.parent))
    assert(root.closed_epoch_ms == nil, "the root has not closed")
    local edge = by_id[child:id()]
    assert(edge ~= nil, "the child is in the tree")
    assert(edge.parent == parent:id(), "the edge names the parent: " .. tostring(edge.parent))
    assert(type(edge.opened_epoch_ms) == "number", "opened: " .. tostring(edge.opened_epoch_ms))
    assert(edge.closed_epoch_ms ~= nil, "the child closed and the tree says so")

    parent:close("done")

    mark("inv14_session_tree")
end

-- ---------------------------------------------------------------------------
-- inv15 — supervisor.parallel: siblings at once in one nursery, and what one
-- sibling's failure does to the rest
-- ---------------------------------------------------------------------------

do
    -- Reachable by name in the full host (EMBEDDED_LIBS), like `policy` in
    -- inv13 — and unlike `policy`, this one cannot be exercised anywhere else:
    -- `parallel` runs on `std.task`, which the pure spec runner has no
    -- registration for at all. Its pack spec covers the shapes and the calls it
    -- refuses; the concurrency is here.
    local supervisor = require("supervisor")

    -- One stubbed device for every child below: the beat is not what this
    -- invariant is about, and a queue would run out.
    local device = kernel.device({
        llm = function()
            return response("ok")
        end,
    })

    -- What a child reports back about its beat: the status, or the whole
    -- reading when it was not "ok". A slot that only said "error" would make a
    -- failure here a puzzle rather than a report.
    local function verdict(o)
        if o.status ~= "error" then
            return o.status
        end
        local detail = o.detail
        if type(detail) == "table" then
            detail = tostring(detail.kind) .. ": " .. tostring(detail.message)
        end
        return "error(" .. tostring(o.kind) .. "): " .. tostring(detail)
    end

    -- A FILE STORE, and not for durability. Siblings write to ONE database —
    -- their parent's — and the two stores differ in what simultaneous writers
    -- meet there [実測: 2026-09-05, this fixture]. The in-memory database is
    -- addressed by a shared-cache URI (`file:knl-<stream>?mode=memory&
    -- cache=shared`), and shared cache locks per TABLE: a second connection
    -- writing while the first holds that lock gets SQLITE_LOCKED at once, which
    -- `busy_timeout` does not wait out, so a child's beat comes back
    -- `err("state")` with `detail.kind == "busy"` — nondeterministically,
    -- depending on which write lands first. A file database has no shared
    -- cache, so the same contention is SQLITE_BUSY and the 5 s busy timeout
    -- waits it out.
    --
    -- That is the store's property and not the supervisor's, and `busy` is the
    -- one class the kernel calls retryable — asking again is the caller's
    -- loop's decision (`policy.retry`), which is why nothing here retries. What
    -- this invariant is about is what `parallel` promises, so it runs where the
    -- promise is not drowned out by the lock.
    local path = os.tmpname()
    local store = { sqlite = path }

    -- (a) AT ONCE, not one after the other. The first child sleeps at a cancel
    -- checkpoint and the second runs straight through; if the two were run in
    -- sequence the second could not possibly finish first.
    local at_once = kernel.open({ owner = "test", budget = { amount = 20, tag = "beats" }, store = store })
    local trace = {}
    local function step(label)
        trace[#trace + 1] = label
    end
    local function at(label)
        for i, seen in ipairs(trace) do
            if seen == label then
                return i
            end
        end
        return nil
    end

    local results = supervisor.parallel(at_once, {
        {
            opts = { budget = { amount = 5 } },
            fn = function(child)
                step("slow:start")
                std.task.sleep(20)
                child:append({ kind = "msg_user", data = { content = "slow" } })
                local o = kernel.beat(child, device)
                step("slow:end")
                return child:id(), verdict(o)
            end,
        },
        {
            opts = { budget = { amount = 5 } },
            fn = function(child)
                step("quick:start")
                child:append({ kind = "msg_user", data = { content = "quick" } })
                local o = kernel.beat(child, device)
                step("quick:end")
                return child:id(), verdict(o)
            end,
        },
    })

    local order = table.concat(trace, ",")
    assert(at("quick:end") < at("slow:end"), "the quick child must finish first: " .. order)
    assert(at("slow:start") < at("quick:end"), "the two must overlap: " .. order)

    -- Aligned by index, and every value the body returned is in the slot.
    assert(#results == 2, "one slot per child, got " .. #results)
    assert(results[1].ok and results[2].ok, "both children came off: " .. order)
    assert(results[1].values.n == 2, "two values, got " .. tostring(results[1].values.n))
    assert(results[1].values[2] == "ok", "the slow child's beat: " .. tostring(results[1].values[2]))
    assert(results[2].values[2] == "ok", "the quick child's beat: " .. tostring(results[2].values[2]))
    assert(results[1].values[1] ~= results[2].values[1], "two children, two streams")
    at_once:close("done")

    -- (b) ISOLATE is the default: one sibling raising is that sibling's, and
    -- the other runs to completion.
    local isolate = kernel.open({ owner = "test", budget = { amount = 20, tag = "beats" }, store = store })
    local ids = {}
    local mixed = supervisor.parallel(isolate, {
        {
            opts = { budget = { amount = 3 } },
            fn = function(child)
                ids[1] = child:id()
                child:append({ kind = "msg_user", data = { content = "go" } })
                return verdict(kernel.beat(child, device))
            end,
        },
        {
            opts = { budget = { amount = 4 } },
            fn = function(child)
                ids[2] = child:id()
                error("the second child went wrong", 0)
            end,
        },
    })

    local function slot_text(slot)
        if slot.ok then
            return "ok(" .. tostring(slot.values[1]) .. ")"
        end
        local err = slot.err
        if type(err) == "table" then
            err = tostring(err.kind) .. ": " .. tostring(err.message)
        end
        return "failed(" .. tostring(err) .. ", cancelled=" .. tostring(slot.cancelled) .. ")"
    end

    assert(mixed[1].ok, "the first child was untouched by the second: " .. slot_text(mixed[1]))
    assert(mixed[1].values[1] == "ok", "its beat: " .. tostring(mixed[1].values[1]))
    assert(not mixed[2].ok, "the second child failed")
    assert(
        tostring(mixed[2].err):find("the second child went wrong", 1, true) ~= nil,
        "the raise is kept verbatim: " .. tostring(mixed[2].err)
    )
    assert(mixed[2].cancelled == nil, "nothing cancelled it — isolate is the default")

    -- The parent paid for both, out of one balance: 20 - 3 - 4.
    assert(isolate:remaining() == 13, "the parent's balance: " .. tostring(isolate:remaining()))
    local reserved, total = 0, 0
    for _, row in ipairs(kernel.views.ledger(isolate)) do
        if row.kind == "budget_reserved" then
            reserved = reserved + 1
            total = total + row.amount
        end
    end
    assert(reserved == 2, "two allocations on the ledger, got " .. reserved)
    assert(total == 7, "and they add up to what left the balance: " .. total)

    -- Both edges are in the tree and both are closed — the bracket ended each
    -- child before this call returned, the raising one included.
    local rows = kernel.views.tree(isolate)
    assert(#rows == 3, "the parent and its two children, got " .. #rows)
    local closed = 0
    for _, row in ipairs(rows) do
        if row.session ~= isolate:id() then
            assert(row.parent == isolate:id(), "the edge names the parent: " .. tostring(row.parent))
            assert(row.closed_epoch_ms ~= nil, "the child closed: " .. tostring(row.session))
            closed = closed + 1
        else
            assert(row.open_children == nil, "the parent has not closed yet")
        end
    end
    assert(closed == 2, "both children closed, got " .. closed)
    assert(ids[1] ~= nil and ids[2] ~= nil, "both bodies ran")

    isolate:close("done")
    os.remove(path)

    mark("inv15_supervisor_parallel")
end

print("[KNL] all_ok")
