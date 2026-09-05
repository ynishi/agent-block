-- device_spec.lua — mlua-lspec unit tests for the session / device split
-- (state and policy are two arguments, not one bundle).
--
-- Run via:
--   test_launch(code_file=".../knl/spec/device_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("knl") resolves
--
-- What this proves (the pure-VM half):
--   1 knl.device resolves its defaults once (fold / filters / cost), checks
--     types at construction, and rejects unknown keys loudly (no silent
--     policy typos). knl.open takes state keys only — a policy key is an
--     unknown option now, not a shorthand.
--   2 a device is frozen: assignment raises, and the tools map is copied, so
--     writing to the caller's table afterwards cannot reach the device.
--   3 d:with derives a new device from d's resolved fields; d is untouched;
--     a state key in the delta raises.
--   4 knl.beat(session, device) is the whole primitive: gate, beat id, fold,
--     filter, reserve, record, call, record, tools — and every event one beat
--     writes carries that beat's id, a string the kernel neither mints nor
--     numbers.
--   5 a refused reservation is `stopped`, the fourth status: no call, no
--     record, and the grant's tag names what stopped it.
--   6 knl.session(opts, fn) is the bracket: it closes on the way out either
--     way — "scope_exit" on a clean exit (the one word for leaving a scope,
--     whichever form wrote it) and "error" on a failing one — a body error
--     wins over the close, and a close that fails on that path is warned
--     rather than raised or swallowed (the suppressed-exception rule).
--   7 tool_policy's decision vocabulary is nil / "run" / "deny" and nothing
--     else; a raise denies (fail-closed) and a fourth value stops the beat
--     with err("conf") before any tool runs.
--   8 the shapes M.shapes publishes cover every public interface — the
--     `data` shape of every kind a beat writes among them (the kernel
--     validates the envelope and its own kinds, the
--     writer owns the rest), plus the two envelope rules this layer mirrors
--     so they fail at the append instead of at the syscall (`beat` is a
--     string, `meta` is shallow).
--   9 a syscall failure is reported as the kernel's own reading of it
--     (`Outcome.err("state").detail` = { kind?, method?, retryable,
--     message }) rather than as a sentence, and beat never acts on
--     `retryable` itself. A raise out of the CALLER's code (fold, a filter,
--     cost, the llm) is the other kind of failure: its detail is the
--     message, plus a traceback in dev mode only.
--
-- The Rust `knl` syscall bridge is not present in the pure lspec runner, so a
-- faithful Lua fake stands in below (mirroring bridge/knl.rs facts: an append
-- records, the budget does not move, and beats are NOT numbered; `reserve` is
-- the one decision point — it deducts or refuses with the grant's tag; new_beat_id
-- mints a fresh id per call; close takes a reason and is idempotent; and
-- `knl.error` reads an attributed raise — `knl: <method>: <kind>: <message>`
-- — back into a table, since a Rust callback cannot raise one).

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Fake `knl` bridge (installed as a global BEFORE require("knl"), which is
-- what the module captures as its syscall layer at load time).
-- ─────────────────────────────────────────────────────────────────────────────

local minted = 0
local opened = 0

