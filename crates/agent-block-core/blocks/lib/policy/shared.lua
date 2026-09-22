--- policy.shared — what the policy files have in common.
---
--- The defaults, the helpers that read a log, the contract prelude
--- (`opts_contract`, `needs_session`), the shapes more than one policy hands
--- out (`beat_record`, `tool_pair`, `stop_reason`) and the registry's
--- argument forms. Nothing here is a policy and nothing here is exported by
--- `policy` itself: `policy.init` publishes the shapes, and the four policy
--- files (`room`, `tools`, `carry`, `loop`) alias what they use from this
--- table by the names their code was written against.
---
--- Read `policy`'s own header first — the rules about state, the log and the
--- shapes are stated there, once, for every file under it.

local kernel = require("knl")
local lshape = require("lshape")
local T = lshape.t

local S = {}

-- ============================================================
-- Defaults — every threshold is a parameter, and this is where the
-- parameter's default lives
-- ============================================================
--
-- A default is a starting point, not a finding. None of the numbers below is
-- measured, none of them is a law, and every one of them is an opts key a
-- caller overrides without touching this file. They are gathered here rather
-- than written into the factories so the whole set can be read at once and so
-- no number appears twice.

--- How many bytes of note `carry` may prepend. Big enough for a tool's error
--- message and a sentence around it, small enough that a failing beat cannot
--- push the rest of the request out of the way.
local DEFAULT_MAX_BYTES = 512

--- How many beats with the same signature `stagnation` calls "repeated".
--- Two is a legitimate retry — a tool that failed once is worth calling
--- again with the same arguments. Three is the first count at which "again"
--- stops being a retry and starts being a pattern.
local DEFAULT_SAME = 3

--- How many beats that produced nothing `stagnation` calls "no_progress".
--- One empty beat happens; there is no reading under which a second
--- consecutive beat that wrote neither a tool call nor a word of content is
--- the run getting somewhere.
local DEFAULT_NO_PROGRESS = 2

--- How many attempts `retry` allows in total, the first one included. Two
--- retries past the original is the point where a failure the kernel called
--- retryable has stopped looking transient.
local DEFAULT_MAX_ATTEMPTS = 3

--- The event kind a verdict is recorded under when the caller names none.
--- "verify" because that is what every consumer in this tree already
--- appends, and a second name for one thing is a second thing to query.
local DEFAULT_VERDICT_KIND = "verify"

--- How many times one call may be made with nothing reset in between when
--- `repeat_cap` is not told. Twice: once to see, once more in case the
--- first answer fell out of the window; a third time is the loop.
local DEFAULT_REPEAT_MAX = 2

--- How many times what a check took the next one may take, and the least
--- it is ever given, when `verdict{ timeout }` is not told. Three covers a
--- check that builds one more crate than the last did; a minute is the
--- smallest span in which "it is still compiling" and "it is hung" can be
--- told apart at all.
local DEFAULT_TIMEOUT_FACTOR = 3
local DEFAULT_TIMEOUT_FLOOR = 60

