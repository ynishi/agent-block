-- support.lua — the in-memory store and the stubs the five policy specs
-- share.
--
-- NOT a spec: it declares no suite and calls nothing in the lspec framework,
-- which is exactly how the runner decides what to run (crates/lua-spec-runner/
-- src/main.rs, `is_spec` — detection is by USE, not by filename, so a file
-- here that ran no tests would still have to be named like one to be skipped).
-- Every spec beside it reaches it with `require("policy.spec.support")`, which
-- resolves through the same `blocks/lib` search path `require("knl")` and
-- `require("policy")` do.
--
-- Why it exists at all: knl's own two specs each carry their own copy of the
-- fake bridge, which is fine at two and is five copies of one contract here —
-- and the day one copy drifts, a policy spec is passing against a kernel that
-- does not exist. One copy, required five times.
--
-- What the fake reproduces (mirroring crates/agent-block-core/src/bridge/knl.rs):
--   * `open` hands back a session carrying the WHOLE declared surface, because
--     `knl.beat` asks for the whole surface before it treats a value as a
--     session — a fake answering only what a beat calls would stand in for
--     something the kernel does not hand out;
--   * `append` stamps `seq` and passes every other field through untouched,
--     `beat` included: the kernel stores the id it is given and numbers
--     nothing;
--   * `reserve` is the one decision point — it deducts, or refuses with the
--     grant's tag and leaves the balance where it was;
--   * `new_beat_id` mints a fresh, time-ordered id per call;
--   * `error` reads an attributed raise (`knl: <method>: <kind>: <message>`)
--     back into a table, since a Rust callback cannot raise one.
--
-- The store is in memory and is the session itself: `knl.open{ store = ... }`
-- is accepted and the option is recorded rather than honoured, because there
-- is no SQLite in the pure runner. What a statement SELECTS is asked where
-- there is a database (crates/agent-block/tests/fixtures/knl_beat_test.lua).

local M = {}

local minted = 0
local opened = 0

--- The classes the kernel publishes (Rust `KnlError::KINDS`), retyped on
--- purpose: this table stands in for the bridge, and a stand-in that borrowed
--- the module's own list could not catch the module reading it wrong.
local FAKE_ERROR_KINDS = {
    busy = true,
    storage = true,
    corruption = true,
    closed = true,
    validation = true,
    unsupported = true,
    timeout = true,
}

local function fake_read_error(raised)
    local text = tostring(raised)
    local out = { message = text, retryable = false }
    for line in text:gmatch("[^\n]+") do
        local attributed = line:match("knl: (.+)$")
        if attributed then
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

