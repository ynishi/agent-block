-- support.lua — the session helpers and the stubs the policy specs share,
-- over the shared fake kernel.
--
-- NOT a spec: it declares no suite and calls nothing in the lspec framework,
-- which is exactly how the runner decides what to run (crates/lua-spec-runner/
-- src/main.rs, `is_spec` — detection is by USE, not by filename, so a file
-- here that ran no tests would still have to be named like one to be skipped).
-- Every spec beside it reaches it with `require("policy.spec.support")`, which
-- resolves through the same `blocks/lib` search path `require("knl")` and
-- `require("policy")` do.
--
-- What it adds, and what it does not: the kernel is the shared stand-in
-- (knl/spec/fake_bridge.lua, which says which bridge facts it mirrors),
-- installed here once for every policy spec. This file asks it for none of
-- the kernel's own writes and no clock — a policy spec counts and orders the
-- rows a beat appended, and writes `epoch_ms` itself when it needs history —
-- and adds only what a policy case is built from: a session with an allowance,
-- a seed, a truncated read, readings of the log, and the llm and tool stubs.

local M = {}

require("knl.spec.fake_bridge").install()

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
--- The fake answers `false` there, like every read that reached the end of a
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

--- The distinct `meta.beat` ids in the session, in first-seen order.
function M.beat_ids(session)
    local seen, ids = {}, {}
    for _, ev in ipairs(session:events()) do
        local id = ev.meta ~= nil and ev.meta.beat or nil
        if id ~= nil and not seen[id] then
            seen[id] = true
            ids[#ids + 1] = id
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