local function fake_session(opts)
    opts = opts or {}
    local grant = opts.budget or {}
    opened = opened + 1
    local id = string.format("sess-%06d", opened)
    local s = {
        _events = {},
        -- One entry per `query` call: { sql, params, opts }.
        _queries = {},
        -- What every `query` answers, until a case puts rows here.
        _query_rows = {},
        _seq = 0,
        _remaining = grant.amount,
        _tag = grant.tag,
        _owner = opts.owner or "anon",
        closed = false,
        close_reason = nil,
    }
    -- Identity, as the bridge answers it: three methods, not three fields.
    -- The fake carries the WHOLE session surface because `knl.beat` asks for
    -- the whole surface before it will treat a value as a session — a fake
    -- that answered only what a beat calls would be a stand-in for something
    -- the kernel does not hand out.
    function s:id()
        return id
    end
    function s:scope_id()
        return "scope-" .. id
    end
    function s:owner()
        return self._owner
    end
    -- An append records and the budget does not move. The kernel stamps seq
    -- and leaves every other field exactly as written — `beat` included, which
    -- is the caller's to declare and never the kernel's to assign.
    function s:append(ev)
        assert(not self.closed, "knl: append: session is closed")
        assert(type(ev) == "table" and type(ev.kind) == "string", "knl: append: kind is required")
        self._seq = self._seq + 1
        ev.seq = self._seq
        self._events[#self._events + 1] = ev
        return ev.seq
    end
    function s:events()
        return self._events
    end
    function s:len()
        return #self._events
    end
    -- The one named fold. Nothing here folds anything: what `tail` answers is
    -- the kernel's, and the fake carries the method because the surface has
    -- it — a stand-in missing one is not a session.
    function s:view(_name, _opts)
        error("knl: view: validation: unknown view")
    end
    -- The SQL read. No SQLite stands behind this: the
    -- fake records the call and answers whatever the case queued, which is
    -- what makes a view's CONTRACT testable here — that it runs one
    -- statement, that the statement names `$sessions`, that the caller's
    -- opts reach the read unaltered, and how the rows are read back. What
    -- the SQL actually selects is a question only a database can answer, and
    -- it is asked where there is one (tests/fixtures/knl_beat_test.lua,
    -- inv11).
    function s:query(sql, params, opts)
        assert(not self.closed, "knl: query: session is closed")
        assert(type(sql) == "string", "knl: query: sql must be a string")
        self._queries[#self._queries + 1] = { sql = sql, params = params, opts = opts }
        return self._query_rows, false
    end
    -- The quota, asked before the spending: it deducts and answers true, or
    -- refuses with the grant's tag and leaves the balance where it was.
    function s:reserve(n)
        assert(not self.closed, "knl: reserve: session is closed")
        if self._remaining == nil then
            return true
        end
        if self._remaining < n then
            return false, self._tag
        end
        self._remaining = self._remaining - n
        return true
    end
    -- The write IS the result: spend answers nothing, and the balance is
    -- read with `remaining()` (the kernel's surface — the fake must not
    -- promise a return the bridge does not make).
    function s:spend(n)
        if self._remaining == nil then
            return
        end
        self._remaining = math.max(0, self._remaining - n)
    end
    function s:exhausted()
        return self._remaining ~= nil and self._remaining <= 0
    end
    function s:remaining()
        return self._remaining
    end
    function s:close(reason)
        if not self.closed then
            self.closed = true
            self.close_reason = reason or "closed"
        end
    end
    return s
end

-- The classes the kernel publishes (Rust `KnlError::KINDS`). Retyped here
-- on purpose: this table stands in for the bridge, and a stand-in that
-- borrowed the module's own list could not catch the module reading it
-- wrong. The two are held against each other where a real bridge exists
-- (tests/fixtures/knl_beat_test.lua, inv10).
local FAKE_ERROR_KINDS = {
    busy = true,
    storage = true,
    corruption = true,
    closed = true,
    validation = true,
    unsupported = true,
}

--- `knl.error` as the bridge implements it (bridge/knl.rs `error_table`):
--- the raise is text, `knl: <method>: <kind>: <message>`, because mlua
--- cannot carry a table out of a Rust callback. Only a class the kernel
--- publishes is read as one; anything else comes back whole and
--- unclassified, and the table renders as the message it was read from.
local function fake_read_error(raised)
    local text = tostring(raised)
    local out = { message = text, retryable = false }
    for line in text:gmatch("[^\n]+") do
        local attributed = line:match("knl: (.+)$")
        if attributed then
            -- Non-greedy: the first two separators only, like Rust's two
            -- `split_once(": ")` — the message keeps its own colons.
            local method, kind, message = attributed:match("^(.-): (.-): (.*)$")
            if kind ~= nil and FAKE_ERROR_KINDS[kind] then
                out.method, out.kind, out.message = method, kind, message
                out.retryable = kind == "busy"
            end
            break
        end
    end
    return setmetatable(out, {
        __tostring = function()
            return text
        end,
    })
end

knl = {
    open = function(o)
        return fake_session(o)
    end,
    error = fake_read_error,
    resume = function(o)
        local s = fake_session(o)
        s.resumed_from = o and o.session
        return s
    end,
    -- Time-ordered and session-free, like the Rust UUID v7 mint.
    new_beat_id = function()
        minted = minted + 1
        return string.format("beat-%06d", minted)
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

-- A stub that answers with one tool_use first, then a plain answer.
local function llm_with_tool(name, id)
    local sent = false
    return function(_req)
        if sent then
            return {
                status = "ok",
                content = { { type = "text", text = "done" } },
                usage = {},
                stop_reason = "end_turn",
            }
        end
        sent = true
        return {
            status = "ok",
            content = { { type = "tool_use", id = id or "c1", name = name, input = { s = "hi" } } },
            usage = {},
            stop_reason = "tool_use",
        }
    end
end

local function echo_tools()
    return {
        echo = {
            description = "echo",
            input_schema = { type = "object" },
            handler = function(args)
                return args.s
            end,
        },
    }
end

-- The `beat` field of every event that carries one, in seq order.
local function stamped_beats(s)
    local ids = {}
    for _, ev in ipairs(s:events()) do
        if ev.beat ~= nil then
            ids[#ids + 1] = ev.beat
        end
    end
    return ids
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("knl.device — construction and resolution", function()
    it("resolves the defaults once (fold / filters / cost)", function()
        local d = K.device({})
        expect(d.fold).to.be(K.fold)
        expect(type(d.filters)).to.be("table")
        expect(#d.filters).to.be(0)
        expect(type(d.cost)).to.be("function")
        expect(d.cost({})).to.be(1)
        -- no default is invented for the rest
        expect(d.llm).to.be(nil)
        expect(d.tools).to.be(nil)
        expect(d.tool_policy).to.be(nil)
        expect(d.system).to.be(nil)
    end)

    it("keeps the fields it was given", function()
        local llm, fold = stub_llm("hi"), function()
            return { messages = {} }
        end
        local d = K.device({ llm = llm, fold = fold, system = "be terse" })
        expect(d.llm).to.be(llm)
        expect(d.fold).to.be(fold)
        expect(d.system).to.be("be terse")
    end)

    it("rejects unknown keys loudly", function()
        expect(function()
            K.device({ llm = stub_llm("x"), inptu = "typo" })
        end).to.fail()
        -- state keys are a different constructor's, not a device's
        expect(function()
            K.device({ owner = "u" })
        end).to.fail()
        expect(function()
            K.device({ budget = { amount = 1 } })
        end).to.fail()
    end)

    it("checks types at construction", function()
        expect(function()
            K.device({ filters = "not-a-table" })
        end).to.fail()
        expect(function()
            K.device({ filters = { "not-a-function" } })
        end).to.fail()
        expect(function()
            K.device({ tool_policy = "not-a-function" })
        end).to.fail()
        expect(function()
            K.device({ cost = 3 })
        end).to.fail()
        expect(function()
            K.device({ fold = "not-a-function" })
        end).to.fail()
        expect(function()
            K.device({ llm = "not-callable" })
        end).to.fail()
    end)

    it("takes tools as a map only (an array is the adapter's job)", function()
        expect(function()
            K.device({ tools = { { name = "flat", handler = function() end } } })
        end).to.fail()
        local d = K.device({ tools = echo_tools() })
        expect(type(d.tools.echo.handler)).to.be("function")
    end)
end)

describe("knl.device — immutability", function()
    it("assignment raises (__newindex guard), on a set field too", function()
        local d = K.device({ llm = stub_llm("x") })
        expect(function()
            d.llm = stub_llm("mutated")
        end).to.fail()
        expect(function()
            d.system = "sneak"
        end).to.fail()
    end)

    it("copies the tools map: writing to the caller's table afterwards does nothing", function()
        local mine = echo_tools()
        local d = K.device({ tools = mine })
        mine.echo = nil
        mine.added = { handler = function() end }
        expect(d.tools.echo).to.exist()
        expect(d.tools.added).to.be(nil)
    end)

    it("the tools map itself is frozen", function()
        local d = K.device({ tools = echo_tools() })
        expect(function()
            d.tools.injected = { handler = function() end }
        end).to.fail()
        expect(function()
            d.tools.echo = { handler = function() end }
        end).to.fail()
    end)

    it("copies the filters array", function()
        local mine = {
            function(req)
                return req
            end,
        }
        local d = K.device({ filters = mine })
        mine[2] = function() end
        expect(#d.filters).to.be(1)
    end)
end)

describe("device:with — derivation", function()
    it("derives a new device; the original is untouched", function()
        local weak, strong = stub_llm("weak"), stub_llm("strong")
        local d = K.device({ llm = weak, system = "base", tools = echo_tools() })
        local d2 = d:with({ llm = strong })
        expect(d2.llm).to.be(strong)
        expect(d2.system).to.be("base") -- inherited
        expect(d2.tools.echo).to.exist() -- inherited, re-frozen
        expect(d.llm).to.be(weak) -- original intact
        expect(d2 == d).to.be(false)
    end)

    it("the derived device is frozen too", function()
        local d = K.device({ llm = stub_llm("x") }):with({ system = "s" })
        expect(function()
            d.system = "t"
        end).to.fail()
    end)

    it("rejects state keys and typos in the delta", function()
        local d = K.device({ llm = stub_llm("x") })
        expect(function()
            d:with({ owner = "other" })
        end).to.fail()
        expect(function()
            d:with({ store = { sqlite = "p" } })
        end).to.fail()
        expect(function()
            d:with({ session = "s" })
        end).to.fail()
        expect(function()
            d:with({ typo = 1 })
        end).to.fail()
    end)

    it("re-checks types on the merged result", function()
        local d = K.device({ llm = stub_llm("x") })
        expect(function()
            d:with({ tool_policy = "not-a-function" })
        end).to.fail()
    end)
end)

describe("knl.open / knl.resume — state only", function()
    it("returns the kernel session itself (no Lua wrapper)", function()
        local s = K.open({ owner = "spec" })
        expect(s:owner()).to.be("spec")
        s:append({ kind = "msg_user", data = { content = "hello" } })
        local evs = s:events()
        expect(#evs).to.be(1)
        expect(evs[1].seq).to.be(1) -- kernel-owned stamp
    end)

    it("refuses policy keys and typos", function()
        expect(function()
            K.open({ llm = stub_llm("x") })
        end).to.fail()
        expect(function()
            K.open({ tools = echo_tools() })
        end).to.fail()
        expect(function()
            K.open({ inptu = "typo" })
        end).to.fail()
    end)

    it("resume takes store / session / budget", function()
        local s = K.resume({ store = { sqlite = "p.db" }, session = "sess-1" })
        expect(s.resumed_from).to.be("sess-1")
        expect(function()
            K.resume({ store = { sqlite = "p.db" }, session = "s", llm = stub_llm("x") })
        end).to.fail()
    end)
end)

describe("knl.beat — the primitive", function()
    it("errors on a non-session first argument", function()
        local o = K.beat({ nothing = true }, K.device({ llm = stub_llm("x") }))
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("conf")
    end)

    it("errors on a second argument that is not a device", function()
        local s = K.open({})
        expect(Outcome.is_error(K.beat(s, { llm = stub_llm("x") }))).to.be(true)
        expect(Outcome.is_error(K.beat(s, nil))).to.be(true)
    end)

    it("errors without an llm in the device", function()
        local o = K.beat(K.open({ owner = "spec" }), K.device({}))
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("conf")
    end)

    it("one beat: records llm_request + llm_response under one declared id", function()
        local s = K.open({})
        local d = K.device({ llm = stub_llm("answer") })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(type(o.out.beat)).to.be("string")

        local evs = s:events()
        expect(evs[1].kind).to.be("msg_user")
        expect(evs[1].beat).to.be(nil) -- the caller's seed is not part of a beat
        expect(evs[2].kind).to.be("llm_request")
        expect(evs[3].kind).to.be("llm_response")
        expect(evs[2].beat).to.be(o.out.beat)
        expect(evs[3].beat).to.be(o.out.beat)
    end)

    it("stamps every event of one beat — model response and tool pair — with the same id", function()
        local s = K.open({})
        local d = K.device({ llm = llm_with_tool("echo"), tools = echo_tools() })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)
        expect(Outcome.is_ok(o)).to.be(true)

        local kinds = {}
        for _, ev in ipairs(s:events()) do
            kinds[#kinds + 1] = ev.kind
        end
        expect(table.concat(kinds, ",")).to.be("msg_user,llm_request,llm_response,tool_call,tool_result")

        local ids = stamped_beats(s)
        expect(#ids).to.be(4)
        for _, id in ipairs(ids) do
            expect(id).to.be(o.out.beat)
        end
        expect(o.out.tools[1].ok).to.be(true)
    end)

    it("two beats declare two different ids (no numbering, no read-back)", function()
        local s = K.open({})
        local d = K.device({ llm = stub_llm("x") })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local first = K.beat(s, d)
        local second = K.beat(s, d)
        expect(Outcome.is_ok(first)).to.be(true)
        expect(Outcome.is_ok(second)).to.be(true)
        expect(type(first.out.beat)).to.be("string")
        expect(first.out.beat == second.out.beat).to.be(false)
        -- the session was never asked to count anything
        expect(s.beats).to.be(nil)
    end)

    it("stops (the fourth status) when the reservation is refused", function()
        -- One unit granted, one unit per beat: the second beat is refused.
        local s = K.open({ budget = { amount = 1, tag = "beats" } })
        local d = K.device({ llm = stub_llm("x") })
        s:append({ kind = "msg_user", data = { content = "q" } })
        expect(Outcome.is_ok(K.beat(s, d))).to.be(true)
        expect(s:remaining()).to.be(0) -- the beat reserved it; the appends did not

        local before = #s:events()
        local second = K.beat(s, d)
        expect(Outcome.is_stopped(second)).to.be(true)
        expect(Outcome.is_ok(second)).to.be(false)
        expect(second.reason).to.be("budget")
        expect(second.tag).to.be("beats") -- which allowance stopped it
        -- A refused beat records nothing and calls nobody.
        expect(#s:events()).to.be(before)
    end)

    it("asks device.cost how much one beat costs", function()
        local asked = {}
        local s = K.open({ budget = { amount = 10, tag = "beats" } })
        local d = K.device({
            llm = stub_llm("x"),
            cost = function(request)
                asked[#asked + 1] = #request.messages
                return 4
            end,
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        expect(Outcome.is_ok(K.beat(s, d))).to.be(true)
        expect(s:remaining()).to.be(6)
        expect(asked[1]).to.be(1) -- the policy saw the request, not the events
    end)

    it("a cost policy that answers 0 is a conf error (no ranking function)", function()
        local s = K.open({ budget = { amount = 10, tag = "beats" } })
        local d = K.device({
            llm = stub_llm("x"),
            cost = function()
                return 0
            end,
        })
        local o = K.beat(s, d)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("conf")
        expect(s:remaining()).to.be(10)
    end)

    it("escalation: beat(s, d:with{llm=strong}) uses strong for that beat only", function()
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
        local s = K.open({})
        local d = K.device({ llm = tagged("weak") })
        s:append({ kind = "msg_user", data = { content = "q" } })
        K.beat(s, d)
        K.beat(s, d:with({ llm = tagged("strong") }))
        K.beat(s, d)
        expect(called[1]).to.be("weak")
        expect(called[2]).to.be("strong")
        expect(called[3]).to.be("weak") -- the original device is untouched
    end)

    it("one device drives two sessions independently", function()
        local d = K.device({ llm = stub_llm("shared") })
        local a, b = K.open({}), K.open({})
        expect(Outcome.is_ok(K.beat(a, d))).to.be(true)
        expect(#a:events()).to.be(2)
        expect(#b:events()).to.be(0)
    end)
end)

describe("knl.session — the canonical bracket", function()
    it("opens, runs the body, and closes with scope_exit on the way out", function()
        local seen
        local first, second = K.session({ owner = "spec" }, function(s)
            seen = s
            s:append({ kind = "msg_user", data = { content = "q" } })
            return "one", 2
        end)
        expect(first).to.be("one")
        expect(second).to.be(2)
        expect(seen:owner()).to.be("spec")
        expect(seen.closed).to.be(true)
        expect(seen.close_reason).to.be("scope_exit")
    end)

    it("closes on a body error and re-raises the body's error", function()
        local seen
        local ok, err = pcall(K.session, { owner = "spec" }, function(s)
            seen = s
            error("body exploded")
        end)
        expect(ok).to.be(false)
        expect(tostring(err):find("body exploded", 1, true) ~= nil).to.be(true)
        expect(seen.closed).to.be(true)
        expect(seen.close_reason).to.be("error")
    end)

    it("resumes instead of opening when the opts name a session", function()
        local resumed
        K.session({ store = { sqlite = "p.db" }, session = "sess-7" }, function(s)
            resumed = s.resumed_from
        end)
        expect(resumed).to.be("sess-7")
    end)

    it("nests: the inner session is its own", function()
        local outer_id, inner_id
        K.session({ owner = "outer" }, function(o)
            outer_id = o
            K.session({ owner = "inner" }, function(i)
                inner_id = i
                i:append({ kind = "msg_user", data = { content = "in" } })
            end)
            expect(#o:events()).to.be(0)
        end)
        expect(outer_id == inner_id).to.be(false)
        expect(inner_id.closed).to.be(true)
        expect(outer_id.closed).to.be(true)
    end)

    it("beats inside the bracket, the loop written by the caller", function()
        local s
        local out = K.session({ owner = "spec", budget = { amount = 5, tag = "beats" } }, function(session)
            s = session
            local d = K.device({ llm = stub_llm("hi") })
            session:append({ kind = "msg_user", data = { content = "q" } })
            return K.beat(session, d)
        end)
        expect(Outcome.is_ok(out)).to.be(true)
        expect(s.closed).to.be(true)
        expect(s:remaining()).to.be(4)
    end)

    it("requires a function body", function()
        expect(function()
            K.session({}, "not a function")
        end).to.fail()
    end)

    -- The body's error is the winner. A close that fails on the way
    -- out is bookkeeping about a failure — it must not replace it, and it
    -- must not vanish either, so it goes to the host log when there is one.
    it("a close that fails on the error path is warned as a record, not raised", function()
        local warned
        log = {
            warn = function(msg)
                warned = msg
            end,
        }
        local ok, err = pcall(K.session, { owner = "spec" }, function(s)
            s.close = function()
                error("knl: close: storage: disk is gone")
            end
            error("body exploded")
        end)
        log = nil
        expect(ok).to.be(false)
        -- The body error wins and propagates unchanged; the close failure
        -- is the suppressed one, and it is structured so the loser is still
        -- readable: which of the two this is, the winner as text, and the
        -- kernel's own reading of the close.
        expect(tostring(err):find("body exploded", 1, true) ~= nil).to.be(true)
        expect(type(warned)).to.be("table")
        expect(warned.event).to.be("close_failed_after_body_error")
        expect(warned.body:find("body exploded", 1, true) ~= nil).to.be(true)
        expect(warned.close.kind).to.be("storage")
        expect(warned.close.method).to.be("close")
        expect(warned.close.retryable).to.be(false)
        expect(warned.close.message).to.be("disk is gone")
    end)

    it("falls back to text for a log that only takes strings", function()
        -- The host's `log.warn` is typed `msg: String` (bridge/log.rs), so
        -- the record is offered first and a sentence carrying the same
        -- three parts is what actually lands there. A warning about a
        -- failure must not become a second failure, so both are guarded.
        local seen = {}
        log = {
            warn = function(msg)
                seen[#seen + 1] = msg
                if type(msg) ~= "string" then
                    error("log.warn: msg must be a string")
                end
            end,
        }
        local ok = pcall(K.session, { owner = "spec" }, function(s)
            s.close = function()
                error("knl: close: storage: disk is gone")
            end
            error("body exploded")
        end)
        log = nil
        expect(ok).to.be(false)
        expect(#seen).to.be(2)
        expect(type(seen[1])).to.be("table")
        expect(type(seen[2])).to.be("string")
        expect(seen[2]:find("close_failed_after_body_error", 1, true) ~= nil).to.be(true)
        expect(seen[2]:find("body exploded", 1, true) ~= nil).to.be(true)
        expect(seen[2]:find("disk is gone", 1, true) ~= nil).to.be(true)
    end)

    it("a failing close needs no log global (it never raises)", function()
        local ok, err = pcall(K.session, { owner = "spec" }, function(s)
            s.close = function()
                error("close blew up")
            end
            error("body exploded")
        end)
        expect(ok).to.be(false)
        expect(tostring(err):find("body exploded", 1, true) ~= nil).to.be(true)
    end)
end)

describe("knl shapes — data contracts, asserted in dev mode", function()
    local lshape = require("lshape")
    local shape = lshape.check

    -- The mode is a property of the case, not of the environment: these
    -- specs run both under the pure lspec runner (which sets LSHAPE_CHECK=1)
    -- and under a bare test_launch (which does not), and a case about
    -- dev-mode behaviour has to say which one it means.
    local function with_dev_mode(on, fn)
        local saved = shape.is_dev_mode
        shape.is_dev_mode = function()
            return on
        end
        local ok, err = pcall(fn)
        shape.is_dev_mode = saved
        if not ok then
            error(err, 0)
        end
    end

    local function in_dev(fn)
        return with_dev_mode(true, fn)
    end

    local function in_prod(fn)
        return with_dev_mode(false, fn)
    end

    it("publishes a shape for every public contract", function()
        for _, name in ipairs({
            "outcome",
            "request",
            "event_base",
            "event_meta",
            "events",
            "device_config",
            "tool_entry",
            "tool_policy_decision",
            "cost_result",
            "llm_result",
            "llm_usage",
            "tool_use_block",
            "open_opts",
            "resume_opts",
            "budget_grant",
            "error",
        }) do
            expect(K.shapes[name]).to.exist()
        end
    end)

    it("a syscall failure validates against the error shape, classified or not", function()
        expect(shape.check({ kind = "busy", method = "reserve", retryable = true, message = "locked" }, K.shapes.error)).to.be(
            true
        )
        -- The unattributed form: no class, no method, the whole text.
        expect(shape.check({ retryable = false, message = "something else" }, K.shapes.error)).to.be(true)
        -- A class outside the kernel's list is not one.
        expect(shape.check({ kind = "nonsense", retryable = false, message = "x" }, K.shapes.error)).to.be(false)
        -- `message` and `retryable` are what a reader can always count on.
        expect(shape.check({ retryable = false }, K.shapes.error)).to.be(false)
        expect(shape.check({ message = "x" }, K.shapes.error)).to.be(false)
    end)

    it("dev mode: a raise out of the caller's own code carries its traceback", function()
        -- fold / filters / cost / the llm are the device's code, so a raise
        -- there is a bug to locate, not a class of kernel failure. The
        -- stack is a development aid: dev mode only, and prod's detail is
        -- exactly the sentence it has always been.
        local function beat_with_a_broken_fold()
            local s = K.open({})
            return K.beat(
                s,
                K.device({
                    llm = stub_llm("x"),
                    fold = function()
                        error("fold blew up")
                    end,
                })
            )
        end

        in_dev(function()
            local o = beat_with_a_broken_fold()
            expect(o.kind).to.be("conf")
            expect(type(o.detail)).to.be("table")
            expect(o.detail.message:find("fold blew up", 1, true) ~= nil).to.be(true)
            -- The stack itself needs the `debug` library, which mlua's safe
            -- stdlib leaves out; dev mode still means one thing (a
            -- structured detail) and the field is simply unfillable there.
            if type(debug) == "table" and type(debug.traceback) == "function" then
                expect(type(o.detail.traceback)).to.be("string")
                expect(o.detail.traceback:find("traceback", 1, true) ~= nil).to.be(true)
            else
                expect(o.detail.traceback).to.be(nil)
            end
        end)

        in_prod(function()
            local o = beat_with_a_broken_fold()
            expect(o.kind).to.be("conf")
            expect(type(o.detail)).to.be("string")
            expect(o.detail:find("fold blew up", 1, true) ~= nil).to.be(true)
        end)
    end)

    it("owns the `data` shape of every kind it writes (one SoT, on this side)", function()
        -- The kernel validates the envelope and the `data` of its OWN kinds
        -- (session_* / budget_*) and stopped judging these, so there is one
        -- declaration of them and it is here.
        for _, kind in ipairs({
            "msg_user",
            "llm_request",
            "llm_response",
            "llm_call_failed",
            "tool_call",
            "tool_result",
        }) do
            expect(K.shapes.events[kind]).to.exist()
        end
        -- and each is closed: a stray key inside `data` is a column a view
        -- would eventually select, so it fails where it was written
        expect(shape.check({ call_id = "c1", ok = true, result = "R" }, K.shapes.events.tool_result)).to.be(true)
        expect(shape.check({ call_id = "c1", ok = true, result = "R", extra = 1 }, K.shapes.events.tool_result)).to.be(
            false
        )
        expect(shape.check({ call_id = "c1", ok = "yes", result = "R" }, K.shapes.events.tool_result)).to.be(false)
    end)

    it("holds the envelope to the rules this layer mirrors", function()
        -- `beat` is an opaque string, `meta` is SHALLOW (labels, not a
        -- second `data`), and the kernel refuses the same two at the
        -- syscall — this is the copy that fails at the line that wrote it.
        expect(shape.check({ kind = "llm_response" }, K.shapes.event_base)).to.be(true)
        expect(shape.check({ kind = "llm_response", beat = 42 }, K.shapes.event_base)).to.be(false)
        expect(shape.check({ kind = "msg_user", meta = { label = "seed", n = 1, on = true } }, K.shapes.event_base)).to.be(
            true
        )
        expect(shape.check({ kind = "msg_user", meta = { label = { deep = 1 } } }, K.shapes.event_base)).to.be(false)
    end)

    it("a beat's Outcome validates against the outcome shape", function()
        local s = K.open({})
        local d = K.device({ llm = stub_llm("shaped") })
        s:append({ kind = "msg_user", data = { content = "q" } })
        expect(shape.check(K.beat(s, d), K.shapes.outcome)).to.be(true)
    end)

    it("a stopped Outcome validates too (the fourth variant)", function()
        local stopped = Outcome.stopped("budget", "tokens")
        expect(shape.check(stopped, K.shapes.outcome)).to.be(true)
        expect(shape.check(Outcome.stopped("budget"), K.shapes.outcome)).to.be(true)
    end)

    it("dev mode: the beat stamp is checked on the events beat writes", function()
        in_dev(function()
            local s = K.open({})
            local d = K.device({ tools = echo_tools() })
            local out = { content = { { type = "tool_use", id = "c1", name = "echo", input = {} } } }
            -- a numeric beat id: `beat` is an opaque STRING, and this is
            -- the one thing this layer adds to the kernel's own validator
            expect(function()
                K._execute_tools(s, d, out, 42)
            end).to.fail()
            -- the same call with a declared id passes
            K._execute_tools(s, d, out, "beat-x")
        end)
    end)

    it("dev mode: a filter that breaks the request shape fails loud", function()
        in_dev(function()
            local s = K.open({})
            local d = K.device({
                llm = stub_llm("x"),
                filters = {
                    function(_req)
                        return { messages = "not-an-array" }
                    end,
                },
            })
            s:append({ kind = "msg_user", data = { content = "q" } })
            expect(function()
                K.beat(s, d)
            end).to.fail()
        end)
    end)

    it("prod mode: the same malformed stamp passes through (no-op gate)", function()
        in_prod(function()
            local s = K.open({})
            local d = K.device({ tools = echo_tools() })
            local out = { content = { { type = "tool_use", id = "c1", name = "echo", input = {} } } }
            K._execute_tools(s, d, out, 42) -- no raise
            expect(#s:events()).to.be(2)
        end)
    end)
end)

describe("Outcome — four statuses", function()
    it("match is exhaustive over all four arms", function()
        local arms = {
            ok = function()
                return "O"
            end,
            refused = function()
                return "R"
            end,
            error = function()
                return "E"
            end,
            stopped = function(o)
                return "S:" .. o.reason
            end,
        }
        expect(Outcome.match(Outcome.stopped("budget", "tokens"), arms)).to.be("S:budget")
        expect(Outcome.match(Outcome.ok({}), arms)).to.be("O")
        -- a missing arm is loud, and a three-armed match no longer suffices
        expect(function()
            Outcome.match(Outcome.ok({}), {
                ok = arms.ok,
                refused = arms.refused,
                error = arms.error,
            })
        end).to.fail()
    end)

    it("the predicates do not overlap", function()
        local stopped = Outcome.stopped("budget")
        expect(Outcome.is_stopped(stopped)).to.be(true)
        expect(Outcome.is_ok(stopped)).to.be(false)
        expect(Outcome.is_error(stopped)).to.be(false)
        expect(Outcome.is_refused(stopped)).to.be(false)
        expect(Outcome.is_stopped(Outcome.ok({}))).to.be(false)
    end)
end)

describe("beat contract hardening (review findings)", function()
    -- The `shape.check` this block's classification cases use. The other
    -- suite in this file keeps its own local of the same name; a describe is
    -- a closure, not a scope shared with the one above it.
    local shape = require("lshape").check

    it("a raising llm is Error('call'), not a raw raise", function()
        local s = K.open({})
        local d = K.device({
            llm = function()
                error("adapter exploded")
            end,
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("call")
        -- the failure is noted in the history, under this beat's id
        local last = s:events()[#s:events()]
        expect(last.kind).to.be("llm_call_failed")
        expect(type(last.beat)).to.be("string")
    end)

    it("an llm that answers a non-table is Error('call'), not a raise on resp.status", function()
        for _, answer in ipairs({ false, "just a string", 42 }) do
            local s = K.open({})
            local d = K.device({
                llm = function()
                    return answer
                end,
            })
            s:append({ kind = "msg_user", data = { content = "q" } })
            local o = K.beat(s, d)
            expect(Outcome.is_error(o)).to.be(true)
            expect(o.kind).to.be("call")
            -- and it is noted in the history under this beat's id, like any
            -- other call failure
            local last = s:events()[#s:events()]
            expect(last.kind).to.be("llm_call_failed")
            expect(type(last.beat)).to.be("string")
        end
    end)

    it("a log that cannot be read is Error('state'), not a raw raise", function()
        local s = K.open({})
        s.events = function()
            error("store read failed: database is locked")
        end
        local o = K.beat(s, K.device({ llm = stub_llm("x") }))
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("state")
        -- A raise with no attribution is reported whole: unclassified, not
        -- retryable, the text verbatim. That is the shape a caller can
        -- always count on — the bridge answers the same way, and so does
        -- the module's fallback in a VM that has no bridge at all.
        expect(type(o.detail)).to.be("table")
        expect(o.detail.kind).to.be(nil)
        expect(o.detail.retryable).to.be(false)
        expect(o.detail.message:find("database is locked", 1, true) ~= nil).to.be(true)
    end)

    it("a read that hit the row cap is Error('state') and nothing is called", function()
        -- `session:events()` is bounded and answers `rows, truncated`. The cap
        -- counts forward from `from`, so a truncated read is missing the END
        -- of the history — the most recent turns — and folding it would send
        -- the model a conversation that stops in the middle and looks, from
        -- the other side, like a conversation that ended there. The beat
        -- refuses at the read instead: no reservation, no request, no call.
        local s = K.open({ budget = { amount = 10, tag = "beats" } })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local rows = s._events
        s.events = function()
            return rows, true
        end

        local called = 0
        local o = K.beat(
            s,
            K.device({
                llm = function()
                    called = called + 1
                    return stub_llm("x")()
                end,
            })
        )

        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("state")
        expect(called).to.be(0)
        -- Nothing was reserved and nothing was recorded: the log holds the
        -- seed and no more.
        expect(s:remaining()).to.be(10)
        expect(#rows).to.be(1)
        -- Unclassified, like any raise the bridge did not attribute — this is
        -- the shell declining to build a request, not a kernel error class.
        expect(type(o.detail)).to.be("table")
        expect(o.detail.kind).to.be(nil)
        expect(o.detail.retryable).to.be(false)
        expect(o.detail.message:find("truncated record", 1, true) ~= nil).to.be(true)
    end)

    it("a store that is busy comes back classified and retryable", function()
        -- The bridge attributes what it raises — `knl: <method>: <kind>:
        -- <message>` — and beat reads it back rather than passing the
        -- sentence on. `busy` is the one class that says asking again could
        -- work, and it is the caller's loop that gets to decide: beat
        -- itself makes exactly one attempt.
        local s = K.open({})
        local reserved = 0
        s.reserve = function()
            reserved = reserved + 1
            error("knl: reserve: busy: locked")
        end
        local o = K.beat(s, K.device({ llm = stub_llm("x") }))
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("state") -- the stage of the beat that failed…
        expect(o.detail.kind).to.be("busy") -- …and what the failure was
        expect(o.detail.method).to.be("reserve")
        expect(o.detail.retryable).to.be(true)
        expect(o.detail.message).to.be("locked")
        expect(reserved).to.be(1) -- beat does not retry on its own
        -- and the beat stopped there: no request recorded, no call made
        expect(#s:events()).to.be(0)
    end)

    it("an append that fails is Error('state'), not a raw raise", function()
        local s = K.open({})
        s.append = function()
            error("knl: append: storage: the store is down")
        end
        local o = K.beat(s, K.device({ llm = stub_llm("x") }))
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("state")
    end)

    it("a call failure whose note cannot be recorded keeps the call's reason as cause", function()
        local s = K.open({})
        local real_append = s.append
        s.append = function(self, ev)
            if ev.kind == "llm_call_failed" then
                error("knl: append: storage: the store is down")
            end
            return real_append(self, ev)
        end
        local d = K.device({
            llm = function()
                return nil, "network down"
            end,
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("state") -- the winner: the record that did not land
        expect(type(o.detail)).to.be("table")
        expect(o.detail.message:find("the store is down", 1, true) ~= nil).to.be(true)
        expect(o.detail.cause:find("network down", 1, true) ~= nil).to.be(true) -- the suppressed one
    end)

    it("an answer with no usage is Error('call') — the llm broke llm_result", function()
        -- `llm_result` promises three counts, and the adapter's Mapper is
        -- what normalizes a provider that reported none into zeros. So an
        -- answer that arrives here without a usage is a broken adapter, and
        -- beat says so instead of writing a count nobody reported.
        local s = K.open({})
        local d = K.device({
            llm = function()
                return {
                    status = "ok",
                    content = { { type = "text", text = "hi" } },
                    stop_reason = "end_turn",
                }
            end,
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("call")
        -- A `call` failure carries the classification now, not a sentence:
        -- an adapter that broke its own contract is not a class of provider
        -- failure, so it reads as `unknown` and is not worth asking again.
        expect(o.detail.kind).to.be("unknown")
        expect(o.detail.retryable).to.be(false)
        expect(o.detail.message:find("usage", 1, true) ~= nil).to.be(true)
        -- nothing was recorded as a response; the failure is the note
        local last = s:events()[#s:events()]
        expect(last.kind).to.be("llm_call_failed")
        for _, ev in ipairs(s:events()) do
            expect(ev.kind == "llm_response").to.be(false)
        end
    end)

    it("a filter returning nil is Error('filter') in prod", function()
        local s = K.open({})
        local d = K.device({
            llm = stub_llm("x"),
            filters = {
                function(req)
                    req.system = "mutated"
                    -- forgets to return
                end,
            },
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("filter")
        -- nothing was recorded: the corrupt request never reached append
        expect(#s:events()).to.be(1)
    end)

    it("a refusal is recorded and reported as the adapter classified it", function()
        local s = K.open({})
        local d = K.device({
            llm = function()
                return {
                    status = "refused",
                    content = { { type = "text", text = "no" } },
                    usage = {},
                    -- The provider's own word for the moment…
                    stop_reason = "refusal",
                    -- …and the adapter's classification of it, which is what
                    -- beat reports. A filter block would be the same status
                    -- with a different kind.
                    refusal = { kind = "content_filter" },
                }
            end,
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)
        expect(Outcome.is_refused(o)).to.be(true)
        expect(o.reason).to.be("content_filter")
        expect(type(o.detail.beat)).to.be("string")
        local last = s:events()[#s:events()]
        expect(last.kind).to.be("llm_response")
        expect(last.beat).to.be(o.detail.beat)
    end)

    it("a refusal with no refusal.kind is Error('call') — the llm broke llm_result", function()
        -- What refused is the adapter's judgement to make. A refusal that
        -- named nothing leaves beat with no reason to report, and inventing
        -- one ("refused", or the provider's stop_reason) would be beat
        -- answering a question it was never told the answer to.
        local s = K.open({})
        local d = K.device({
            llm = function()
                return {
                    status = "refused",
                    content = { { type = "text", text = "no" } },
                    usage = {},
                    stop_reason = "refusal",
                }
            end,
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)
        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("call")
        expect(o.detail.kind).to.be("unknown")
        expect(o.detail.message:find("refusal", 1, true) ~= nil).to.be(true)
        local last = s:events()[#s:events()]
        expect(last.kind).to.be("llm_call_failed")
    end)

    it("carries the port's classification out to the Outcome and into the note", function()
        -- The finding this round closed: a call that did not come off left
        -- beat as `err("call", "a sentence")`, and a sentence is not
        -- something a loop can decide on — `policy.retry` reads
        -- `detail.kind` / `detail.retryable`, so no policy could fire on the
        -- failure most worth asking again about. The port classifies
        -- (`knl_adapter`, the one place a status is read) and beat carries
        -- that reading through, unchanged, to both places a caller looks:
        -- the Outcome, and the note in the log.
        local s = K.open({})
        local d = K.device({
            llm = function()
                return nil,
                    {
                        kind = "rate_limited",
                        retryable = true,
                        retry_after = 30,
                        message = "API error 429 (rate_limit)",
                        status = 429,
                    }
            end,
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)

        expect(Outcome.is_error(o)).to.be(true)
        expect(o.kind).to.be("call")
        expect(o.detail.kind).to.be("rate_limited")
        expect(o.detail.retryable).to.be(true)
        expect(o.detail.retry_after).to.be(30)
        expect(o.detail.status).to.be(429)
        expect(shape.check(o.detail, K.shapes.call_error)).to.be(true)

        -- The same fields are in the log, so the classification survives the
        -- Outcome being dropped. `error` stays the sentence a person reads
        -- (and `policy.carry` puts in front of the next request).
        local note = s:events()[#s:events()]
        expect(note.kind).to.be("llm_call_failed")
        expect(note.data.error).to.be("API error 429 (rate_limit)")
        expect(note.data.kind).to.be("rate_limited")
        expect(note.data.retryable).to.be(true)
        expect(note.data.retry_after).to.be(30)
        expect(note.data.status).to.be(429)
        expect(shape.check(note.data, K.shapes.events.llm_call_failed)).to.be(true)
    end)

    it("does not adopt a word the vocabulary does not have", function()
        -- A device whose llm answers with someone else's vocabulary (llm_proto's
        -- own `rate_limit` / `quota` / `not_found`, say) is not passed through:
        -- `unknown` is what the published list says an unnamed failure is, and
        -- a `kind` no declaration covers is one a caller's policy cannot decide
        -- on. The text still says what happened.
        local s = K.open({})
        local d = K.device({
            llm = function()
                return nil, { kind = "not_found", message = "no such model" }
            end,
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)

        expect(o.detail.kind).to.be("unknown")
        expect(o.detail.retryable).to.be(false)
        expect(o.detail.message).to.be("no such model")
        expect(shape.check(o.detail, K.shapes.call_error)).to.be(true)
    end)

    it("classifies a plain sentence and a raise as unknown, not retryable", function()
        -- The two ways a device that is not a port fails: `nil, "..."` and a
        -- raise. Neither says what kind of failure it was, so neither is
        -- given one — but both come back in the same shape, which is what
        -- lets one predicate read every `call` failure.
        for _, llm in ipairs({
            function()
                return nil, "network down"
            end,
            function()
                error("adapter exploded")
            end,
        }) do
            local s = K.open({})
            s:append({ kind = "msg_user", data = { content = "q" } })
            local o = K.beat(s, K.device({ llm = llm }))
            expect(o.kind).to.be("call")
            expect(o.detail.kind).to.be("unknown")
            expect(o.detail.retryable).to.be(false)
            expect(type(o.detail.message)).to.be("string")
            expect(shape.check(o.detail, K.shapes.call_error)).to.be(true)
            local note = s:events()[#s:events()]
            expect(note.data.kind).to.be("unknown")
            expect(note.data.retryable).to.be(false)
        end
    end)

    it("a raising tool_policy denies the call (fail-closed)", function()
        local ran = false
        local s = K.open({})
        local d = K.device({
            llm = llm_with_tool("danger"),
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
        s:append({ kind = "msg_user", data = { content = "q" } })
        local o = K.beat(s, d)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(ran).to.be(false) -- the handler never executed
        expect(o.out.tools[1].ok).to.be(false)
    end)
end)

-- The tool_policy contract. The vocabulary is three values and no
-- more: nil (no opinion), "run", "deny". A policy is a gate, so its own
-- failures close (a raise denies) and its own mistakes stop the beat
-- rather than being read as some fourth intention.
describe("tool_policy — the decision contract", function()
    local function danger_device(policy)
        return K.device({
            llm = llm_with_tool("danger"),
            tools = {
                danger = {
                    handler = function()
                        return "side effect"
                    end,
                },
            },
            tool_policy = policy,
        })
    end

    local function beat_with(policy)
        local ran = false
        local s = K.open({})
        local d = K.device({
            llm = llm_with_tool("danger"),
            tools = {
                danger = {
                    handler = function()
                        ran = true
                        return "side effect"
                    end,
                },
            },
            tool_policy = policy,
        })
        s:append({ kind = "msg_user", data = { content = "q" } })
        return K.beat(s, d), s, ran and true or false
    end

    it('no policy, a nil decision and "run" all run the tool', function()
        for _, policy in ipairs({
            false, -- stands in for "no policy at all"
            function() end,
            function()
                return "run"
            end,
        }) do
            local o, s, ran = beat_with(policy or nil)
            expect(Outcome.is_ok(o)).to.be(true)
            expect(ran).to.be(true)
            expect(o.out.tools[1].ok).to.be(true)
            expect(s:events()[#s:events()].data.result).to.be("side effect")
        end
    end)

    it('"deny" closes the pair ok=false and the handler never runs', function()
        local o, s, ran = beat_with(function()
            return "deny"
        end)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(ran).to.be(false)
        expect(o.out.tools[1].ok).to.be(false)
        local last = s:events()[#s:events()]
        expect(last.kind).to.be("tool_result")
        expect(last.data.ok).to.be(false)
        expect(last.data.result).to.be("tool 'danger' denied by policy")
    end)

    it("a denial's reason rides in the tool_result", function()
        local _, s = beat_with(function()
            return "deny", "not on this scope"
        end)
        expect(s:events()[#s:events()].data.result).to.be("tool 'danger' denied by policy: not on this scope")
    end)

    it("a raising policy denies (fail-closed) and says so", function()
        local o, s, ran = beat_with(function()
            error("policy bug")
        end)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(ran).to.be(false)
        expect(o.out.tools[1].ok).to.be(false)
        expect(s:events()[#s:events()].data.result:find("policy raised", 1, true) ~= nil).to.be(true)
    end)

    it("any other decision is Error('conf') and nothing runs", function()
        for _, decision in ipairs({ "skip", "allow", true, 1 }) do
            local o, s, ran = beat_with(function()
                return decision
            end)
            expect(Outcome.is_error(o)).to.be(true)
            expect(o.kind).to.be("conf")
            expect(ran).to.be(false)
            -- the beat itself happened: its llm_response is recorded, and
            -- no tool_call was written for a call that never ran
            local kinds = {}
            for _, ev in ipairs(s:events()) do
                kinds[#kinds + 1] = ev.kind
            end
            expect(table.concat(kinds, ",")).to.be("msg_user,llm_request,llm_response")
        end
    end)

    it("the device still refuses a tool_policy that is not a function", function()
        expect(function()
            danger_device("deny")
        end).to.fail()
    end)
end)

describe("fold hardening (review findings)", function()
    it("empty messages carry the JSON-array tag across the boundary", function()
        local req = K.fold({}, {})
        expect(#req.messages).to.be(0)
        expect(getmetatable(req.messages).__jsontype).to.be("array")
    end)

    it("reads system and tools off the device", function()
        local req = K.fold({}, K.device({ system = "SYS", tools = echo_tools() }))
        expect(req.system).to.be("SYS")
        expect(#req.tools).to.be(1)
        expect(req.tools[1].name).to.be("echo")
        expect(req.tools[1].input_schema.type).to.be("object")
    end)

    it("a dangling tool_use (crash mid-tool) is closed with a synthetic error result", function()
        local events = {
            { kind = "msg_user", data = { content = "go" }, seq = 1 },
            {
                kind = "llm_response",
                beat = "b1",
                data = {
                    content = {
                        { type = "text", text = "using tools" },
                        { type = "tool_use", id = "c1", name = "t", input = {} },
                        { type = "tool_use", id = "c2", name = "t", input = {} },
                    },
                    usage = {},
                },
                seq = 2,
            },
            -- c1 was answered before the crash; c2 was not
            { kind = "tool_result", beat = "b1", data = { call_id = "c1", ok = true, result = "R" }, seq = 3 },
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

    -- What an assistant turn that said nothing folds to.
    --
    -- An `llm_response` can be recorded with an empty `content`: the adapter
    -- tags the empty array so it crosses the JSON boundary as `[]`, and the
    -- kernel stores what it is given. This pins the request that comes back
    -- out of it, exactly, so "would a provider take this?" is a question with
    -- a concrete input to try by hand rather than a guess about the fold.
    --
    -- Nothing here decides whether that request is acceptable. If it turns out
    -- a provider rejects an assistant message with no content, THIS is the
    -- shape a repair filter would have to change — drop the message, or put a
    -- placeholder block in it — and it would be a filter, in the device, not a
    -- rule inside `fold`: the fold reads the record and the record says the
    -- turn happened and was empty.
    it("an assistant turn recorded with no content folds to an empty content array", function()
        local events = {
            { kind = "msg_user", data = { content = "go" }, seq = 1 },
            {
                kind = "llm_response",
                beat = "b1",
                data = { content = {}, usage = { input_tokens = 1, output_tokens = 0 } },
                seq = 2,
            },
        }
        local req = K.fold(events, {})

        -- Two messages: the seed, and the empty assistant turn — the turn is
        -- kept, not skipped. A fold that dropped it would be inventing a
        -- history in which the model was never asked.
        expect(#req.messages).to.be(2)
        expect(req.messages[1].role).to.be("user")
        expect(req.messages[2].role).to.be("assistant")

        -- And its content is the empty array verbatim: same table, no
        -- placeholder block, nothing added.
        expect(req.messages[2].content).to.be(events[2].data.content)
        expect(#req.messages[2].content).to.be(0)

        -- The whole request, spelled out, so the shape is pinned and not
        -- merely counted: no system, no tools, and two messages.
        expect(req.system).to.be(nil)
        expect(req.tools).to.be(nil)
        expect(req.messages[1].content).to.be("go")
    end)

    it("an answered tool pair needs no repair (no synthetic results)", function()
        local events = {
            {
                kind = "llm_response",
                beat = "b1",
                data = { content = { { type = "tool_use", id = "c1", name = "t", input = {} } }, usage = {} },
                seq = 1,
            },
            { kind = "tool_result", beat = "b1", data = { call_id = "c1", ok = true, result = "R" }, seq = 2 },
            {
                kind = "llm_response",
                beat = "b2",
                data = { content = { { type = "text", text = "done" } }, usage = {} },
                seq = 3,
            },
        }
        local req = K.fold(events, {})
        expect(#req.messages).to.be(3)
        expect(#req.messages[2].content).to.be(1) -- just the real result
    end)
end)

describe("knl.views — the query views the module ships", function()
    -- No SQLite here on purpose. What a statement SELECTS is a question for
    -- a database and is asked where there is one (knl_beat_test.lua inv11);
    -- what is asked here is the part that is this layer's own: that a view
    -- is one query, that it reads the set the caller named, that it binds
    -- rather than splices, and how it reads the rows back.
    local VIEW_NAMES = { "beats", "tool_pairs", "ledger", "usage" }

    local function last_query(s)
        return s._queries[#s._queries]
    end

    it("runs exactly one query per call", function()
        for _, name in ipairs(VIEW_NAMES) do
            local s = K.open({})
            K.views[name](s)
            expect(#s._queries).to.be(1)
        end
    end)

    it("reads: every statement is a SELECT", function()
        for _, name in ipairs(VIEW_NAMES) do
            local s = K.open({})
            K.views[name](s)
            expect(last_query(s).sql:match("^%s*SELECT") ~= nil).to.be(true)
        end
    end)

    it("names the streams with $sessions, so a set is read by the same SQL", function()
        -- The token is what the kernel expands into bound placeholders. A
        -- view that filtered on `$stream` alone could not span a set at
        -- all, and one that wrote ids into its text would not be binding.
        for _, name in ipairs(VIEW_NAMES) do
            local s = K.open({})
            K.views[name](s)
            expect(last_query(s).sql:find("$sessions", 1, true) ~= nil).to.be(true)
        end
    end)

    it("binds nothing of its own (no view takes parameters)", function()
        for _, name in ipairs(VIEW_NAMES) do
            local s = K.open({})
            K.views[name](s)
            expect(last_query(s).params == nil).to.be(true)
        end
    end)

    it("passes the caller's opts through to the query untouched", function()
        local opts = { sessions = { "sess-a", "sess-b" }, timeout_ms = 250, limit = 10 }
        for _, name in ipairs(VIEW_NAMES) do
            local s = K.open({})
            K.views[name](s, opts)
            local passed = last_query(s).opts
            expect(passed).to.be(opts)
            expect(passed.sessions[1]).to.be("sess-a")
            expect(passed.sessions[2]).to.be("sess-b")
        end
    end)

    it("hands back the rows and the truncation flag the query answered", function()
        local s = K.open({})
        s._query_rows = { { beat = "b-1", seq_from = 3, seq_to = 6, kinds = "llm_request,llm_response" } }
        local rows, truncated = K.views.beats(s)
        expect(#rows).to.be(1)
        expect(rows[1].beat).to.be("b-1")
        expect(rows[1].kinds).to.be("llm_request,llm_response")
        expect(truncated).to.be(false)
    end)

    it("reads tool_pairs' `ok` back as a boolean (SQLite has none)", function()
        local s = K.open({})
        s._query_rows = {
            { beat = "b-1", call_id = "c1", name = "echo", ok = 1 },
            { beat = "b-1", call_id = "c2", name = "ghost", ok = 0 },
        }
        local rows = K.views.tool_pairs(s)
        expect(rows[1].ok).to.be(true)
        expect(rows[2].ok).to.be(false)
        -- and the row the query answered is not written to on the way
        expect(s._query_rows[1].ok).to.be(1)
    end)

    it("counts usage by kind, one row per stream (the accounting reading)", function()
        -- What the statement selects is a question for a database (asked in
        -- knl_beat_test.lua inv11). What is this layer's own is that the
        -- read is one SELECT over the `llm_response` records, grouped by
        -- stream — token usage is a query view now, not a kernel built-in.
        local s = K.open({})
        s._query_rows = {
            { stream = "sess-a", calls = 2, input_tokens = 20, output_tokens = 6, thinking_tokens = 0 },
        }
        local rows, truncated = K.views.usage(s)
        expect(#rows).to.be(1)
        expect(rows[1].calls).to.be(2)
        expect(rows[1].input_tokens).to.be(20)
        expect(truncated).to.be(false)

        local sql = last_query(s).sql
        expect(sql:find("llm_response", 1, true) ~= nil).to.be(true)
        expect(sql:find("COUNT(*)", 1, true) ~= nil).to.be(true)
        expect(sql:find("GROUP BY stream", 1, true) ~= nil).to.be(true)
        for _, counter in ipairs({ "input_tokens", "output_tokens", "thinking_tokens" }) do
            expect(sql:find("$.usage." .. counter, 1, true) ~= nil).to.be(true)
        end
    end)

    it("declares every view it exports, and exports every view it declares", function()
        -- The same completeness rule as the module registry, on the views:
        -- one table stands behind `knl.shapes.views` and the `views` entry
        -- of `knl.shapes.api`, so a view added to one is added to both.
        for name in pairs(K.views) do
            expect(K.shapes.views[name]).to.exist()
        end
        for name in pairs(K.shapes.views) do
            expect(type(K.views[name])).to.be("function")
        end
        expect(K.shapes.api.views.members).to.be(K.shapes.views)
    end)
end)
