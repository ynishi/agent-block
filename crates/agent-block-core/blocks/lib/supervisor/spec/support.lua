-- support.lua — the in-memory kernel the supervisor specs are driven against.
--
-- NOT a spec: it declares no suite and calls nothing in the test framework,
-- which is how the runner decides what to run (crates/lua-spec-runner/src/
-- main.rs, `is_spec` — detection is by USE, not by filename). Every spec beside
-- it reaches it with `require("supervisor.spec.support")`, which resolves
-- through the same `blocks/lib` search path `require("knl")` does.
--
-- Why it is not policy's copy: this module is about the SESSION TREE, and
-- policy's fake has no tree in it. What is needed here and not there is one
-- database several streams live on, an allocation that moves units between two
-- of them in one write (and refuses when the balance is short), a close that
-- records the children still open, and a `query` that answers the two views a
-- supervisor reads. Widening policy's fake to carry all that would make every
-- policy spec depend on a tree none of them has an opinion about.
--
-- What the fake reproduces (mirroring crates/agent-block-core/src/knl/
-- session.rs and src/bridge/knl.rs):
--   * `open{ parent = s, budget = { from_parent = n } }` writes the child's
--     `session_opened{ parent }` + `budget_granted` and the parent's
--     `budget_reserved{ child }` as ONE move, or writes `budget_refused` on the
--     parent and raises `refused` with no child opened at all;
--   * `close` records `session_closed{ reason, detail?, open_children? }` once;
--   * a raise is the attributed text `knl: <method>: <kind>: <message>`, since
--     a Rust callback cannot raise a table, and `error` reads it back;
--   * `query` answers the ledger view, the tree view and a plain read of the
--     `events` table, with `data` as TEXT — the column crosses the query
--     boundary encoded, which is the whole reason `supervisor.merge` decodes.
--
-- The two stand-ins, named so no case mistakes them for the real thing:
--   * the `query` dispatch is by a marker in the statement, not by running SQL.
--     What a statement SELECTS is asked where there is a database
--     (crates/agent-block/tests/fixtures/knl_beat_test.lua);
--   * `std.json` is a memo, not a codec: `encode` hands back an opaque token
--     and remembers the value, `decode` looks it up. It proves that `merge`
--     decodes the column it read and folds what came back; what a real
--     `std.json.decode` does to the text is the host's.

local M = {}

local opened = 0
local clock = 0

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
    refused = true,
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

-- ─────────────────────────────────────────────────────────────────────────────
-- The database: one table of streams, which is what makes a tree possible
-- ─────────────────────────────────────────────────────────────────────────────

--- Every stream every session in this VM opened, by id. One table, because a
--- parent and its children are on ONE database in the kernel too — an
--- allocation spanning two databases is refused there before anything is
--- written.
local streams = {}

