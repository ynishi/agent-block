-- carry_spec.lua — mlua-lspec unit tests for `policy.carry`, the filter that
-- carries the last beat's failure forward as one bounded note.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/carry_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- What this proves:
--   1 the binding convention: `policy.carry{...}` is a session-free policy and
--     `(session)` binds it — a session in the OPTS is refused as the typo it
--     is, and a binder handed something that is not a session says so where it
--     was bound rather than at the first beat;
--   2 a tool that failed and a call that did not come off are both carried,
--     and the note names which;
--   3 A TRUNCATED RESPONSE IS NOT CARRIED. It is a completed beat: it recorded
--     an llm_response, `stop_reason` says max_tokens, and nothing failed;
--   4 a clean beat is not carried at all — the very request that came in goes
--     back out, the same table;
--   5 the note is bounded by `max_bytes`, marked where it was cut, and trimmed
--     once as a whole rather than reason by reason;
--   6 the request is rebuilt, never written into: the table a fold handed the
--     filter comes out of it unchanged, which is what keeps a filter from
--     reaching the durable llm_request record;
--   7 the note goes in FRONT, where it cannot land among a tool_use / tool_result
--     pairing;
--   8 a tool that RETURNS its failure is invisible to the default and is what
--     `failed` is for. The default reads the kernel's `ok` flag, which is set
--     only when a handler raises, so a pair answering `{ ok = false, error =
--     ... }` closes `ok = true`. The first case below pins that gap rather
--     than papering over it, and the rest prove the predicate closes it.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")
local Outcome = kernel.Outcome

--- The requests this session actually sent, in order, read back out of the
--- durable record rather than captured in the stub: what the note has to reach
--- is the `llm_request` event, and that is where this looks.
local function sent(session)
    local out = {}
    for _, ev in ipairs(session:events()) do
        if ev.kind == "llm_request" then
            out[#out + 1] = ev.data.request
        end
    end
    return out
end

--- The first message's content as text, whatever shape it arrived in.
local function head(request)
    local content = request.messages[1].content
    if type(content) == "string" then
        return content
    end
    return tostring(content[1] and (content[1].text or content[1].content))
end

--- A device whose filter is a bound `carry`, over a queued llm.
local function device_for(session, opts, llm, tools)
    return kernel.device({
        llm = llm,
        tools = tools,
        filters = { policy.carry(opts)(session) },
    })
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.carry — binding", function()
    it("answers a binder, and the binder answers the filter", function()
        local bind = policy.carry({ max_bytes = 64 })
        expect(type(bind)).to.be("function")
        local filter = bind(support.session())
        expect(type(filter)).to.be("function")
    end)

    it("is session-free until it is bound: one policy, two sessions", function()
        local bind = policy.carry({})
        local a, b = support.session(), support.session()
        expect(bind(a) == bind(b)).to.be(false)
    end)

    it("refuses a session in the opts, and says why", function()
        local ok, err = pcall(policy.carry, { session = support.session() })
        expect(ok).to.be(false)
        expect(tostring(err):find("an argument", 1, true) ~= nil).to.be(true)
    end)

    it("refuses a binder argument that is not a session", function()
        local bind = policy.carry({})
        for _, not_a_session in ipairs({ 42, "session", { nothing = true } }) do
            expect(function()
                bind(not_a_session)
            end).to.fail()
        end
    end)

    it("insists on a whole number of bytes", function()
        for _, bad in ipairs({ 0, -1, 2.5 }) do
            expect(function()
                policy.carry({ max_bytes = bad })
            end).to.fail()
        end
        expect(function()
            policy.carry({ max_byte = 10 })
        end).to.fail()
    end)

    it("insists that `failed` is a function", function()
        -- Whichever mode this file is run in: the bound is written beside the
        -- factory, not left to the dev-mode gate. `api_spec` pins prod with no
        -- gate installed at all.
        for _, bad in ipairs({ 42, "always", { ok = false } }) do
            expect(function()
                policy.carry({ failed = bad })
            end).to.fail()
        end
    end)

    it("takes a well-formed `failed` beside max_bytes", function()
        local bind = policy.carry({
            max_bytes = 64,
            failed = function(_pair)
                return false
            end,
        })
        expect(type(bind(support.session()))).to.be("function")
    end)
end)

