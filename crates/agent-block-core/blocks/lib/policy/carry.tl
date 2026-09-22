--- policy.carry — one bounded note about the beat that failed.
---
--- One of the eleven, on a file of its own because it is the one filter that
--- reads the log: it derives the last beat's tool pairs from
--- `session:events()`, decides with the caller's `failed` which of them went
--- wrong, and prepends one note of at most `max_bytes` to the request.
---
--- Exported through `policy`; see that header for the rules every policy
--- keeps. This file is `require`d by `policy/init.lua` and not meant to be
--- reached for directly.

local kernel = require("knl")
local lshape = require("lshape")
local shared = require("policy.shared")
local T = lshape.t
local shape = lshape.check

local record CarryOpts
    max_bytes: integer
    failed: function(shared.ToolPair): boolean
end

local record M
    type Filter = function(kernel.Request): kernel.Request
    type Bind = function(kernel.Session): Filter
    type CarryFactory = function(CarryOpts): Bind
    shapes: {string:lshape.Schema}
    api: {string:shared.ApiEntry}
end

-- What this file shares with its siblings, by the names the code was
-- written against (`policy.shared`).
local DEFAULT_MAX_BYTES = shared.DEFAULT_MAX_BYTES
local ELLIPSIS = shared.ELLIPSIS
local NOTE_PREFIX = shared.NOTE_PREFIX
local ARRAY_TAG = shared.ARRAY_TAG
local whole_at_least = shared.whole_at_least
local opts_of = shared.opts_of
local whole_log = shared.whole_log
local beats_of = shared.beats_of
local canonical = shared.canonical
local data_of = shared.data_of
local needs_session = shared.needs_session
local opts_contract = shared.opts_contract
local TOOL_PAIR = shared.TOOL_PAIR
local arg_of = shared.arg_of
local SESSION_ARG = shared.SESSION_ARG

--- What `policy.carry` is configured with. `failed` is the caller's reading of
--- a tool pair, for the failures the kernel's `ok` flag cannot see.
local CARRY_OPTS, CARRY_ARG = opts_contract({
    max_bytes = T.number:describe("the note's whole length in bytes; a whole number >= 1"):is_optional(),
    failed = T.fn:describe("fn(pair) -> boolean; default: the pair's ok flag"):is_optional(),
})

-- ============================================================
-- carry — one bounded note about the beat that failed
-- ============================================================