local function record(stream, kind, data)
    clock = clock + 1
    stream.seq = stream.seq + 1
    local event = { kind = kind, seq = stream.seq, epoch_ms = clock, data = data }
    stream.events[#stream.events + 1] = event
    return event
end

--- The ids of the children of `stream` that had not ended.
local function open_children(stream)
    local still = {}
    for _, child in ipairs(stream.children) do
        if not child.closed then
            still[#still + 1] = child.id
        end
    end
    return still
end

-- ─────────────────────────────────────────────────────────────────────────────
-- query — the two views a supervisor reads, and a plain read of the table
-- ─────────────────────────────────────────────────────────────────────────────

local BUDGET_KINDS = {
    budget_granted = true,
    budget_reserved = true,
    budget_refused = true,
    budget_spent = true,
}

--- The streams one read spans: the named set, or this one alone.
local function spanned(stream, opts)
    if opts ~= nil and opts.sessions ~= nil then
        return opts.sessions
    end
    return { stream.id }
end

local function ledger_rows(stream, opts)
    local rows = {}
    for _, id in ipairs(spanned(stream, opts)) do
        local target = streams[id]
        for _, event in ipairs(target and target.events or {}) do
            if BUDGET_KINDS[event.kind] then
                rows[#rows + 1] = {
                    seq = event.seq,
                    kind = event.kind,
                    amount = event.data.amount,
                    tag = event.data.tag,
                }
            end
        end
    end
    return rows
end

local function tree_rows(stream)
    local rows = {}
    local function walk(node)
        local closed_at, children
        for _, event in ipairs(node.events) do
            if event.kind == "session_closed" then
                closed_at = closed_at or event.epoch_ms
                children = children or event.data.open_children
            end
        end
        rows[#rows + 1] = {
            session = node.id,
            parent = node.parent and node.parent.id or nil,
            opened_epoch_ms = node.events[1] and node.events[1].epoch_ms or nil,
            closed_epoch_ms = closed_at,
            open_children = children,
        }
        for _, child in ipairs(node.children) do
            walk(child)
        end
    end
    walk(stream)
    return rows
end

local function event_rows(stream, opts)
    local rows = {}
    for _, id in ipairs(spanned(stream, opts)) do
        local target = streams[id]
        for _, event in ipairs(target and target.events or {}) do
            rows[#rows + 1] = {
                stream = id,
                seq = event.seq,
                kind = event.kind,
                data = event.data ~= nil and M.json.encode(event.data) or nil,
            }
        end
    end
    return rows
end

-- ─────────────────────────────────────────────────────────────────────────────
-- The session handle: the whole declared surface, because knl asks for it
-- ─────────────────────────────────────────────────────────────────────────────

local function fake_session(stream)
    local s = { _stream = stream }

    function s:id()
        return stream.id
    end
    function s:scope_id()
        return "scope-" .. stream.id
    end
    function s:owner()
        return stream.owner
    end
    function s:append(ev)
        assert(not stream.closed, "knl: append: closed: session is closed")
        assert(type(ev) == "table" and type(ev.kind) == "string", "knl: append: validation: kind is required")
        return record(stream, ev.kind, ev.data).seq
    end
    function s:events()
        return stream.events
    end
    function s:len()
        return #stream.events
    end
    function s:view(_name, _opts)
        error("knl: view: validation: unknown view")
    end
    function s:query(sql, params, opts)
        stream.queries[#stream.queries + 1] = { sql = sql, params = params, opts = opts }
        if sql:find("budget_granted", 1, true) then
            return ledger_rows(stream, opts), false
        end
        if sql:find("RECURSIVE tree", 1, true) then
            return tree_rows(stream), false
        end
        if sql:find("FROM events", 1, true) then
            return event_rows(stream, opts), stream.truncate == true
        end
        error("knl: query: validation: the fake does not answer this statement")
    end
    function s:reserve(n)
        assert(not stream.closed, "knl: reserve: closed: session is closed")
        if stream.remaining == nil then
            return true
        end
        if stream.remaining < n then
            record(stream, "budget_refused", { amount = n, tag = stream.tag, remaining = stream.remaining })
            return false, stream.tag
        end
        stream.remaining = stream.remaining - n
        record(stream, "budget_reserved", { amount = n, tag = stream.tag })
        return true
    end
    function s:spend(n)
        if stream.remaining == nil then
            return
        end
        stream.remaining = math.max(0, stream.remaining - n)
        record(stream, "budget_spent", { amount = n, tag = stream.tag })
    end
    function s:exhausted()
        return stream.remaining ~= nil and stream.remaining <= 0
    end
    function s:remaining()
        return stream.remaining
    end
    function s:close(reason, detail)
        if stream.closed then
            return
        end
        stream.closed = true
        stream.close_reason = reason or "closed"
        local still = open_children(stream)
        record(stream, "session_closed", {
            reason = stream.close_reason,
            detail = detail,
            open_children = #still > 0 and still or nil,
        })
    end

    return s
end

local function open_stream(opts)
    opts = opts or {}
    opened = opened + 1
    local id = string.format("sess-%06d", opened)
    local parent_handle = opts.parent
    local parent = parent_handle and parent_handle._stream or nil
    local budget = opts.budget or {}

    local stream = {
        id = id,
        parent = parent,
        children = {},
        events = {},
        queries = {},
        seq = 0,
        owner = opts.owner or (parent and parent.owner) or "anon",
        closed = false,
    }

    if parent ~= nil then
        -- The allocation: one move, both ledgers, or neither.
        local amount = budget.from_parent
        local tag = budget.tag or parent.tag
        if parent.remaining ~= nil and parent.remaining < amount then
            record(parent, "budget_refused", {
                amount = amount,
                tag = tag,
                remaining = parent.remaining,
                child = id,
            })
            error(
                "knl: open: refused: the parent's balance is "
                    .. tostring(parent.remaining)
                    .. ", which does not cover "
                    .. tostring(amount)
            )
        end
        if parent.remaining ~= nil then
            parent.remaining = parent.remaining - amount
        end
        stream.remaining = amount
        stream.tag = tag
        streams[id] = stream
        parent.children[#parent.children + 1] = stream
        record(parent, "budget_reserved", { amount = amount, tag = tag, child = id })
        record(stream, "session_opened", { scope_id = "scope-" .. id, owner = stream.owner, parent = parent.id })
        record(stream, "budget_granted", { amount = amount, tag = tag, parent = parent.id })
        return stream
    end

    stream.remaining = budget.amount
    stream.tag = budget.tag
    streams[id] = stream
    record(stream, "session_opened", { scope_id = "scope-" .. id, owner = stream.owner })
    if budget.amount ~= nil then
        record(stream, "budget_granted", { amount = budget.amount, tag = budget.tag })
    end
    return stream
end

-- The bridge, installed as a global. The module reads `knl` off `_G` (at load
-- and again lazily), so this is the one place a spec's kernel comes from.
knl = {
    open = function(o)
        return fake_session(open_stream(o))
    end,
    resume = function(o)
        return fake_session(open_stream(o))
    end,
    error = fake_read_error,
    new_beat_id = function()
        clock = clock + 1
        return string.format("beat-%06d", clock)
    end,
}

-- ─────────────────────────────────────────────────────────────────────────────
-- std.json — the memo stand-in (see the header)
-- ─────────────────────────────────────────────────────────────────────────────

local encoded = {}
local tokens = 0

M.json = {
    encode = function(value)
        tokens = tokens + 1
        local token = "json:" .. tokens
        encoded[token] = value
        return token
    end,
    decode = function(text)
        local value = encoded[text]
        if value == nil then
            error("the memo codec was handed text it did not encode: " .. tostring(text), 0)
        end
        return value
    end,
}

if rawget(_G, "std") == nil then
    std = { json = M.json }
end

local kernel = require("knl")

-- ─────────────────────────────────────────────────────────────────────────────
-- Session and event helpers
-- ─────────────────────────────────────────────────────────────────────────────

--- A root session with an allowance a case can spend from.
function M.session(opts)
    opts = opts or {}
    return kernel.open({
        owner = opts.owner or "spec",
        budget = opts.budget or { amount = 100, tag = "beats" },
    })
end

--- The caller's seed: an event like any other, with what the kind is about
--- under `data`.
function M.seed(session, text)
    session:append({ kind = "msg_user", data = { content = text } })
    return session
end

--- An assistant answer in the log, the way a beat records one.
function M.answered(session, text)
    session:append({
        kind = "llm_response",
        data = {
            content = { { type = "text", text = text } },
            usage = { input_tokens = 1, output_tokens = 1, thinking_tokens = 0 },
        },
    })
    return session
end

--- The queries a session was asked, in order (the fake records every one).
function M.queries(session)
    return session._stream.queries
end

--- Make the next read on this session report that the row cap cut it off.
function M.truncate(session, on)
    session._stream.truncate = on ~= false
end

--- The recorded kinds of a stream, in seq order, as one comparable string.
function M.kinds(session)
    local names = {}
    for _, event in ipairs(session:events()) do
        names[#names + 1] = event.kind
    end
    return table.concat(names, ",")
end

--- Whether a stream ended, and how.
function M.close_reason(session)
    return session._stream.close_reason
end

return M