local function fake_session(opts)
    opts = opts or {}
    local grant = opts.budget or {}
    opened = opened + 1
    local id = string.format("sess-%06d", opened)
    local s = {
        _events = {},
        _queries = {},
        _query_rows = {},
        _store = opts.store,
        _seq = 0,
        _remaining = grant.amount,
        _tag = grant.tag,
        _owner = opts.owner or "anon",
        closed = false,
        close_reason = nil,
    }
    function s:id()
        return id
    end
    function s:scope_id()
        return "scope-" .. id
    end
    function s:owner()
        return self._owner
    end
    function s:append(ev)
        assert(not self.closed, "knl: append: closed: session is closed")
        assert(type(ev) == "table" and type(ev.kind) == "string", "knl: append: validation: kind is required")
        assert(ev.beat == nil or type(ev.beat) == "string", "knl: append: validation: beat must be a string")
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
    function s:view(_name, _opts)
        error("knl: view: validation: unknown view")
    end
    function s:query(sql, params, query_opts)
        assert(not self.closed, "knl: query: closed: session is closed")
        self._queries[#self._queries + 1] = { sql = sql, params = params, opts = query_opts }
        return self._query_rows, false
    end
    function s:reserve(n)
        assert(not self.closed, "knl: reserve: closed: session is closed")
        if self._remaining == nil then
            return true
        end
        if self._remaining < n then
            return false, self._tag
        end
        self._remaining = self._remaining - n
        return true
    end
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

-- The bridge, installed as a global. The module reads `knl` off `_G` (at load
-- and again lazily), so this is the one place a spec's kernel comes from.
knl = {
    open = function(o)
        return fake_session(o)
    end,
    resume = function(o)
        local s = fake_session(o)
        s.resumed_from = o and o.session
        return s
    end,
    error = fake_read_error,
    new_beat_id = function()
        minted = minted + 1
        return string.format("beat-%06d", minted)
    end,
}

local kernel = require("knl")

-- ─────────────────────────────────────────────────────────────────────────────
-- Session and event helpers
-- ─────────────────────────────────────────────────────────────────────────────

--- A session on the in-memory store, with an allowance no spec is meant to
--- hit. A spec about the budget grants its own.
function M.session(opts)
    opts = opts or {}
    return kernel.open({
        owner = opts.owner or "spec",
        budget = opts.budget or { amount = 100, tag = "beats" },
        store = { memory = true },
    })
end

--- The caller's seed: an event like any other, with what the kind is about
--- under `data`.
function M.seed(session, text)
    session:append({ kind = "msg_user", data = { content = text } })
    return session
end

--- Make `session` answer its reads the way the kernel does when the row cap
--- cut one short: the rows it holds, and `true` beside them.
---
--- The fake answers a single value, like every read that reached the end of a
--- stream, so this is how a spec puts the other case in front of a policy. The
--- rows are the ones already recorded — a truncated read is a real prefix of a
--- real log, not an empty one, which is exactly what makes it dangerous to
--- fold.
function M.truncate(session)
    local rows = session._events
    session.events = function()
        return rows, true
    end
    return session
end

--- The recorded kinds in seq order, as one comparable string.
function M.kinds(session)
    local names = {}
    for _, ev in ipairs(session:events()) do
        names[#names + 1] = ev.kind
    end
    return table.concat(names, ",")
end

--- The distinct `beat` ids in the session, in first-seen order.
function M.beat_ids(session)
    local seen, ids = {}, {}
    for _, ev in ipairs(session:events()) do
        if ev.beat ~= nil and not seen[ev.beat] then
            seen[ev.beat] = true
            ids[#ids + 1] = ev.beat
        end
    end
    return ids
end

-- ─────────────────────────────────────────────────────────────────────────────
-- llm stubs — the `llm_result` contract, and the two ways a call fails
-- ─────────────────────────────────────────────────────────────────────────────

--- The three counts an adapter promises. Written once so no stub invents a
--- usage shape of its own.
function M.usage()
    return { input_tokens = 1, output_tokens = 1, thinking_tokens = 0 }
end

--- An `ok` result carrying `blocks`.
function M.answer(blocks, stop_reason)
    return {
        status = "ok",
        content = blocks,
        usage = M.usage(),
        stop_reason = stop_reason or "end_turn",
    }
end

--- A plain text answer.
function M.text(body)
    return M.answer({ { type = "text", text = body } })
end

--- An answer asking for one tool.
function M.calls(id, name, input)
    return M.answer({ { type = "tool_use", id = id, name = name, input = input or {} } }, "tool_use")
end

--- An llm that hands back queued answers in order. A queued function is
--- called with the request instead, which is how a case makes the call fail.
function M.queue(...)
    local answers = { ... }
    local at = 0
    return function(request)
        at = at + 1
        local answer = answers[at]
        assert(answer ~= nil, "the llm stub ran more often than the case queued")
        if type(answer) == "function" then
            return answer(request)
        end
        return answer
    end
end

--- An llm that hands back the same answer for every beat.
function M.always(answer)
    return function(_request)
        return answer
    end
end

--- The transport failure form: `nil, err`, which beat records as
--- `llm_call_failed` and reports as `err("call")`.
function M.fails(reason)
    return function(_request)
        return nil, reason
    end
end

-- ─────────────────────────────────────────────────────────────────────────────
-- tool stubs
-- ─────────────────────────────────────────────────────────────────────────────

--- One tool that answers `reply`.
function M.tool(name, reply)
    return {
        [name] = {
            description = name,
            input_schema = { type = "object" },
            handler = function()
                return reply
            end,
        },
    }
end

--- One tool whose handler raises, so the pair closes `ok = false`.
function M.failing_tool(name, message)
    return {
        [name] = {
            description = name,
            input_schema = { type = "object" },
            handler = function()
                error(message, 0)
            end,
        },
    }
end

return M
