-- support.lua — the in-memory kernel the supervisor specs are driven against.
--
-- NOT a spec: it declares no suite and calls nothing in the test framework,
-- which is how the runner decides what to run (crates/lua-spec-runner/src/
-- main.rs, `is_spec` — detection is by USE, not by filename). Every spec beside
-- it reaches it with `require("supervisor.spec.support")`, which resolves
-- through the same `blocks/lib` search path `require("knl")` does.
--
-- The kernel is the shared stand-in (knl/spec/fake_bridge.lua, which says
-- which bridge facts it mirrors). The session tree is the bridge's own and so
-- is the base's: one database several streams live on, an allocation that
-- moves units from a parent to a child in one write (or refuses when the
-- balance is short, opening no child), and a close that records the children
-- still open.
--
-- What this file adds on top, and why policy's support does not: a supervisor
-- reads the log the KERNEL writes around a tree, so
--   * the fake is asked for the kernel's lifecycle and ledger writes
--     (`session_opened` / `session_closed`, `budget_*`) and a clock, which a
--     policy spec would find as rows it never appended;
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

-- ─────────────────────────────────────────────────────────────────────────────
-- query — the two views a supervisor reads, and a plain read of the table
-- ─────────────────────────────────────────────────────────────────────────────

--- The fake, once installed below: its `sessions` map is the one database
--- every read here spans.
local fake

local BUDGET_KINDS = {
    budget_granted = true,
    budget_reserved = true,
    budget_refused = true,
    budget_spent = true,
}

--- The streams one read spans: the named set, or this one alone.
local function spanned(session, opts)
    if opts ~= nil and opts.sessions ~= nil then
        return opts.sessions
    end
    return { session:id() }
end

local function ledger_rows(session, opts)
    local rows = {}
    for _, id in ipairs(spanned(session, opts)) do
        local target = fake.sessions[id]
        for _, event in ipairs(target and target._events or {}) do
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

local function tree_rows(session)
    local rows = {}
    local function walk(node)
        local closed_at, children
        for _, event in ipairs(node._events) do
            if event.kind == "session_closed" then
                closed_at = closed_at or event.epoch_ms
                children = children or event.data.open_children
            end
        end
        rows[#rows + 1] = {
            session = node:id(),
            parent = node._parent and node._parent:id() or nil,
            opened_epoch_ms = node._events[1] and node._events[1].epoch_ms or nil,
            closed_epoch_ms = closed_at,
            open_children = children,
        }
        for _, child in ipairs(node._children) do
            walk(child)
        end
    end
    walk(session)
    return rows
end

local function event_rows(session, opts)
    local rows = {}
    for _, id in ipairs(spanned(session, opts)) do
        local target = fake.sessions[id]
        for _, event in ipairs(target and target._events or {}) do
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

local function answer(session, sql, _params, opts)
    if sql:find("budget_granted", 1, true) then
        return ledger_rows(session, opts), false
    end
    if sql:find("RECURSIVE tree", 1, true) then
        return tree_rows(session), false
    end
    if sql:find("FROM events", 1, true) then
        return event_rows(session, opts), session._truncate == true
    end
    error("knl: query: validation: the fake does not answer this statement", 0)
end

-- The fake bridge, installed as the global `knl` BEFORE require("knl"). The
-- clock is a counter: a tree row reads when a stream opened and closed, and
-- only the order of those stamps is anybody's business here.
local clock = 0
fake = require("knl.spec.fake_bridge").install({
    writes = { lifecycle = true, ledger = true },
    clock = function()
        clock = clock + 1
        return clock
    end,
    query = answer,
})

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
    return session._queries
end

--- Make the next read on this session report that the row cap cut it off.
function M.truncate(session, on)
    session._truncate = on ~= false
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
    return session.close_reason
end

return M