describe("policy.carry — what is carried", function()
    it("carries a tool that failed, naming the tool", function()
        local session = support.session()
        support.seed(session, "q")
        local device = device_for(
            session,
            { max_bytes = 256 },
            support.queue(support.calls("c1", "boom"), support.text("recovered")),
            support.failing_tool("boom", "the disk is gone")
        )

        expect(Outcome.is_ok(kernel.beat(session, device))).to.be(true)
        expect(Outcome.is_ok(kernel.beat(session, device))).to.be(true)

        local requests = sent(session)
        expect(#requests).to.be(2)
        -- The first beat had nothing to carry.
        expect(head(requests[1])).to.be("q")
        local note = head(requests[2])
        expect(note:find("the previous beat did not complete", 1, true) ~= nil).to.be(true)
        expect(note:find("boom", 1, true) ~= nil).to.be(true)
        expect(note:find("the disk is gone", 1, true) ~= nil).to.be(true)
    end)

    it("carries a call that did not come off, which the fold shows nothing of", function()
        -- `llm_call_failed` is skipped by knl.fold, so without this note the
        -- next beat's request is indistinguishable from one where the previous
        -- beat never happened.
        local session = support.session()
        support.seed(session, "q")
        local device = device_for(
            session,
            { max_bytes = 256 },
            support.queue(support.fails("the provider said no"), support.text("recovered"))
        )

        local first = kernel.beat(session, device)
        expect(Outcome.is_error(first)).to.be(true)
        expect(first.kind).to.be("call")
        expect(Outcome.is_ok(kernel.beat(session, device))).to.be(true)

        local note = head(sent(session)[2])
        expect(note:find("the model call did not come off", 1, true) ~= nil).to.be(true)
        expect(note:find("the provider said no", 1, true) ~= nil).to.be(true)
    end)

    it("does NOT carry a response the model truncated", function()
        -- A beat that hit the output ceiling came off: it recorded a response,
        -- `stop_reason` names the ceiling, and nothing failed. Carrying it
        -- would tell the model its own completed beat was a failure.
        local session = support.session()
        support.seed(session, "q")
        local truncated = support.answer({ { type = "text", text = "half an ans" } }, "max_tokens")
        local device = device_for(session, {}, support.queue(truncated, support.text("rest")))

        expect(Outcome.is_ok(kernel.beat(session, device))).to.be(true)
        expect(Outcome.is_ok(kernel.beat(session, device))).to.be(true)

        local second = sent(session)[2]
        expect(head(second)).to.be("q")
        expect(head(second):find("did not complete", 1, true)).to.be(nil)
    end)

    it("does NOT carry a refusal (a recorded response, not a failure)", function()
        local session = support.session()
        support.seed(session, "q")
        local refusal = {
            status = "refused",
            content = { { type = "text", text = "no" } },
            usage = support.usage(),
            stop_reason = "refusal",
            refusal = { kind = "model" },
        }
        local device = device_for(session, {}, support.queue(refusal, support.text("next")))

        expect(Outcome.is_refused(kernel.beat(session, device))).to.be(true)
        expect(Outcome.is_ok(kernel.beat(session, device))).to.be(true)
        expect(head(sent(session)[2])).to.be("q")
    end)

    it("hands a clean beat's request straight back, the same table", function()
        local session = support.session()
        support.seed(session, "q")
        local filter = policy.carry({})(session)
        local device = kernel.device({ llm = support.always(support.text("fine")) })
        expect(Outcome.is_ok(kernel.beat(session, device))).to.be(true)

        local request = { messages = { { role = "user", content = "q" } } }
        expect(filter(request)).to.be(request)
    end)

    it("carries nothing before the first beat", function()
        local session = support.session()
        support.seed(session, "q")
        local filter = policy.carry({})(session)
        local request = { messages = {} }
        expect(filter(request)).to.be(request)
    end)
end)

describe("policy.carry — the note itself", function()
    --- Run one failing beat and hand back the filter bound to that session.
    local function after_a_failure(opts, message)
        local session = support.session()
        support.seed(session, "q")
        local device =
            device_for(session, opts, support.queue(support.calls("c1", "boom")), support.failing_tool("boom", message))
        expect(Outcome.is_ok(kernel.beat(session, device))).to.be(true)
        return policy.carry(opts)(session), session
    end

    it("is bounded by max_bytes, marked where it was cut", function()
        local filter = after_a_failure({ max_bytes = 40 }, string.rep("x", 4000))
        local note = head(filter({ messages = {} }))
        expect(#note).to.be(40)
        expect(note:sub(-3)).to.be("...")
    end)

    it("is trimmed once, as a whole: a short note keeps every byte", function()
        local filter = after_a_failure({ max_bytes = 4000 }, "short")
        local note = head(filter({ messages = {} }))
        expect(#note < 4000).to.be(true)
        expect(note:sub(-3) == "...").to.be(false)
        expect(note:find("short", 1, true) ~= nil).to.be(true)
    end)

    it("goes in front, where it cannot land among a tool_use / tool_result pair", function()
        local filter = after_a_failure({ max_bytes = 256 }, "kaboom")
        local request = {
            messages = {
                { role = "assistant", content = { { type = "tool_use", id = "c9", name = "t" } } },
                { role = "user", content = { { type = "tool_result", tool_use_id = "c9", content = "R" } } },
            },
        }
        local out = filter(request)
        expect(#out.messages).to.be(3)
        expect(out.messages[1].role).to.be("user")
        expect(out.messages[1].content:find("did not complete", 1, true) ~= nil).to.be(true)
        -- the pair is still adjacent and still in order
        expect(out.messages[2].content[1].type).to.be("tool_use")
        expect(out.messages[3].content[1].tool_use_id).to.be("c9")
    end)

    it("rebuilds the request instead of writing into it", function()
        local filter = after_a_failure({ max_bytes = 256 }, "kaboom")
        local messages = { { role = "user", content = "q" } }
        local request = { messages = messages, system = "SYS" }
        local out = filter(request)

        expect(out == request).to.be(false)
        expect(out.messages == messages).to.be(false)
        -- the caller's tables are exactly as they were handed over
        expect(#messages).to.be(1)
        expect(request.messages).to.be(messages)
        -- and everything the fold put on the request rides through
        expect(out.system).to.be("SYS")
        expect(out.messages[2]).to.be(messages[1])
        expect(getmetatable(out.messages).__jsontype).to.be("array")
    end)
end)

describe("policy.carry — a tool that RETURNS its failure", function()
    --- The two events one answered tool call leaves behind, appended the way a
    --- beat appends them: the call first, then the result it produced.
    ---
    --- Written rather than run, because what is under test is a pair the
    --- kernel closed `ok = true` — a handler that answered a rejection instead
    --- of raising one, which no stub tool can produce by failing.
    local function answered(session, name, result, ok)
        local data = { call_id = "c1", name = name, args = { path = "a.txt", line = 300 } }
        session:append({ kind = "tool_call", beat = "b-1", data = data })
        session:append({ kind = "tool_result", beat = "b-1", data = { call_id = "c1", ok = ok, result = result } })
    end

    --- What an edit tool answers when the model asks to change a line that is
    --- not in the file: a value, not a raise.
    local function rejection(reason)
        return { ok = false, error = reason }
    end

    --- The predicate for a tool that reports failure in its own result.
    local function returned_failure(pair)
        return pair.result and pair.result.ok == false
    end

    --- One such beat, and the filter bound over the session that holds it.
    local function after_a_rejection(opts, reason)
        local session = support.session()
        support.seed(session, "q")
        answered(session, "edit", rejection(reason or "there is no line 300"), true)
        return policy.carry(opts)(session), session
    end

    it("is invisible to the default: the pair closed ok = true", function()
        -- The gap, pinned. The kernel sets `ok` from whether the handler
        -- RAISED, and this one did not, so the default finds no failure and
        -- hands the request straight back — the same table.
        local filter = after_a_rejection({})
        local request = { messages = {} }
        expect(filter(request)).to.be(request)
    end)

    it("is carried once `failed` reads the result, and the note holds the tool's own error", function()
        local filter = after_a_rejection({ max_bytes = 256, failed = returned_failure })
        local note = head(filter({ messages = {} }))
        expect(note:find("the previous beat did not complete", 1, true) ~= nil).to.be(true)
        expect(note:find("edit", 1, true) ~= nil).to.be(true)
        expect(note:find("there is no line 300", 1, true) ~= nil).to.be(true)
    end)

    it("is bounded by max_bytes like any other note, and marked where it was cut", function()
        local filter = after_a_rejection({ max_bytes = 40, failed = returned_failure }, string.rep("x", 4000))
        local note = head(filter({ messages = {} }))
        expect(#note).to.be(40)
        expect(note:sub(-3)).to.be("...")
    end)

    it("hands the predicate the call's name and input beside the result", function()
        local seen
        local filter = after_a_rejection({
            failed = function(pair)
                seen = pair
                return false
            end,
        })
        filter({ messages = {} })
        expect(seen.name).to.be("edit")
        expect(seen.input.line).to.be(300)
        expect(seen.call_id).to.be("c1")
        expect(seen.beat).to.be("b-1")
        expect(seen.ok).to.be(true)
        expect(seen.result.error).to.be("there is no line 300")
    end)

    it("decides for every tool pair, the ones the kernel closed ok = false included", function()
        -- A predicate reading a RETURNED failure says nothing about a raised
        -- one — the result is the raise's message, a string with no `ok` — so
        -- this pair is not carried. `failed` replaces the default's judgement
        -- rather than being asked in addition to it, and a caller that wants
        -- both writes both into the one predicate.
        local session = support.session()
        support.seed(session, "q")
        answered(session, "edit", "the disk is gone", false)
        local filter = policy.carry({ failed = returned_failure })(session)
        local request = { messages = {} }
        expect(filter(request)).to.be(request)
    end)

    it("carries a call that did not come off whatever the predicate answers", function()
        -- `llm_call_failed` is not a tool pair: there is no result for a
        -- predicate to read, and the fold shows nothing of the event at all.
        local session = support.session()
        support.seed(session, "q")
        session:append({ kind = "llm_call_failed", beat = "b-1", data = { error = "the provider said no" } })
        local filter = policy.carry({
            failed = function(_pair)
                return false
            end,
        })(session)
        local note = head(filter({ messages = {} }))
        expect(note:find("the model call did not come off", 1, true) ~= nil).to.be(true)
        expect(note:find("the provider said no", 1, true) ~= nil).to.be(true)
    end)
end)
