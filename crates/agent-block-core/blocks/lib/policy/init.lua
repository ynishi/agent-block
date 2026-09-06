--- policy — the values a caller's loop plugs into a knl device.
---
--- What this is
---   The kernel provides one BEAT and holds no loop. `knl.beat(session,
---   device)` runs one model call plus the tools that call asked for and
---   hands back an `Outcome`; composing beats is the caller's, on the spot.
---   A POLICY is a value that plugs into one of the device's seams — `fold`,
---   a `filter`, `cost`, `tool_policy`, `llm` — or that the caller's loop
---   consults between beats. It never runs a loop of its own, and the only
---   thing it does with the kernel's log is READ it: nothing here appends,
---   reserves, spends or closes.
---
---   Policies are opt-in, one at a time. A device built without them is the
---   plain kernel, behaving exactly as it did before this module existed —
---   which is what keeps the kernel free of the shell's habits rather than
---   growing them a beat at a time.
---
--- The six, and where each one plugs in
---
---     policy.window      -> a `fold`      the last n beats, or as many as
---                                         fit the model's window
---     policy.carry       -> a `filter`    one bounded note about the beat
---                                         that failed
---     policy.stagnation  -> a predicate   the loop asks it between beats:
---                                         is this run going in circles?
---     policy.retry       -> a predicate   the loop asks it about an Outcome:
---                                         is this failure worth asking again?
---     policy.escalate    -> `next`        the device for the next beat: this
---                                         one, or one with a stronger llm
---     policy.tokens      -> a `cost`      the beat's request in tokens, by
---                                         the Port's count, for a grant
---                                         tagged "tokens"
---
---   `window`, `carry` and `tokens` are device fields (`knl.device{ fold =
---   ..., filters = { ... }, cost = ... }`); `stagnation`, `retry` and
---   `escalate` are the loop's own and the kernel never sees them. That
---   split is the whole shape of this module: a policy either changes what
---   one beat SENDS (or reserves), or it decides what the loop does BETWEEN
---   beats. Nothing here decides what a beat does while it runs — that is the
---   kernel's, and it is not a seam.
---
--- One shell, two packs — and where a model's limits go
---   The shell above the kernel is this module and `supervisor`, and the two
---   are a SET: a loop is composed from both — policies on the device and
---   between beats, the supervisor for the session tree — and neither is a
---   loop by itself. A third pack beside them is not how the shell grows.
---
---   In particular, a model's limits are not a pack. A narrow context window,
---   a model that re-reads what it has already seen, one that cannot copy an
---   `expect` exactly, one that stalls on a hard error, a task too large for
---   one loop — each of these is answered where the kernel already left a
---   seam for it, and by what the Port already knows:
---
---     the window            the Port declares it (`LLMPort:profile`), and
---                           `window{ fit }` / `tokens` ask it
---     what a tool may do    `tool_policy` on the device, reading the log
---     what a request says   `fold` (what is sent) and `filters` (a note)
---     when to stop          a predicate the loop asks (`stagnation`, and
---                           its siblings a loop may write)
---     too big for one loop  `supervisor.child` / `parallel` / `merge`
---
---   The thresholds a particular model wants (how many bytes a read may
---   return, how many times a range may be re-read) are opts on those
---   factories, and the model's name never appears in this module. Before a
---   new module is written for a model, the question is whether the kernel
---   lacks a seam (then the kernel is the change) or whether the Port lacks a
---   declaration (then the adapter is) — and one of the two has so far been
---   the answer every time.
---
--- Opts are policy, the session is an argument
---   `knl.device` and `knl.open` split policy from state: a device holds
---   `llm` / `tools` / `fold` / `filters` and refuses `owner` / `store` /
---   `session`, because a device is a frozen value that several sessions can
---   share while a session is durable state one kernel owns. This module
---   keeps the same line, and it is the answer to the one question every
---   log-reading policy raises — how does it reach the log?
---
---     * a factory's opts are POLICY. A session in them would be a typo, and
---       is refused as one;
---     * the session arrives as an ARGUMENT, in whatever signature the
---       returned value already has.
---
---   For `window` that argument is `events`: a fold is handed the log by
---   beat, so it never needs a handle. For `stagnation` it is `session`: the
---   predicate the loop calls takes one. `carry` is the only one whose
---   returned signature has no room for it — a filter is `fn(request) ->
---   request` and the kernel will pass nothing else — so the factory answers
---   a BINDER instead:
---
---       local device = knl.device({
---           llm = llm,
---           fold = policy.window({ tail = 4 }),
---           filters = { policy.carry({ max_bytes = 512 })(session) },
---       })
---
---   `policy.carry{...}` is a session-free value a caller can hold and bind
---   to whichever session it is driving, exactly as one device is shared
---   across sessions. The alternative — `policy.carry{ session = s }` — would
---   put state in a policy constructor's config, which is the one thing the
---   kernel's own constructors are written to refuse.
---
--- No policy holds state
---   Nothing in this module remembers anything between beats: not in the
---   module, not in a factory's closure. A factory closure holds only what it
---   was configured with (`tail`, `max_bytes`, the thresholds, the strong
---   llm), and those are frozen at construction the way a device's fields
---   are. Everything else is derived from the log on every call, so two
---   processes reading the same session reach the same verdict and a resumed
---   session does not start counting from zero.
---
---   Which of them read the log, and which read nothing:
---
---     window      reads the log — the `events` beat handed it
---     carry       reads the log — `session:events()`, through the binder
---     stagnation  reads the log — `session:events()`, per call
---     retry       reads NO log: the `Outcome` it is given, plus `attempt`,
---                 which is the caller's own count and is passed in
---     escalate    reads NO log: the `Outcome` it is given
---
---   The escape hatch the design allows — an explicit `run` table the CALLER
---   creates and owns for one shell run — is not used by any of the five,
---   because nothing any of them needs is missing from the log or from an
---   argument. If a later policy does need one, it takes that table as an
---   argument like any other; a module-level global would be the same state
---   with nobody owning it.
---
--- Reading the log: `session:events()`, not a query view
---   `carry` and `stagnation` both read `session:events()` rather than
---   `knl.views.tool_pairs` / `knl.views.beats`, and it is a choice rather
---   than an oversight. Neither question is answerable from those views:
---   `llm_call_failed` is not a tool pair and has no row in `tool_pairs`, and
---   a beat that made no tool call at all — the very thing `no_progress` is
---   about — is exactly the beat that leaves no row behind. A view would
---   answer half of each question and the log would still have to be read for
---   the other half, which is two reads and two ways to be wrong.
---
---   A read that fails is not caught here. A closed session or a store that
---   will not answer raises out of `session:events()`, and the raise is
---   reported where the policy was called from: for `carry` that is beat,
---   which turns a raising filter into `Outcome.err("filter")`. Swallowing it
---   would hide a dead store behind a policy that quietly does nothing.
---
---   A read that was CUT SHORT is refused the same way (`whole_log`). The
---   kernel's read is bounded and answers `rows, truncated`, and the cap counts
---   forward — so a truncated read is the FRONT of the log and both of these
---   policies are asking about its end. Folding one would let `carry` build a
---   note from a beat that is not the last, and `stagnation` judge repetition
---   over beats the run has already left behind: wrong answers that look right.
---
---   A BOUNDED TAIL READ WOULD NOT FIX IT, which is why neither policy takes
---   one. `session:view("tail", n)` bounds EVENTS and both of these reason in
---   BEATS, and a beat writes as many events as the model asked for tool calls:
---   no `n` is a beat count, and a window can begin in the middle of a beat.
---   `beats_of` cannot tell that from a whole one, so `failure_note` would
---   report "a tool call failed" for a pair whose `tool_call` half was outside
---   the window, and `made_progress` would call a beat idle whose only
---   `tool_call` was sliced off — the same hole, moved somewhere quieter. The
---   query views are ruled out above for reasons of their own. A run long
---   enough to hit the cap wants a window on the request (`policy.window`) or
---   a fresh session, and that is a caller's decision, so the policy says so
---   and stops.
---
--- Beats, as this module sees them
---   The kernel stamps a `beat` id on every event one beat writes and does
---   not number them (`knl`'s header). So a beat, here, is derived: the
---   events carrying one id, in the order they were written, as
---   `{ id = <string>, events = { ... } }` — `policy.shapes.beat_record`.
---   That record is what a custom `signature` is handed, and it is the only
---   place this module names a structure of its own.
---
---   Events with no `beat` — the caller's seed, `session_*`, `budget_*` — are
---   part of no beat and are not in any record. They are still part of the
---   LOG, which is why `window` slices the event list rather than the beat
---   list.
---
--- The shapes are declared and the registry is executed
---   Every public interface here — each factory's opts, and the arguments and
---   return of every function a factory hands back — is an lshape published
---   through `policy.shapes`, and `policy.shapes.api` names the shape of
---   every argument of every export. In dev mode (LSHAPE_CHECK) each declared
---   export is wrapped once, at load, by a gate that holds the call to its
---   entry; prod installs no wrapper and pays nothing. This is the same
---   arrangement `knl.shapes` has and for the same reason: a registry nobody
---   runs is prose with a table around it.
---
---   Because the gate is dev-only, every check a call must not get through
---   WITHOUT is written beside it as an explicit check and is loud in both
---   modes — an unknown option, a threshold that is not a whole number, an
---   `llm` that cannot be called. A policy built out of a mistyped config
---   must fail at the line that built it, not at the beat that used it.
---
---   And a check that is loud in both modes must not be ANSWERED by the gate,
---   which is the subtler half of the same rule. The gate wraps the export, so
---   whatever it judges it judges first; if it were handed the closed opts
---   shape it would be the thing that reports an unknown option, in dev only,
---   in different words. So the two judgements are split and each has one
---   owner: `only` says whether a key is declared (both modes, and it is the
---   message a caller reads), the registry says whether the declared keys have
---   the right shape (dev only). `opts_contract` is where that split is made.
---
--- Deliberately not here
---   Summarising a window instead of dropping it, a cost policy, a
---   `tool_policy` gate, parallel or speculative beats, and any policy that
---   would need to write to the log to work. Each is deferred until a real
---   loop asks for it.

local kernel = require("knl")
local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

local M = {}

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

--- The beats of an event list, in the order they first appear:
--- `{ { id, events }, ... }` — `policy.shapes.beat_record`.
---
--- Events with no `beat` are part of no beat and are left out; the kernel's
--- own boundaries and the caller's seed are log, not beat. Grouping is by the
--- id rather than by adjacency, so a log whose beats were interleaved (two
--- drivers on one session) still reads back as whole beats.
---
--- @param events table|nil  a session's events, in seq order
--- @return table  an array of beat records
local function beats_of(events)
    local order, by_id = {}, {}
    for _, ev in ipairs(events or {}) do
        local id = ev.beat
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
-- shapes — the public contracts of this module
-- ============================================================
--
-- Every factory's opts and every function a factory hands back is declared
-- here and published through `M.shapes`, so a caller reads the contract as
-- data. `M.shapes.api` (further down) names the shape of every argument, and
-- the dev-mode gate at the foot of this file runs it.
--
-- The shapes that describe a knl value are the kernel's own — `event_base`,
-- `request`, `outcome`, `error_kinds` — reached through `kernel.shapes`
-- rather than retyped. A second copy of a contract is a contract with two
-- versions.

--- A Lua function, as a shape. `lshape.t` exposes only the five prims it
--- names, so this is built from the same plain-data schema form.
local FUNCTION = setmetatable({ kind = "prim", prim = "function" }, lshape.t._internal.schema_mt)
local USERDATA = setmetatable({ kind = "prim", prim = "userdata" }, lshape.t._internal.schema_mt)

--- Something a beat can call: a function, or a table / userdata carrying
--- `__call`. `callable` above is the exact test, run loudly at construction;
--- this is its data shape.
local CALLABLE = T.any_of({ FUNCTION, T.table, USERDATA })

--- A session handle: the kernel's userdata, or the faithful Lua stand-in a
--- spec drives. No schema can ask a userdata what it can do, so the shape
--- says the two types it can have and the binder makes the real judgement.
local SESSION_HANDLE = T.any_of({ T.table, USERDATA })

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

--- What `policy.window` is configured with.
local WINDOW_OPTS, WINDOW_ARG = opts_contract({
    tail = T.number:describe("how many beats the request keeps; a whole number >= 1"):is_optional(),
    keep_seed = T.boolean
        :describe("also keep every event before the first beat (the caller's seed), ahead of the window; default false")
        :is_optional(),
    fit = T.table
        :describe(
            "{ port, conf? }: keep as many beats as fit the port's context window — port:count(request, conf) + profile.max_output <= profile.context_window"
        )
        :is_optional(),
})

--- What `policy.tokens` is configured with: the Port whose counting the cost
--- delegates to, and the conf that Port was (or will be) opened with.
local TOKENS_OPTS, TOKENS_ARG = opts_contract({
    port = T.table:describe("an LLM Port: anything answering count(request, conf) -> integer"),
    conf = T.table:describe("the conf the port is opened with; forwarded to count"):is_optional(),
})

--- What `policy.carry` is configured with. `failed` is the caller's reading of
--- a tool pair, for the failures the kernel's `ok` flag cannot see.
local CARRY_OPTS, CARRY_ARG = opts_contract({
    max_bytes = T.number:describe("the note's whole length in bytes; a whole number >= 1"):is_optional(),
    failed = T.fn:describe("fn(pair) -> boolean; default: the pair's ok flag"):is_optional(),
})

--- What `policy.stagnation` is configured with. Both thresholds count beats,
--- and `signature` decides what "the same" means for the channel being run.
local STAGNATION_OPTS, STAGNATION_ARG = opts_contract({
    same = T.number:describe("beats with one signature that count as repeated; a whole number >= 2"):is_optional(),
    no_progress = T.number:describe("beats that produced nothing in a row; a whole number >= 1"):is_optional(),
    signature = FUNCTION:is_optional(),
})

--- The two failure vocabularies a retry decides on, as one list.
---
--- `knl.shapes.error_kinds` classifies a KERNEL failure (a contended store, a
--- closed session) and `knl.shapes.call_error_kinds` classifies a MODEL CALL
--- that did not come off (a rate limit, an overloaded provider, a connection
--- that dropped). They are separate on purpose — a busy store and a busy
--- provider are not the same failure — but they meet in one field: both ride
--- in `detail.kind`, which is what lets one predicate read both.
---
--- So `kinds` closes on the union rather than on either half. Naming
--- `rate_limited` used to be a construction error, which meant no retry policy
--- could be written for the class of failure most often worth asking again
--- about.
---
--- Both lists are read from `knl` rather than retyped: a class added on either
--- side is available here the moment it lands.
local RETRY_KINDS = {}
do
    local seen = {}
    for _, list in ipairs({ kernel.shapes.error_kinds, kernel.shapes.call_error_kinds }) do
        for _, kind in ipairs(list) do
            if not seen[kind] then
                seen[kind] = true
                RETRY_KINDS[#RETRY_KINDS + 1] = kind
            end
        end
    end
end

--- What `policy.retry` is configured with.
local RETRY_OPTS, RETRY_ARG = opts_contract({
    kinds = T.array_of(T.one_of(RETRY_KINDS)):is_optional(),
    max = T.number:describe("attempts in total, the first included; a whole number >= 1"):is_optional(),
})

--- What `policy.escalate` is configured with. `strong` is required — an
--- escalation with nothing to escalate TO is not a policy.
local ESCALATE_OPTS, ESCALATE_ARG = opts_contract({
    strong = CALLABLE,
    when = FUNCTION:is_optional(),
})

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

M.shapes = {
    window_opts = WINDOW_OPTS,
    carry_opts = CARRY_OPTS,
    stagnation_opts = STAGNATION_OPTS,
    retry_opts = RETRY_OPTS,
    escalate_opts = ESCALATE_OPTS,
    beat_record = BEAT_RECORD,
    tool_pair = TOOL_PAIR,
    stop_reason = STOP_REASON,
}

-- ============================================================
-- window — a fold over the last n beats
-- ============================================================

--- The tail of `events` beginning at the first event of the n-th beat from
--- the end.
---
--- The cut is by BEAT, never by a count of events, and that is the whole
--- point of the function. A beat writes `llm_request`, `llm_response` and
--- then its tool pairs; cutting anywhere inside that leaves an assistant
--- message whose `tool_use` blocks have no answering `tool_result` — the very
--- state `knl.fold`'s repair exists to paper over on a crashed run, and there
--- is no reason to manufacture it on purpose. Cutting at the first event of a
--- beat can only ever produce whole beats.
---
--- What falls outside the window falls outside it entirely, unstamped events
--- included: an earlier seed, the session's own opening, the ledger. Keeping
--- those and dropping only the beats would make the request say something the
--- log does not — a conversation that begins where it began and then skips.
---
--- `keep_seed` asks for exactly that skip, on purpose. Everything before the
--- first beat — the seed the caller opened the session with — stays ahead of
--- the window, and the beats between it and the window are what goes. A loop
--- whose task is stated in its seed and that runs for more beats than fit has
--- no other way to keep the task in the request: a window without the seed
--- forgets what it was asked. The request then does say something the log
--- does not, and a fold that wanted to say so could add a note; this one
--- does not, because the models this is for follow the seed either way.
---
--- A log with `tail` beats or fewer is not cut at all, and the same list is
--- handed back rather than copied.
---
--- @param events table|nil  a session's events, in seq order
--- @param tail number  how many beats to keep
--- @param keep_seed boolean|nil  keep the events before the first beat too
--- @return table  the slice, in seq order
local function window_slice(events, tail, keep_seed)
    events = events or {}
    local order, first_at = {}, {}
    for i, ev in ipairs(events) do
        local id = ev.beat
        if id ~= nil and first_at[id] == nil then
            first_at[id] = i
            order[#order + 1] = id
        end
    end
    if #order <= tail then
        return events
    end
    -- `tail == 0` keeps no beat at all: the slice is the seed (with
    -- `keep_seed`) or nothing. Only `fit` asks for it, as the last candidate.
    local from = tail == 0 and (#events + 1) or first_at[order[#order - tail + 1]]
    local slice = {}
    if keep_seed then
        for i = 1, first_at[order[1]] - 1 do
            slice[#slice + 1] = events[i]
        end
    end
    for i = from, #events do
        slice[#slice + 1] = events[i]
    end
    return slice
end

--- How many beats `events` holds.
local function beat_count(events)
    local seen, n = {}, 0
    for _, ev in ipairs(events or {}) do
        local id = ev.beat
        if id ~= nil and not seen[id] then
            seen[id] = true
            n = n + 1
        end
    end
    return n
end

--- What a Port must answer for `fit`, checked at construction so a window that
--- cannot count is refused before a beat asks it to.
local function port_for_fit(fit, who)
    if type(fit) ~= "table" or type(fit.port) ~= "table" then
        error(who .. ": fit must be { port = <Port>, conf? }", 3)
    end
    if type(fit.port.count) ~= "function" or type(fit.port.profile) ~= "function" then
        error(who .. ": fit.port must answer count(request, conf) and profile(conf)", 3)
    end
    if fit.conf ~= nil and type(fit.conf) ~= "table" then
        error(who .. ": fit.conf must be a table when given", 3)
    end
    return fit.port, fit.conf
end

--- The tokens a request may take, read off the Port's profile: the window
--- less the room the answer needs. Loud when the profile does not say —
--- a window that guessed would be the 400 it exists to prevent, one step
--- later.
local function request_limit(profile, who)
    if type(profile) ~= "table" then
        error(who .. ": port:profile(conf) must answer a table, got " .. tostring(profile), 3)
    end
    local window, output = profile.context_window, profile.max_output
    if not whole_at_least(window, 1) then
        error(
            who
                .. ": the port's profile names no context_window; declare it in the conf the port is opened with "
                .. "(context_window = <tokens>) or on the port",
            3
        )
    end
    if output == nil then
        output = 0
    elseif not whole_at_least(output, 0) then
        error(who .. ": profile.max_output must be a whole number >= 0, got " .. tostring(output), 3)
    end
    if output >= window then
        error(
            string.format("%s: profile.max_output (%d) leaves no room in context_window (%d)", who, output, window),
            3
        )
    end
    return window - output
end

--- Build a `fold` that folds the last `tail` beats of the log.
---
--- The fold it answers is the kernel's own, run over a shorter list: it slices
--- and then calls `knl.fold`, so message assembly, the tool-pair repair and
--- the JSON-array tagging are the kernel's single implementation of them and
--- not a second one that will drift. `system` and `tools` are untouched —
--- they are composed from the device on every fold and were never in the log
--- to begin with.
---
---     knl.device({ llm = llm, fold = policy.window({ tail = 4 }) })
---
--- With `keep_seed = true` the events before the first beat stay in the
--- request ahead of the window (see `window_slice`): the task stated in the
--- seed survives however many beats the loop runs.
---
---     knl.device({ llm = llm, fold = policy.window({ tail = 4, keep_seed = true }) })
---
--- With `fit = { port, conf? }` the window is sized by the model rather than
--- by a number of beats: the fold keeps as many of the last beats as the
--- Port's context window has room for, counting each candidate request with
--- `port:count(request, conf)` against `port:profile(conf).context_window -
--- max_output`. Whole beats go, oldest first, `tail` (if given) is a cap on
--- top, and the seed is the last thing standing when `keep_seed` is set. A
--- request that does not fit even then raises: the loop was going to be
--- refused by the server one step later, and this is the step that can say
--- why. The counting and the window are the Port's — this fold knows no
--- model.
---
---     knl.device({ llm = port:open(conf), fold = policy.window({ fit = { port = port, conf = conf }, keep_seed = true }) })
---
--- The fold raising is the last resort and not the intended one. A loop that
--- would rather stop than fail asks the SECOND value `fit` hands back —
--- a predicate in `stagnation`'s form, answering `"context"` when the next
--- request would not fit and `nil` when it would:
---
---     local fold, fits = policy.window({ fit = { port = port, conf = conf }, keep_seed = true })
---     local device = knl.device({ llm = port:open(conf), fold = fold })
---     ...
---     if fits(session, device) then break end   -- a planned stop, before the beat
---     local out = knl.beat(session, device)
---
--- The two share one implementation and the Port's count cache, so asking
--- costs the fold it was going to do anyway. Without `fit` the second value
--- is nil: a window of n beats always fits something.
---
--- @param opts table  { tail = <whole number >= 1>?, keep_seed = <boolean>?, fit = { port, conf? }? } — `tail` is required without `fit`
--- @return function fold  fn(events, device) -> request
--- @return function|nil fits  fn(session, device) -> nil | "context" (with `fit` only)
function M.window(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.window: opts must be a table", 2)
    end
    only(opts, { tail = true, keep_seed = true, fit = true }, "policy.window")
    if opts.fit == nil and not whole_at_least(opts.tail, 1) then
        error("policy.window: tail must be a whole number >= 1, got " .. tostring(opts.tail), 2)
    end
    if opts.tail ~= nil and not whole_at_least(opts.tail, 1) then
        error("policy.window: tail must be a whole number >= 1, got " .. tostring(opts.tail), 2)
    end
    if opts.keep_seed ~= nil and type(opts.keep_seed) ~= "boolean" then
        error("policy.window: keep_seed must be a boolean, got " .. tostring(opts.keep_seed), 2)
    end
    local port, conf
    if opts.fit ~= nil then
        port, conf = port_for_fit(opts.fit, "policy.window")
    end
    shape.assert_dev(opts, WINDOW_OPTS, "policy.window opts")

    local tail = opts.tail
    local keep_seed = opts.keep_seed == true
    if port == nil then
        return function(events, device)
            return kernel.fold(window_slice(events, tail, keep_seed), device)
        end
    end

    -- The profile is read per fold, not captured: a Port opened with one conf
    -- answers one profile, and reading it each time costs nothing while
    -- letting a Port whose window is learned late (a served model queried
    -- for it) answer the truth.
    local floor = keep_seed and 0 or 1

    --- The largest window that fits, or nil and what the smallest one cost.
    --- Whole beats go, oldest first: the candidates are nested, so the search
    --- is over a monotone predicate and a bisection finds the same answer as
    --- the walk in log candidates rather than all of them — which matters
    --- because each candidate is a fold and, on a Port with a server to ask,
    --- a count the first time it is seen.
    local function largest_fitting(events, device)
        local limit = request_limit(port:profile(conf), "policy.window")
        local n = beat_count(events)
        local most = tail and math.min(tail, n) or n

        local function fold_at(k)
            local request = kernel.fold(window_slice(events, k, keep_seed), device)
            local tokens = port:count(request, conf)
            if type(tokens) ~= "number" then
                error("policy.window: port:count must answer a number, got " .. tostring(tokens), 3)
            end
            return request, tokens
        end

        local whole, whole_tokens = fold_at(most)
        if whole_tokens <= limit then
            return whole
        end

        -- Everything fits at `lo` or below and nothing at `hi` or above;
        -- `floor` is the smallest window there is, and it has already failed
        -- when the loop ends without an answer.
        local lo, hi = floor, most
        local best, smallest_tokens = nil, nil
        while lo <= hi do
            local mid = (lo + hi) // 2
            local request, tokens = fold_at(mid)
            if tokens <= limit then
                best = request
                lo = mid + 1
            else
                hi = mid - 1
                if mid == floor then
                    smallest_tokens = tokens
                end
            end
        end
        if best then
            return best
        end
        if smallest_tokens == nil then
            local _, tokens = fold_at(floor)
            smallest_tokens = tokens
        end
        return nil, smallest_tokens, limit
    end

    local fold = function(events, device)
        local request, tokens, limit = largest_fitting(events, device)
        if request then
            return request
        end
        error(
            string.format(
                "policy.window: nothing left to drop and the request still does not fit: %d tokens > %d (the seed%s). "
                    .. "Ask the second value this factory answers before the beat to stop instead of failing.",
                tokens,
                limit,
                keep_seed and " alone" or " is not kept; the newest beat alone"
            ),
            2
        )
    end

    --- The same question, asked before the beat rather than inside it.
    local fits = function(session, device)
        if type(session) ~= "table" or type(session.events) ~= "function" then
            error("policy.window fits: session must be a knl session", 2)
        end
        local request = largest_fitting(whole_log(session, "policy.window fits"), device or {})
        if request == nil then
            return "context"
        end
        return nil
    end

    return fold, fits
end

-- ============================================================
-- tokens — a cost in tokens
-- ============================================================

--- Build a `cost` that reserves a beat's request in tokens.
---
--- The kernel's budget is a quota in whatever unit the owner tagged the grant
--- with, and `device.cost(request)` is how many of that unit one beat asks
--- for before its call; the default is one, so a grant counts beats. This
--- answers the request's token count instead, so that
---
---     knl.session({ budget = { amount = 200000, tag = "tokens" } }, function(s)
---         local device = knl.device({ llm = port:open(conf), cost = policy.tokens({ port = port, conf = conf }) })
---
--- counts tokens: the balance is what the session may still send, a beat
--- that would overrun it is `Outcome.stopped("budget", "tokens")` with no
--- call made, and nothing about token usage is folded back from the answer —
--- `knl.views.usage` is the provider's accounting of what a call cost, read
--- for what it is, and not a second ledger.
---
--- The number is the Port's (`port:count(request, conf)`), the same count
--- `policy.window{ fit }` sizes the request by, so the two agree by
--- construction. The kernel requires a cost of at least one; an empty
--- request still spends a beat.
---
--- @param opts table  { port = <Port answering count(request, conf)>, conf = <table>? }
--- @return function cost  fn(request) -> integer >= 1
function M.tokens(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.tokens: opts must be a table", 2)
    end
    only(opts, { port = true, conf = true }, "policy.tokens")
    if type(opts.port) ~= "table" or type(opts.port.count) ~= "function" then
        error("policy.tokens: port must be a table answering count(request, conf)", 2)
    end
    if opts.conf ~= nil and type(opts.conf) ~= "table" then
        error("policy.tokens: conf must be a table when given", 2)
    end
    shape.assert_dev(opts, TOKENS_OPTS, "policy.tokens opts")

    local port, conf = opts.port, opts.conf
    return function(request)
        local n = port:count(request, conf)
        if not whole_at_least(n, 0) then
            error("policy.tokens: port:count must answer a whole number >= 0, got " .. tostring(n), 2)
        end
        if n < 1 then
            return 1
        end
        return n
    end
end

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
local function trim(text, limit)
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
local function render_result(result)
    if type(result) == "string" then
        return result
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
local function default_failed(pair)
    return not pair.ok
end

--- One `tool_result` event as the pair a predicate is handed
--- (`policy.shapes.tool_pair`), with the call half looked up by id.
local function pair_of(beat, called, data)
    local call = called[data.call_id] or {}
    return {
        beat = beat,
        call_id = data.call_id,
        name = call.name,
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
local function reason_for(pair)
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
local function failure_note(events, limit, failed)
    local order = beats_of(events)
    local previous = order[#order]
    if previous == nil then
        return nil
    end

    local called = {}
    for _, ev in ipairs(previous.events) do
        if ev.kind == "tool_call" then
            local data = data_of(ev)
            if data.call_id ~= nil then
                called[data.call_id] = { name = data.name, input = data.args }
            end
        end
    end

    local reasons = {}
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
local function prepend_note(request, note)
    local out = {}
    for k, v in pairs(request) do
        out[k] = v
    end
    local messages = setmetatable({ { role = "user", content = note } }, ARRAY_TAG)
    for _, message in ipairs(request.messages or {}) do
        messages[#messages + 1] = message
    end
    out.messages = messages
    return out
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
function M.carry(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.carry: opts must be a table", 2)
    end
    only(opts, { max_bytes = true, failed = true }, "policy.carry")
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
    return function(session)
        -- The one thing the binder needs off the handle, checked where it is
        -- bound rather than at the first beat: a filter that raised on its
        -- first call would be reported as a filter failure, which is not what
        -- went wrong. The read itself is pcall'd because the real handle is
        -- Rust userdata whose indexing can raise — the same reason knl's own
        -- session gate duck-types through a pcall.
        local reachable = type(session) == "table" or type(session) == "userdata"
        local readable, events_fn = false, nil
        if reachable then
            readable, events_fn = pcall(function()
                return session.events
            end)
        end
        if not readable or not callable(events_fn) then
            error("policy.carry: bind takes a knl session (from knl.open / knl.resume)", 2)
        end
        return function(request)
            local note = failure_note(whole_log(session, "policy.carry"), limit, failed)
            if note == nil then
                return request
            end
            return prepend_note(request, note)
        end
    end
end

-- ============================================================
-- stagnation — is the run going in circles?
-- ============================================================

--- The default signature: what a beat CALLED, and nothing else.
---
--- Tool name and tool input, in a rendering that does not depend on table
--- order. Everything a beat carries that changes on its own — the call id,
--- the beat id, `epoch_ms`, `seq`, the token counts — is left out, because a
--- signature that included any of them would never repeat and the policy
--- would never fire.
---
--- A beat that called no tool has NO signature and answers nil. That is what
--- keeps the two verdicts apart: "repeated" is about doing the same thing
--- again, so a beat that did no thing cannot be part of a repetition, and the
--- run of empty beats is `no_progress`'s question instead. A caller whose
--- channel repeats in some other way (the same answer text, the same emitted
--- event) supplies its own `signature` and decides that for itself.
---
--- @param beat table  a `policy.shapes.beat_record`
--- @return string|nil  the signature, or nil when the beat has none
local function default_signature(beat)
    local parts = {}
    for _, ev in ipairs(beat.events) do
        if ev.kind == "tool_call" then
            local data = data_of(ev)
            parts[#parts + 1] = tostring(data.name) .. canonical(data.args)
        end
    end
    if #parts == 0 then
        return nil
    end
    return table.concat(parts, ";")
end

--- One beat's signature, held to the contract.
---
--- `signature` is the caller's code and its contract is `fn(beat) -> string |
--- nil`. A third kind of answer is a broken policy, not a third meaning, and
--- it is loud in prod as well as dev: a signature silently read as "no
--- signature" would turn the whole check off and look exactly like a run that
--- is not repeating.
local function signature_of(signature, beat)
    local value = signature(beat)
    if value ~= nil and type(value) ~= "string" then
        error("policy.stagnation: signature must return a string or nil, got " .. type(value), 0)
    end
    return value
end

--- Whether the last `n` beats all carry one signature.
local function is_repeated(order, n, signature)
    if #order < n then
        return false
    end
    local last = signature_of(signature, order[#order])
    if last == nil then
        return false
    end
    for i = #order - n + 1, #order - 1 do
        if signature_of(signature, order[i]) ~= last then
            return false
        end
    end
    return true
end

--- Whether a beat put anything into the record: a tool call, or a word of
--- text in the response it recorded.
---
--- "New content" is read as content AT ALL — a text block whose text is not
--- empty and not only whitespace. It is deliberately not "content that has
--- not been seen before": whether an answer repeats is what a signature is
--- for, and folding that question in here would make one verdict out of two
--- and leave a caller unable to tell them apart.
---
--- A beat that failed its call wrote no response and no tool call, so it made
--- no progress — which is true, and is why a run of failing beats reaches
--- `no_progress` rather than running until the budget stops it.
local function made_progress(beat)
    for _, ev in ipairs(beat.events) do
        if ev.kind == "tool_call" then
            return true
        end
        if ev.kind == "llm_response" then
            for _, block in ipairs(data_of(ev).content or {}) do
                if block.type == "text" and type(block.text) == "string" and block.text:match("%S") then
                    return true
                end
            end
        end
    end
    return false
end

--- Whether the last `m` beats all produced nothing.
local function is_idle(order, m)
    if #order < m then
        return false
    end
    for i = #order - m + 1, #order do
        if made_progress(order[i]) then
            return false
        end
    end
    return true
end

--- Build the predicate a caller's loop asks between beats: has this run
--- stopped getting anywhere?
---
---     local stalled = policy.stagnation({ same = 3, no_progress = 2 })
---     ...
---     local why = stalled(session)
---     if why ~= nil then break end
---
--- Two counters, two verdicts, and they are independent readings of the same
--- log rather than one score:
---
---   "repeated"     the last `same` beats carry one signature — the model is
---                  making the same call over again
---   "no_progress"  the last `no_progress` beats wrote neither a tool call
---                  nor a word of content — the run is producing nothing
---
--- `repeated` is asked first. Under the default signature the two cannot both
--- hold (a beat with no tool call has no signature and cannot be part of a
--- repetition), but a custom signature can make them overlap, so the order is
--- fixed here and stated rather than left to whichever check happens to run.
---
--- The predicate holds no counters. It derives the beats from
--- `session:events()` on every call, which is what lets a resumed session be
--- judged on its whole history and two drivers reach the same verdict.
---
--- RECOVERING FROM A TRIP IS THE CALLER'S LOOP. This answers that the run is
--- going in circles and stops there; whether to break, hand the work to a
--- person, or say something to the model and go round once more is not a
--- verdict, and there is no factory here for it. A loop that would rather
--- nudge appends its own message and beats again, under a bound — an
--- unbounded nudge is the same circle with a sentence in it:
---
---     local why = stalled(session)
---     if why == "repeated" and nudges < MAX_NUDGES then
---         nudges = nudges + 1
---         session:append({ kind = "msg_user", data = { content = "that is not working" } })
---     elseif why ~= nil then break end
---
--- The append is the caller's own, on the caller's session, and this module
--- writes none of it: nothing here appends (the header), and a policy that
--- nudged on its own behalf would be a loop pretending to be a value.
---
--- @param opts table  { same?, no_progress?, signature? }
--- @return function predicate  fn(session) -> nil | "repeated" | "no_progress"
function M.stagnation(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.stagnation: opts must be a table", 2)
    end
    only(opts, { same = true, no_progress = true, signature = true }, "policy.stagnation")
    if opts.same ~= nil and not whole_at_least(opts.same, 2) then
        -- Two is the floor because one beat cannot repeat anything.
        error("policy.stagnation: same must be a whole number >= 2, got " .. tostring(opts.same), 2)
    end
    if opts.no_progress ~= nil and not whole_at_least(opts.no_progress, 1) then
        error("policy.stagnation: no_progress must be a whole number >= 1, got " .. tostring(opts.no_progress), 2)
    end
    if opts.signature ~= nil and type(opts.signature) ~= "function" then
        error("policy.stagnation: signature must be a function (fn(beat) -> string | nil)", 2)
    end
    shape.assert_dev(opts, STAGNATION_OPTS, "policy.stagnation opts")

    local same = opts.same or DEFAULT_SAME
    local no_progress = opts.no_progress or DEFAULT_NO_PROGRESS
    local signature = opts.signature or default_signature

    return function(session)
        local order = beats_of(whole_log(session, "policy.stagnation"))
        if is_repeated(order, same, signature) then
            return "repeated"
        end
        if is_idle(order, no_progress) then
            return "no_progress"
        end
        return nil
    end
end

-- ============================================================
-- retry — is this failure worth asking again?
-- ============================================================

--- Build the predicate a caller's loop asks about an `Outcome`.
---
---     local again = policy.retry({ kinds = { "busy" }, max = 3 })
---     ...
---     local ask, delay = again(outcome, attempt)
---
--- What it decides on is the KIND of failure, read out of the Outcome's
--- detail — `detail.kind`, and `detail.retryable`, the judgement that came
--- with it. It does not read an HTTP status, a status class, or any number a
--- provider attached: a 503 is not a class of failure, it is one provider's
--- word for several, and a policy that retried on it would be retrying on the
--- provider's vocabulary instead of the kernel's.
---
--- TWO VOCABULARIES ANSWER IN THAT ONE FIELD and `kinds` takes either. A
--- `state` failure carries one of `knl.shapes.error_kinds` (the kernel's own:
--- `busy`, `storage`, …) and a `call` failure one of
--- `knl.shapes.call_error_kinds` (the adapter's classification of a call that
--- did not come off: `rate_limited`, `overloaded`, …). They stay separate
--- vocabularies — a contended store is not a busy provider — and a caller
--- names from whichever it means.
---
---   * no `kinds` — retry exactly when `detail.retryable` is true, which is
---     the judgement that came with the failure and the right default;
---   * `kinds` given — retry when `detail.kind` is one of them, and that
---     naming is the whole answer. It is how a caller says "I will also ask
---     again about a storage failure", or "of the retryable ones I want only
---     the rate limit" — judgements neither the kernel nor the adapter makes
---     for anyone.
---
--- `max` is attempts IN TOTAL, the first one included, so `attempt` — the
--- caller's own count of attempts already made, 1 on the first — is retried
--- while it is below `max`. It is an argument rather than something kept
--- here: the count belongs to the loop that is doing the attempting, and a
--- counter in this module would be shared by every loop that used it.
---
--- `retry_after`, when the detail carries one as a number of seconds, rides
--- back as the second return. `knl.shapes.error` is an open shape, which is
--- what lets an adapter attach it.
---
--- Only an `error` Outcome is ever retried. `ok` has nothing to ask again;
--- `refused` is the model declining, and asking the same question again is
--- not an answer to that; `stopped` is the budget, and a retry past it would
--- be a loop spending an allowance the owner did not give.
---
--- @param opts table  { kinds? = { <error kind | call error kind>... }, max? = <whole number >= 1> }
--- @return function predicate  fn(outcome, attempt) -> boolean, number?
function M.retry(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.retry: opts must be a table", 2)
    end
    only(opts, { kinds = true, max = true }, "policy.retry")
    if opts.max ~= nil and not whole_at_least(opts.max, 1) then
        error("policy.retry: max must be a whole number >= 1, got " .. tostring(opts.max), 2)
    end
    local named
    if opts.kinds ~= nil then
        if type(opts.kinds) ~= "table" then
            error("policy.retry: kinds must be an array of knl error kinds", 2)
        end
        local known = {}
        for _, kind in ipairs(RETRY_KINDS) do
            known[kind] = true
        end
        named = {}
        for i, kind in ipairs(opts.kinds) do
            if not known[kind] then
                error("policy.retry: kinds[" .. i .. "] is not a knl failure kind: " .. tostring(kind), 2)
            end
            named[kind] = true
        end
    end
    shape.assert_dev(opts, RETRY_OPTS, "policy.retry opts")

    local max = opts.max or DEFAULT_MAX_ATTEMPTS
    return function(outcome, attempt)
        -- The count is the loop's and it is required. A missing one read as
        -- zero would make every failure retryable forever, which is the one
        -- mistake a retry policy must not make quietly.
        if not whole_at_least(attempt, 1) then
            error("policy.retry: attempt must be a whole number >= 1, got " .. tostring(attempt), 2)
        end
        if type(outcome) ~= "table" or outcome.status ~= "error" then
            return false
        end
        if attempt >= max then
            return false
        end
        local detail = outcome.detail
        if type(detail) ~= "table" then
            -- Only a failure the kernel classified carries a reading; the
            -- stages whose detail is a sentence (`conf` / `filter` / `call`)
            -- name no kind, so there is nothing here to decide on.
            return false
        end
        local worth
        if named ~= nil then
            worth = detail.kind ~= nil and named[detail.kind] == true
        else
            worth = detail.retryable == true
        end
        if not worth then
            return false
        end
        if type(detail.retry_after) == "number" then
            return true, detail.retry_after
        end
        return true
    end
end

-- ============================================================
-- escalate — the device for the next beat
-- ============================================================

--- The default judgement: escalate on a refusal, or on a failure that asking
--- again would not fix.
---
--- A retryable failure is not one to escalate on — a busy store is not a
--- model that could not manage the task, and swapping the llm would spend a
--- stronger one on a problem it has no bearing on. That is the line between
--- this policy and `retry`: `retry` answers "the same device again", this one
--- answers "a different device".
local function default_when(outcome)
    if type(outcome) ~= "table" then
        return false
    end
    if outcome.status == "refused" then
        return true
    end
    if outcome.status ~= "error" then
        return false
    end
    local detail = outcome.detail
    return not (type(detail) == "table" and detail.retryable == true)
end

--- Build `next(outcome, device) -> device`: the device the following beat
--- should use.
---
---     local escalate = policy.escalate({ strong = opus })
---     ...
---     device = escalate(outcome, device)
---
--- ESCALATE HERE MEANS CHANGING THE TOOL, NOT ASKING A SUPERVISOR. Nothing is
--- delegated, nobody is notified, and no second agent is involved: the answer
--- is a device, derived with `d:with{ llm = strong }`, and the next beat runs
--- in the same session against the same log. The word is worth pinning down
--- because it means the other thing almost everywhere else.
---
--- When `when` does not hold, the device that came in is handed straight back
--- — the same value, not a copy — so a loop can assign the result
--- unconditionally and a beat that did not need escalating pays nothing.
---
--- A `when` that raises is not caught. It is the caller's code and this is the
--- caller's loop calling it, so the raise lands where it was made rather than
--- being read as a judgement one way or the other — a gate that fell open, or
--- shut, on its own bug would be the wrong answer either way.
---
--- @param opts table  { strong = <llm>, when? = fn(outcome) -> boolean }
--- @return function next  fn(outcome, device) -> device
function M.escalate(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.escalate: opts must be a table", 2)
    end
    only(opts, { strong = true, when = true }, "policy.escalate")
    if not callable(opts.strong) then
        error("policy.escalate: strong must be an llm (a function, or a callable)", 2)
    end
    if opts.when ~= nil and type(opts.when) ~= "function" then
        error("policy.escalate: when must be a function (fn(outcome) -> boolean)", 2)
    end
    shape.assert_dev(opts, ESCALATE_OPTS, "policy.escalate opts")

    local strong = opts.strong
    local when = opts.when or default_when
    return function(outcome, device)
        if when(outcome) then
            return device:with({ llm = strong })
        end
        return device
    end
end

-- ============================================================
-- The API registry
-- ============================================================
--
-- One entry per public export, naming the shape of what goes in and what
-- comes out — the same form `knl.shapes.api` uses, so the two read alike and
-- one spec can be written against the other's shape.
--
-- `args` is an ordered list of `{ shape, desc }`, one per positional
-- argument, and it is what the dev-mode gate below RUNS. `members` names the
-- functions a factory hands BACK: they are values this module produces rather
-- than exports it owns, so — exactly like `device:with` in knl — their
-- entries are declared and walked by the spec but nothing wraps them.

--- One declared argument: the shape it is held to, and the word for it.
local function arg_of(schema, desc)
    return { shape = schema, desc = desc }
end

local EVENTS_ARG = arg_of(T.array_of(kernel.shapes.event_base), "events")
local SESSION_ARG = arg_of(SESSION_HANDLE, "session")
local OUTCOME_ARG = arg_of(kernel.shapes.outcome, "outcome")

-- The `*_ARG` shapes below are the OPEN twins of the published contracts (see
-- `opts_contract`). The registry holds a call to the SHAPE of the options it
-- declared; whether a key is declared at all is `only`'s judgement, made in
-- both modes, and the gate must not answer it first with a message of its own.

M.shapes.api = {
    window = {
        args = { arg_of(WINDOW_ARG, "opts") },
        returns = 'fold — fn(events, device) -> request; and, with `fit`, fits — fn(session, device) -> nil | "context"',
        members = {
            fold = {
                args = { EVENTS_ARG, arg_of(T.table, "device (read for system / tools)") },
                returns = kernel.shapes.request,
            },
            fits = {
                args = { SESSION_ARG, arg_of(T.table, "device (read for system / tools)") },
                returns = 'nil | policy.shapes.stop_reason ("context")',
            },
        },
    },
    tokens = {
        args = { arg_of(TOKENS_ARG, "opts") },
        returns = "cost — fn(request) -> integer >= 1 (the request's tokens, by the port's count)",
        members = {
            cost = {
                args = { arg_of(kernel.shapes.request, "request") },
                returns = "integer >= 1",
            },
        },
    },
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
    stagnation = {
        args = { arg_of(STAGNATION_ARG, "opts") },
        returns = "predicate — fn(session) -> nil | policy.shapes.stop_reason",
        members = {
            predicate = {
                args = { SESSION_ARG },
                returns = "nil | policy.shapes.stop_reason",
            },
            signature = {
                args = { arg_of(BEAT_RECORD, "beat") },
                returns = "string | nil (nil = this beat has no signature and cannot repeat)",
            },
        },
    },
    retry = {
        args = { arg_of(RETRY_ARG, "opts") },
        returns = "predicate — fn(outcome, attempt) -> boolean, delay_seconds?",
        members = {
            predicate = {
                args = { OUTCOME_ARG, arg_of(T.number, "attempt (attempts already made; 1 on the first)") },
                returns = "boolean, number? — ask again, and the delay the detail named",
            },
        },
    },
    escalate = {
        args = { arg_of(ESCALATE_ARG, "opts") },
        returns = "next — fn(outcome, device) -> device",
        members = {
            next = {
                args = { OUTCOME_ARG, arg_of(T.table, "device") },
                returns = "device — the same one, or d:with{ llm = strong }",
            },
            when = {
                args = { OUTCOME_ARG },
                returns = "boolean",
            },
        },
    },
    shapes = {
        args = {},
        returns = "this registry: every shape above, plus `api`",
    },
}

-- ============================================================
-- The registry, executed
-- ============================================================
--
-- In dev mode each declared export is replaced, once, here at load, by a
-- wrapper that holds the call to its entry. Prod installs nothing and a call
-- pays nothing — which is why every check a factory must not be built without
-- is written beside it, loud in both modes.
--
-- What the gate judges is the shape of the arguments that were PASSED. An
-- argument nobody supplied is left to the function: `policy.window()` must go
-- on raising its own "tail must be a whole number" rather than a shape
-- violation about an opts table that was never there.
--
-- And what it judges about an opts table is the shape of the keys that are
-- DECLARED, never whether an undeclared one is present — that judgement is
-- `only`'s, in both modes (`opts_contract`). A gate that answered it would
-- make the module say two different things about one typo depending on an
-- environment variable, which is a divergence between test harnesses waiting
-- to happen (and it was one: the spec runner sets LSHAPE_CHECK=1, and three
-- refusal cases that passed under a bare runner failed under it).

local function arg_checked(name, fn, declared)
    return function(...)
        for i = 1, #declared do
            local value = select(i, ...)
            if value ~= nil then
                shape.assert_dev(value, declared[i].shape, name .. " arg " .. i .. " (" .. declared[i].desc .. ")")
            end
        end
        return fn(...)
    end
end

if shape.is_dev_mode() then
    for name, entry in pairs(M.shapes.api) do
        local export = M[name]
        if type(export) == "function" then
            M[name] = arg_checked("policy." .. name, export, entry.args)
        end
    end
end

return M