--- Which check the next one's seconds are read off, when `verdict{ timeout
--- }` does not say. The longest so far: it assumes nothing about which way
--- a check's time moves. A failing check tends to be quick — a compile
--- error stops it early — and a window read off the quick one cuts the
--- slow, whole pass that follows; but that is a tendency of one kind of
--- repository, and the opposite (a fast green, a slow red at link time)
--- is as real. The longest is the one reading that neither can shrink.
local DEFAULT_TIMEOUT_MEASURE = "longest"

--- How much of the window one tool result may take when `result_cap` is not
--- told. A quarter leaves room for three more of the same size beside the
--- system prompt, the tools and the conversation — enough that a loop can
--- read, edit and read again without the fold running out of beats to drop,
--- and small enough that one answer cannot end the run on its own.
local DEFAULT_RESULT_SHARE = 0.25

--- How deep `canonical` renders a nested value before it stops. A tool input
--- is JSON-shaped and shallow; the cap is what keeps a cyclic hand-built one
--- from taking the signature with it.
local MAX_CANONICAL_DEPTH = 8

--- What a trimmed note ends with, and the only thing `trim` adds. ASCII on
--- purpose: the cut is by BYTES, and a multi-byte marker would be one more
--- thing to get wrong at the boundary.
local ELLIPSIS = "..."

--- What `carry`'s note opens with. One sentence, so the model reads the
--- reason as a statement about the record rather than as an instruction.
local NOTE_PREFIX = "the previous beat did not complete: "

--- The JSON-array tag the bridge's converter honours (`lua_to_json` reads
--- `__jsontype = "array"`), the same one `knl.fold` puts on every array it
--- builds. `carry` rebuilds the messages array, so it re-tags: an array that
--- lost the tag on the way through a filter would cross the boundary as `{}`.
local ARRAY_TAG = { __jsontype = "array" }

-- ============================================================
-- Shared helpers
-- ============================================================

--- Whether `v` can be called like a function (a callable table / userdata
--- counts: a Port shim may hand back either). The same test `knl.device`
--- makes of an `llm`, made here for the same argument.
local function callable(v)
    if type(v) == "function" then
        return true
    end
    local mt = getmetatable(v)
    return type(mt) == "table" and mt.__call ~= nil
end

--- Whether `v` is a whole number of at least `min`.
---
--- lshape has no integer prim and no numeric range, so every threshold in
--- this module carries its type in the shape and its bound here — the same
--- division `knl` makes for `cost`, and checked in prod for the same reason:
--- a window of 0 beats or a retry cap of 0 attempts is a policy that silently
--- does the opposite of what it says.
local function whole_at_least(v, min)
    return type(v) == "number" and v % 1 == 0 and v >= min
end

--- Reject an option this factory does not know.
---
--- Loud, and in prod too. The opts shapes below are closed and asserted in
--- dev, but a dev-only gate would let a mistyped policy through in prod as a
--- silent no-op, which is the failure a policy can least afford: it looks
--- exactly like the policy working and deciding not to act.
---
--- A state key gets the reason rather than the bare complaint — passing a
--- session to a factory is the one wrong guess the design invites.
local function only(opts, allowed, who)
    for k in pairs(opts) do
        if not allowed[k] then
            local hint = ""
            if k == "session" or k == "store" or k == "owner" or k == "budget" then
                hint = " (a session is an argument, never an option — see the header)"
            end
            error(who .. ": unknown option '" .. tostring(k) .. "'" .. hint, 3)
        end
    end
end

--- The session's whole log, or a raise saying it does not fit in one read.
---
--- `session:events()` is bounded and answers `rows, truncated` (knl's header).
--- The cap counts FORWARD, so a truncated read is the front of the log with
--- the newest beats missing — and both readers below are asking about the END
--- of the run. `carry` would build its note from a beat that is not the last
--- one, and `stagnation` would judge repetition and idleness over beats the
--- run has already moved past: two confident wrong answers, and neither of
--- them looks wrong.
---
--- So it refuses. There is no partial reading of "what just happened" that is
--- worth having, and a policy that quietly answered from a stale window would
--- be worse than one that stops — see the header for why a bounded tail read
--- is not the way out either.
---
--- Raised at level 0: the message is the whole of it, and where it surfaces is
--- the policy's caller (beat, for `carry`'s filter; the loop, for
--- `stagnation`), exactly as a read that failed does.
---
--- @param session userdata|table  a knl session
--- @param who string  the policy's name, for the message
--- @return table  the events, in seq order
local function whole_log(session, who)
    local events, truncated = session:events()
    if truncated then
        error(
            who
                .. ": the session's log is longer than one read of it — the kernel's row cap "
                .. "stopped at "
                .. tostring(#events)
                .. " events, so the newest beats are not in what came back and this policy would "
                .. "be judging a run that has already moved on; window the request "
                .. "(policy.window), or start a new session",
            0
        )
    end
    return events
end

--- The beat id a stored event carries, or nil when it is part of no beat.
---
--- The id is a label in the envelope — `meta.beat` — and not a field of its
--- own, and an event need not carry `meta` at all, so every read of it goes
--- through here rather than being spelled out three times.
---
--- @param ev table  a stored event
--- @return string|nil  the id of the beat that wrote it
local function beat_of(ev)
    local meta = ev.meta
    if meta == nil then
        return nil
    end
    return meta.beat
end

--- The beats of an event list, in the order they first appear:
--- `{ { id, events }, ... }` — `policy.shapes.beat_record`.
---
--- Events with no `meta.beat` are part of no beat and are left out; the
--- kernel's own boundaries and the caller's seed are log, not beat. Grouping
--- is by the id rather than by adjacency, so a log whose beats were
--- interleaved (two drivers on one session) still reads back as whole beats.
---
--- @param events table|nil  a session's events, in seq order
--- @return table  an array of beat records
local function beats_of(events)
    local order, by_id = {}, {}
    for _, ev in ipairs(events or {}) do
        local id = beat_of(ev)
        if id ~= nil then
            local record = by_id[id]
            if record == nil then
                record = { id = id, events = {} }
                by_id[id] = record
                order[#order + 1] = record
            end
            record.events[#record.events + 1] = ev
        end
    end
    return order
end

--- A value as a deterministic string: sorted keys, recursively.
---
--- The default signature compares tool inputs, so the rendering has to be
--- stable across beats — and `pairs` is not. JSON encoding is not the answer
--- either: `std.json.encode` walks a table in whatever order `pairs` gives
--- it, so two identical inputs can render two ways, and it is a host global
--- this module has no other reason to need.
---
--- @param value any
--- @param depth number|nil  how far down this call already is
--- @return string
local function canonical(value, depth)
    depth = depth or 0
    local t = type(value)
    if t == "string" then
        return string.format("%q", value)
    elseif t == "number" or t == "boolean" or t == "nil" then
        return tostring(value)
    elseif t ~= "table" then
        -- A function / userdata in a tool input is not content; naming its
        -- type is everything a comparison can honestly say about it.
        return "<" .. t .. ">"
    end
    if depth >= MAX_CANONICAL_DEPTH then
        return "<deep>"
    end
    local keys = {}
    for k in pairs(value) do
        keys[#keys + 1] = k
    end
    table.sort(keys, function(a, b)
        return tostring(a) < tostring(b)
    end)
    local parts = {}
    for _, k in ipairs(keys) do
        parts[#parts + 1] = tostring(k) .. "=" .. canonical(value[k], depth + 1)
    end
    return "{" .. table.concat(parts, ",") .. "}"
end

--- An event's `data`, or an empty table when it carried none.
---
--- A malformed record is read as an empty one rather than indexed: every
--- reader below walks a whole log, and one bad event must not take the
--- verdict with it.
local function data_of(ev)
    return type(ev.data) == "table" and ev.data or {}
end

-- ============================================================
-- The contract prelude — what every policy's opts are declared with
-- ============================================================
--
-- Every factory's opts and every function a factory hands back is declared
-- in the file that owns the policy and published through `policy.shapes`,
-- so a caller reads the contract as data. `policy.shapes.api` names the
-- shape of every argument, and the dev-mode gate at the foot of
-- `policy/init.lua` runs it.
--
-- The shapes that describe a knl value are the kernel's own — `event_base`,
-- `request`, `outcome`, `error_kinds` — reached through `kernel.shapes`
-- rather than retyped. A second copy of a contract is a contract with two
-- versions.

--- A Lua function, as a shape. `lshape.t` exposes only the five prims it
--- names, so this is built from the same plain-data schema form.
local FUNCTION = setmetatable({ kind = "prim", prim = "function" }, lshape.t._internal.schema_mt)

--- Something a beat can call, and a session handle: the kernel's, not a
--- second copy. Both used to be written here as well, in the same `any_of`
--- form and with the same words around them, which is a contract with two
--- versions waiting to disagree.
local CALLABLE = kernel.shapes.callable
local SESSION_HANDLE = kernel.shapes.session_handle

--- Hold `session` to being one — the kernel's judgement, not a local retelling
--- of it.
---
--- `knl.is_session` asks the handle for the whole of `knl.shapes.session`,
--- which is generated from the Rust side's own types, through a pcall because
--- the real handle is userdata whose indexing can raise. Every version of this
--- question written here instead asked a smaller one: a single method, or
--- `type(session) == "table"` — and that last one refused a perfectly good
--- session in a run, before its first beat. There is one answer and it lives
--- with the declaration.
---
--- @param session any  the value a caller passed
--- @param who string  the policy's name, for the message
local function needs_session(session, who)
    if not kernel.is_session(session) then
        error(who .. ": session must be a knl session (from knl.open / knl.resume)", 3)
    end
end

--- An opts contract, as the two shapes it has to be.
---
--- CLOSED is the published one (`policy.shapes.*_opts`) and the one the
--- factory asserts in dev: an option this module does not know is a policy
--- typo, and a typo that quietly became a no-op is the failure a policy can
--- least afford — it looks exactly like the policy working and deciding not
--- to act.
---
--- OPEN is the same fields with that one judgement removed, and it exists for
--- the dev-mode registry gate alone. The gate wraps the export, so whatever it
--- judges it judges FIRST, and a closed shape there would make it the thing
--- that answers an unknown option — in dev only, with a message about a shape
--- violation instead of the one `only` writes, which names the option and says
--- why a session is never one. That is a module with two behaviours for one
--- mistake, split by an environment variable, and it is exactly what this
--- module's header promises it does not have.
---
--- So the judgement lives in one place. `only` owns "is this key declared at
--- all" and is loud in both modes; the gate owns "are the declared keys the
--- right shape" and is welcome to be dev-only, because every bound that
--- actually matters (`tail >= 1`, a callable `strong`, a `kinds` the kernel
--- publishes) is checked beside it in prod too.
---
--- @param fields table  the field name -> schema map, written once
--- @return table closed  the published contract
--- @return table open  the same fields, for the registry
local function opts_contract(fields)
    return T.shape(fields, { open = false }), T.shape(fields)
end

--- One beat, as this module derives it from the log: the id the kernel
--- stamped, and the events carrying it in the order they were written. This
--- is what a custom `signature` is handed.
local BEAT_RECORD = T.shape({
    id = T.string,
    events = T.array_of(kernel.shapes.event_base),
}, { open = false })

--- One answered tool call, as `carry` derives it from the log and hands it to
--- a caller's `failed`.
---
--- The four fields a caller decides on are the ones `knl.views.tool_pairs`
--- names — `name`, `input`, `result`, `ok` — with the `call_id` and the `beat`
--- they were written under beside them. The view is read from the store and
--- carries the identifying half; this pair is read from the log and carries
--- the `input` the call was made with and the `result` the handler produced,
--- because whether a returned value is a failure cannot be decided without
--- them.
---
--- `call_id`, `name` and `input` are optional because a `tool_result` whose
--- `tool_call` is not in the log has nothing to take them from — an
--- interrupted beat leaves exactly that.
local TOOL_PAIR = T.shape({
    beat = T.string,
    call_id = T.string:is_optional(),
    name = T.string:is_optional(),
    input = T.any:is_optional(),
    result = T.any:is_optional(),
    ok = T.boolean,
}, { open = false })

--- What a policy answers when it has a verdict: `stagnation`'s two, and
--- `window{ fit }`'s one. Each word is one policy's, and a word nobody
--- answers would be a policy nobody wrote.
local STOP_REASON = T.one_of({ "repeated", "no_progress", "context" })

-- ============================================================
-- The registry's argument forms
-- ============================================================

--- One declared argument: the shape it is held to, and the word for it.
local function arg_of(schema, desc)
    return { shape = schema, desc = desc }
end

local EVENTS_ARG = arg_of(T.array_of(kernel.shapes.event_base), "events")
local SESSION_ARG = arg_of(SESSION_HANDLE, "session")
local OUTCOME_ARG = arg_of(kernel.shapes.outcome, "outcome")

-- ============================================================
-- What the siblings take
-- ============================================================

S.DEFAULT_MAX_BYTES = DEFAULT_MAX_BYTES
S.DEFAULT_SAME = DEFAULT_SAME
S.DEFAULT_NO_PROGRESS = DEFAULT_NO_PROGRESS
S.DEFAULT_MAX_ATTEMPTS = DEFAULT_MAX_ATTEMPTS
S.DEFAULT_VERDICT_KIND = DEFAULT_VERDICT_KIND
S.DEFAULT_REPEAT_MAX = DEFAULT_REPEAT_MAX
S.DEFAULT_TIMEOUT_FACTOR = DEFAULT_TIMEOUT_FACTOR
S.DEFAULT_TIMEOUT_FLOOR = DEFAULT_TIMEOUT_FLOOR
S.DEFAULT_TIMEOUT_MEASURE = DEFAULT_TIMEOUT_MEASURE
S.DEFAULT_RESULT_SHARE = DEFAULT_RESULT_SHARE
S.ELLIPSIS = ELLIPSIS
S.NOTE_PREFIX = NOTE_PREFIX
S.ARRAY_TAG = ARRAY_TAG
S.callable = callable
S.whole_at_least = whole_at_least
S.only = only
S.whole_log = whole_log
S.beat_of = beat_of
S.beats_of = beats_of
S.canonical = canonical
S.data_of = data_of
S.FUNCTION = FUNCTION
S.CALLABLE = CALLABLE
S.SESSION_HANDLE = SESSION_HANDLE
S.needs_session = needs_session
S.opts_contract = opts_contract
S.BEAT_RECORD = BEAT_RECORD
S.TOOL_PAIR = TOOL_PAIR
S.STOP_REASON = STOP_REASON
S.arg_of = arg_of
S.EVENTS_ARG = EVENTS_ARG
S.SESSION_ARG = SESSION_ARG
S.OUTCOME_ARG = OUTCOME_ARG

return S