--- `text`, cut to `limit` bytes. The one place anything in this module is
--- shortened, and the limit is the caller's declared one.
---
--- The cut is marked. A note that was silently truncated reads as a complete
--- sentence that happens to end oddly, and the model has no way to tell that
--- something was removed; the marker is what makes the trim visible, and it
--- is paid for out of the limit rather than added on top of it.
local function trim(text: string, limit: integer): string
    if #text <= limit then
        return text
    end
    if limit <= #ELLIPSIS then
        return text:sub(1, limit)
    end
    return text:sub(1, limit - #ELLIPSIS) .. ELLIPSIS
end

--- A tool pair's `result` as note text: a string verbatim, anything else in
--- the rendering `canonical` gives it.
---
--- Which matters as soon as a caller can call a RETURNED value a failure. A
--- tool answering `{ ok = false, error = "there is no line 300" }` keeps its
--- reason inside a table, and `tostring` of a table is an address: a note
--- reading `table: 0x55…` would carry the failure forward without carrying
--- what failed, which is the whole of what the next beat needs. `canonical` is
--- this module's own renderer and costs it no host global.
local function render_result(result: any): string
    if type(result) == "string" then
        return result as string
    end
    return canonical(result)
end

--- The default reading of a tool pair: the kernel's own flag, and only it.
---
--- The kernel closes a pair `ok = false` when the handler RAISED (or when a
--- `tool_policy` denied the call before it ran) — `knl`'s beat. An `ok` no
--- record carried is read as true, which keeps this exactly the judgement
--- `carry` has always made: a pair is a failure when it closed `ok = false`,
--- not when it left the flag out.
local function default_failed(pair: shared.ToolPair): boolean
    return not pair.ok
end

--- One `tool_result` event as the pair a predicate is handed
--- (`policy.shapes.tool_pair`), with the call half looked up by id.
local function pair_of(beat: string, called: {string:{string:any}}, data: {string:any}): shared.ToolPair
    local call = called[data.call_id as string] or {}
    return {
        beat = beat,
        call_id = data.call_id as string,
        name = call.name as string,
        input = call.input,
        result = data.result,
        ok = data.ok ~= false,
    }
end

--- What a carried pair says in the note.
---
--- The tool's NAME comes off the `tool_call` half of the pair, and when the
--- pair has no call to take it from the note says a tool call failed rather
--- than inventing one to blame.
local function reason_for(pair: shared.ToolPair): string
    local who = "a tool call"
    if pair.name ~= nil then
        who = "tool '" .. tostring(pair.name) .. "'"
    end
    return who .. " failed: " .. render_result(pair.result)
end

--- What went wrong in the last beat, as one bounded note, or nil when
--- nothing did.
---
--- Two things count as a failure. A call that did not come off
--- (`llm_call_failed`, which `knl.fold` skips entirely, so without this note
--- the model sees nothing at all of it) is always one, and no predicate is
--- consulted about it: it is not a tool pair and there is nothing in it for
--- one to read. Every `tool_result` of the beat is put to `failed`, which by
--- default is the kernel's `ok` flag and otherwise is the caller's reading of
--- the pair.
---
--- A RESPONSE THAT WAS TRUNCATED IS NOT ONE. A beat that hit the model's
--- output ceiling recorded an `llm_response` like any other and its
--- `stop_reason` says so; the beat came off, nothing failed, and the answer
--- it produced is in the request already. Nothing here matches it — not
--- because truncation is excluded by a special case, but because it leaves
--- behind none of the two records this reads. That is the same reason a
--- refusal is not carried: it is a recorded response, not a failure.
---
--- @param events table|nil  the session's events, in seq order
--- @param limit number  the note's whole length in bytes
--- @param failed function  fn(pair) -> boolean, over a `policy.shapes.tool_pair`
--- @return string|nil  the note, or nil when the last beat did not fail
local function failure_note(events: {shared.Event}, limit: integer, failed: function(shared.ToolPair): boolean): string
    local order = beats_of(events)
    local previous = order[#order]
    if previous == nil then
        return nil
    end

    local called: {string:{string:any}} = {}
    for _, ev in ipairs(previous.events) do
        if ev.kind == "tool_call" then
            local data = data_of(ev)
            if data.call_id ~= nil then
                called[data.call_id as string] = { name = data.name, input = data.args }
            end
        end
    end

    local reasons: {string} = {}
    for _, ev in ipairs(previous.events) do
        local data = data_of(ev)
        if ev.kind == "llm_call_failed" then
            reasons[#reasons + 1] = "the model call did not come off: " .. tostring(data.error)
        elseif ev.kind == "tool_result" then
            local pair = pair_of(previous.id, called, data)
            if failed(pair) then
                reasons[#reasons + 1] = reason_for(pair)
            end
        end
    end

    if #reasons == 0 then
        return nil
    end
    return trim(NOTE_PREFIX .. table.concat(reasons, "; "), limit)
end

--- `request` with `note` as a user message in front of the rest.
---
--- In FRONT, and for a reason that has nothing to do with emphasis: the last
--- messages of a request are where the `tool_use` blocks and the
--- `tool_result` blocks answering them sit, paired by id, and anything
--- inserted among them breaks a pairing the provider rejects the request
--- over. The head of the list is the one position from which a note cannot
--- reach any pair. The request is the Anthropic content-block shape
--- (`knl.fold`'s header), where consecutive same-role messages are combined,
--- so a note in front of a user message costs nothing either.
---
--- The request is rebuilt rather than edited. A filter replaces the request
--- wholesale, and writing into the table it was handed would reach the
--- caller's fold and — through the `llm_request` record — the durable log.
local function prepend_note(request: kernel.Request, note: string): kernel.Request
    local out: {string:any} = {}
    for k, v in pairs(request as {string:any}) do
        out[k] = v
    end
    local messages = setmetatable({ { role = "user", content = note } } as any, ARRAY_TAG as metatable<any>) as {any}
    for _, message in ipairs(request.messages or {}) do
        messages[#messages + 1] = message
    end
    out.messages = messages
    return out as kernel.Request
end

--- Build a BINDER that answers a `filter` carrying the last beat's failure
--- forward.
---
--- Two calls, because a filter's signature has no room for a session and a
--- factory's opts are no place for one (the header): `policy.carry{...}` is
--- the policy, `(session)` binds it to the state it reads.
---
---     local filter = policy.carry({ max_bytes = 512 })(session)
---     local device = knl.device({ llm = llm, filters = { filter } })
---
--- The filter runs after the fold, so what it prepends is in front of a
--- request the fold has already finished building — including a windowed one,
--- where the failing beat may itself have been sliced away and the note is
--- then the only trace of it left.
---
--- WHAT THE DEFAULT CANNOT SEE, and what `failed` is for
---   The kernel closes a tool pair `ok = false` when the handler RAISED, and
---   that flag is the only failure the default reads. A tool that reports a
---   failure by RETURNING one does not trip it: an edit tool handed a line
---   number that is not in the file, answering `{ ok = false, error = "there
---   is no line 300" }`, returned perfectly normally, so its pair closes
---   `ok = true` and the default carries nothing. The next request then shows
---   the model its own call and an answer, with no word that the answer was a
---   rejection — and asking the same wrong thing again is exactly the case
---   this policy exists for.
---
---   The kernel cannot close that gap on the caller's behalf. What a handler
---   returns is the tool's own vocabulary — `ok`, `error`, `status`,
---   `is_error`, a bare string — and no two tools agree on it, so reading it
---   is a judgement only the caller who wired those tools can make. `failed`
---   is where that judgement goes, one predicate over one pair:
---
---       local filter = policy.carry({
---           failed = function(pair) return pair.result and pair.result.ok == false end,
---       })(session)
---
---   It decides for TOOL PAIRS, and for all of them: a pair the kernel closed
---   `ok = false` is put to the same predicate and is carried only if it says
---   so. A model call that did not come off is not a pair and is carried
---   either way. The pair is `policy.shapes.tool_pair`, and what gets carried
---   is built from its `result` the same way in both modes and cut at the one
---   point `max_bytes` bounds.
---
--- @param opts table  { max_bytes? = <whole number >= 1>, failed? = fn(pair) -> boolean }
--- @return function bind  fn(session) -> fn(request) -> request
function M.carry(opts: CarryOpts): M.Bind
    opts = opts_of(opts, { max_bytes = true, failed = true }, "policy.carry")
    if opts.max_bytes ~= nil and not whole_at_least(opts.max_bytes, 1) then
        error("policy.carry: max_bytes must be a whole number >= 1, got " .. tostring(opts.max_bytes), 2)
    end
    if opts.failed ~= nil and type(opts.failed) ~= "function" then
        -- Loud in prod too, like every other bound here: a `failed` that was
        -- not callable would raise out of the FILTER instead, where beat reads
        -- it as `Outcome.err("filter")` and the policy's mistake is reported
        -- as the beat's.
        error("policy.carry: failed must be a function (fn(pair) -> boolean)", 2)
    end
    shape.assert_dev(opts, CARRY_OPTS, "policy.carry opts")

    local limit = opts.max_bytes or DEFAULT_MAX_BYTES
    local failed = opts.failed or default_failed
    return function(session: kernel.Session): M.Filter
        -- Checked where it is bound rather than at the first beat: a filter
        -- that raised on its first call would be reported as a filter
        -- failure, which is not what went wrong.
        needs_session(session, "policy.carry bind")
        return function(request: kernel.Request): kernel.Request
            local note = failure_note(whole_log(session, "policy.carry"), limit, failed)
            if note == nil then
                return request
            end
            return prepend_note(request, note)
        end
    end
end

--- The contracts this file publishes; `policy.shapes` gathers them.
M.shapes = {
    carry_opts = CARRY_OPTS,
}

--- This file's entries in `policy.shapes.api`, gathered there.
M.api = {
    carry = {
        args = { arg_of(CARRY_ARG, "opts") },
        returns = "bind — fn(session) -> filter",
        members = {
            bind = {
                args = { SESSION_ARG },
                returns = "filter — fn(request) -> request",
            },
            filter = {
                args = { arg_of(kernel.shapes.request, "request") },
                returns = kernel.shapes.request,
            },
            failed = {
                args = { arg_of(TOOL_PAIR, "pair") },
                returns = "boolean — carry this pair's result forward",
            },
        },
    },
}

return M
