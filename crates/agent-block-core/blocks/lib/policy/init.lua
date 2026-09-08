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
--- The nine, and where each one plugs in
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
---     policy.result_cap  -> `tools`       one tool result may not outgrow a
---                                         share of the model's window
---     policy.repeat_cap  -> `tools`       the same call, again, with nothing
---                                         changed in between, is refused
---                                         past a count
---     policy.verdict     -> a check       the loop runs it after every beat:
---                                         is the thing outside the
---                                         conversation true yet?
---
---   `window`, `carry`, `tokens`, `result_cap` and `repeat_cap` are device
---   fields (`knl.device{ fold = ..., filters = { ... }, cost = ..., tools = ... }`);
---   `stagnation`, `retry`, `escalate` and `verdict` are the loop's own and
---   the kernel never sees them. That
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
---     repeat_cap  reads the log — `session:events()`, through the binder,
---                 on every call it is asked to answer
---     verdict     reads the log — `session:events()`, per call, and only
---                 when `timeout` is a table: what the checks took are gaps
---                 between the kernel's stamps, read by the `measure` the
---                 caller chose, not a number the factory kept. A number
---                 for `timeout` reads nothing
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

--- What `policy.verdict` is configured with. `run` is the caller's — only it
--- knows what "it works" is for this run — and everything else is about what
--- the loop does with the answer.
local VERDICT_OPTS, VERDICT_ARG = opts_contract({
    run = FUNCTION
        :describe(
            "fn(timeout?) -> { ok, ran?, stdout?, stderr?, exit_code? }; nil = no verdict, the run is the model's word"
        )
        :is_optional(),
    changed = FUNCTION:describe("fn(session) -> boolean; a green with nothing changed is not a pass"):is_optional(),
    kind = T.string:describe('the event kind the answer is recorded under; default "verify"'):is_optional(),
    timeout = T.any_of({ T.number, T.table })
        :describe(
            "the seconds handed to run: a number is handed whole every time; "
                .. '{ first, factor?, floor?, measure? } reads them off the log, `measure` = "longest" | "last" | "last_ok" | "first" | fn(events, kind) -> seconds|nil'
        )
        :is_optional(),
})

--- What `policy.result_cap` is configured with. The share is of the window
--- the Port declares, never a count of bytes: what is large depends on the
--- model, and the model is what the Port knows.
local RESULT_CAP_OPTS, RESULT_CAP_ARG = opts_contract({
    port = T.table:describe("an LLM Port: profile(conf) for the window, count(request, conf) for the size"),
    conf = T.table:describe("the conf the port is opened with"):is_optional(),
    share = T.number
        :describe("the most of the window one tool result may take, 0 < share <= 1; default 0.25")
        :is_optional(),
})

--- What `policy.repeat_cap` is configured with. `resets` names the tools whose
--- success makes an old call new again — an edit changes what a read would
--- answer.
local REPEAT_CAP_OPTS, REPEAT_CAP_ARG = opts_contract({
    max = T.number
        :describe("how many times one call (tool + arguments) may be made with nothing reset in between; default 2")
        :is_optional(),
    resets = T.table
        :describe("tool names whose successful result starts the count over for every call; default none")
        :is_optional(),
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
    local from = first_at[order[#order - tail + 1]]
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

    -- The newest beat is never dropped, `keep_seed` or not. It holds the
    -- tool_result the model is waiting for, and a request without it answers
    -- nothing the model asked: it would read the file again, which is the
    -- loop this fold exists to prevent. When even that one beat does not fit,
    -- the honest answer is that nothing fits — said out loud, by the
    -- predicate or by the raise — and not a request that quietly forgot.
    local floor = 1

    -- The profile is read per fold, not captured: a Port opened with one conf
    -- answers one profile, and reading it each time costs nothing while
    -- letting a Port whose window is learned late (a served model queried
    -- for it) answer the truth.
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

        if most < floor then
            -- No beat yet: the seed alone is the whole conversation, and
            -- there is nothing to choose between.
            local request, tokens = fold_at(0)
            if tokens <= limit then
                return request
            end
            return nil, tokens, limit
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
                "policy.window: the newest beat does not fit%s: %d tokens > %d. One tool result is larger "
                    .. "than the window can hold — cap what a tool may answer (policy.result_cap). "
                    .. "Ask the second value this factory answers before the beat to stop instead of failing.",
                keep_seed and " even with the seed alone beside it" or " on its own",
                tokens,
                limit
            ),
            2
        )
    end

    --- The same question, asked before the beat rather than inside it.
    local fits = function(session, device)
        needs_session(session, "policy.window fits")
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
-- repeat_cap — the same call again, with nothing changed, is refused
-- ============================================================

--- The JSON a call's arguments render to: what "the same call" compares.
--- Key order is not promised by every encoder, so the keys are sorted here
--- before encoding — a table read back from the log and a table handed to a
--- handler must key alike.
local function call_key(name, args)
    if type(args) ~= "table" then
        return tostring(name) .. "\0" .. tostring(args)
    end
    local keys = {}
    for k in pairs(args) do
        keys[#keys + 1] = tostring(k)
    end
    table.sort(keys)
    local parts = {}
    for _, k in ipairs(keys) do
        local v = args[k]
        parts[#parts + 1] = k .. "=" .. (type(v) == "table" and std.json.encode(v) or tostring(v))
    end
    return tostring(name) .. "\0" .. table.concat(parts, "\1")
end

--- Whether a `tool_result` says its tool did what it was asked. The kernel's
--- `ok` is raise detection; a tool that refuses in its return value (`std.fs`
--- does) says so with `result.ok == false`. Both are read.
local function result_succeeded(data)
    if data.ok == false then
        return false
    end
    local result = data.result
    if type(result) == "table" and result.ok == false then
        return false
    end
    return true
end

--- Build a wrapper over a device's `tools` map that refuses a call already
--- made `max` times since the last reset.
---
---     local tools = policy.repeat_cap({ max = 2, resets = { "fs_edit" } })(session)(raw_tools)
---
--- The model, its context window full, drops old reads out of the
--- conversation and asks for them again — the same file, the same range —
--- and asks again when those drop too, without ever editing. Measured on a
--- vLLM-served model against a file larger than its window: the loop ran
--- out of budget having read one range eleven times. Nothing in the
--- kernel is wrong: every call was answered. The tool layer is where a
--- repeated question can be told it is repeated, and the answer that helps
--- is a refusal that says so, not the content again.
---
--- The count is read off the log, not kept. Like `carry`, the factory
--- answers a BINDER: `policy.repeat_cap{...}(session)` is the value that
--- wraps `tools`, and every call counts its own `tool_call` records since
--- the last successful result of a tool in `resets` — an edit that landed
--- makes an old read new, since the file it would read has changed. The
--- kernel records the `tool_call` before it runs the handler, so the call
--- being answered is in its own count: `max = 2` lets a call through twice
--- and refuses the third. A process that restarted resumes the same count;
--- two drivers on one log refuse alike.
---
--- The refusal is a return value, `{ ok = false, reason = "repeated", ... }`,
--- so the kernel records the pair and the model reads why. `result_cap` is
--- the sibling for the other way a tool result breaks a run; the two
--- compose in either order.
---
--- @param opts table|nil  { max?, resets? }
--- @return function binder  fn(session) -> fn(tools) -> tools
function M.repeat_cap(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.repeat_cap: opts must be a table", 2)
    end
    only(opts, { max = true, resets = true }, "policy.repeat_cap")
    if opts.max ~= nil and (type(opts.max) ~= "number" or opts.max < 1 or opts.max % 1 ~= 0) then
        error("policy.repeat_cap: max must be a whole number >= 1, got " .. tostring(opts.max), 2)
    end
    if opts.resets ~= nil then
        if type(opts.resets) ~= "table" then
            error("policy.repeat_cap: resets must be an array of tool names", 2)
        end
        for i, name in ipairs(opts.resets) do
            if type(name) ~= "string" or name == "" then
                error("policy.repeat_cap: resets[" .. i .. "] must be a non-empty tool name", 2)
            end
        end
    end
    shape.assert_dev(opts, REPEAT_CAP_OPTS, "policy.repeat_cap opts")

    local max = opts.max or DEFAULT_REPEAT_MAX
    local resets = {}
    for _, name in ipairs(opts.resets or {}) do
        resets[name] = true
    end

    return function(session)
        needs_session(session, "policy.repeat_cap")

        --- How many times `key` has been called since the last reset, off
        --- the log: every `tool_call` with that key after the newest
        --- successful `tool_result` of a reset tool.
        local function count(key)
            local events = whole_log(session, "policy.repeat_cap")
            local reset_tools_by_call = {}
            local since = 0
            for i, ev in ipairs(events) do
                local data = type(ev.data) == "table" and ev.data or {}
                if ev.kind == "tool_call" then
                    if resets[data.name] and data.call_id ~= nil then
                        reset_tools_by_call[data.call_id] = true
                    end
                elseif ev.kind == "tool_result" and data.call_id ~= nil and reset_tools_by_call[data.call_id] then
                    if result_succeeded(data) then
                        since = i
                    end
                end
            end
            local n = 0
            for i = since + 1, #events do
                local ev = events[i]
                if ev.kind == "tool_call" then
                    local data = type(ev.data) == "table" and ev.data or {}
                    if call_key(data.name, data.args) == key then
                        n = n + 1
                    end
                end
            end
            return n
        end

        return function(tools)
            if type(tools) ~= "table" then
                error("policy.repeat_cap: tools must be the device's map of name -> entry", 2)
            end
            local out = {}
            for name, entry in pairs(tools) do
                if type(entry) ~= "table" or type(entry.handler) ~= "function" then
                    error("policy.repeat_cap: tool '" .. tostring(name) .. "' has no handler", 2)
                end
                local capped = {}
                for k, v in pairs(entry) do
                    capped[k] = v
                end
                local handler = entry.handler
                capped.handler = function(args)
                    local seen = count(call_key(name, args))
                    if seen <= max then
                        return handler(args)
                    end
                    return {
                        ok = false,
                        reason = "repeated",
                        times = seen,
                        max = max,
                        error = string.format(
                            "'%s' was already called with exactly these arguments %d times and nothing has "
                                .. "changed since. The answer would be the same. Act on what you already saw "
                                .. "instead of asking again.",
                            tostring(name),
                            seen - 1
                        ),
                    }
                end
                out[name] = capped
            end
            return out
        end
    end
end

-- ============================================================
-- verdict — what ends a run, when the model's word is not enough
-- ============================================================

--- The checks in the log, each with what it took.
---
--- The kernel stamps `epoch_ms` on every record, so a check's time is the
--- gap between its record and the one before it; a record without a stamp
--- on either side measures nothing rather than something wrong, and is left
--- out. Each entry carries the check's answer — `ok`, and `ran` (a record
--- from before the field existed counts as answered) — and, for a check
--- that was cut off, `timeout_s`, the seconds it was given: not how long
--- it takes, but a bound it exceeded, which a measure may or may not use.
---
--- @param events table  a session's events, in seq order
--- @param kind string  the kind checks are recorded under
--- @return table  { { seconds, ok, ran, timeout_s? }, ... } in log order
local function checks_of(events, kind)
    local found = {}
    for i = 2, #events do
        local ev = events[i]
        if ev.kind == kind and type(ev.data) == "table" then
            local at, before = ev.epoch_ms, events[i - 1].epoch_ms
            if type(at) == "number" and type(before) == "number" and at >= before then
                found[#found + 1] = {
                    seconds = (at - before) / 1000,
                    ok = ev.data.ok == true,
                    ran = ev.data.ran ~= false,
                    timeout_s = type(ev.data.timeout_s) == "number" and ev.data.timeout_s or nil,
                }
            end
        end
    end
    return found
end

--- The ways a check's seconds are read off the log, by name. Each answers
--- `fn(events, kind) -> seconds | nil` — nil when nothing it counts is
--- there yet — which is also what a caller's own function must answer.
---
--- Which one is right is the caller's to say, because it depends on the
--- repository. Whether a failing check is quicker than a passing one, and
--- by how much, is a fact about what `run` runs — a compile error that
--- stops the build early, or a link that takes as long either way — and
--- nothing in the log says which repository this is. So the reading is
--- chosen, not inferred, and the factory does not pick one on evidence it
--- does not have.
---
---   last     the latest check that answered, pass or fail. What the
---            repository costs to check now, if a failing check costs
---            what a passing one does
---   last_ok  the latest check that answered and passed. The whole check,
---            if a failing one stops early and would under-read it
---   longest  the longest reading there is: the latest answered check or
---            any earlier one, whichever took longer, and a check that was
---            cut off counts as the seconds it was given — it took at
---            least that. Never shrinks, so a quick failure cannot cut the
---            slow pass that follows it, and a cut-off widens the window
---            rather than being forgotten
---   first    the first check that answered, and that one for the rest of
---            the run. A measurement taken once — a loop that checks
---            before its first beat reads its baseline here
local MEASURES = {}

function MEASURES.last(events, kind)
    local taken
    for _, c in ipairs(checks_of(events, kind)) do
        if c.ran then
            taken = c.seconds
        end
    end
    return taken
end

function MEASURES.last_ok(events, kind)
    local taken
    for _, c in ipairs(checks_of(events, kind)) do
        if c.ran and c.ok then
            taken = c.seconds
        end
    end
    return taken
end

function MEASURES.longest(events, kind)
    local taken
    for _, c in ipairs(checks_of(events, kind)) do
        local reading = c.ran and c.seconds or c.timeout_s
        if reading ~= nil and (taken == nil or reading > taken) then
            taken = reading
        end
    end
    return taken
end

function MEASURES.first(events, kind)
    for _, c in ipairs(checks_of(events, kind)) do
        if c.ran then
            return c.seconds
        end
    end
    return nil
end

--- The measure `verdict{ timeout }` names, as a function; a function is its
--- own. Refuses a name this module does not know, loudly, at construction.
local function measure_of(measure)
    if measure == nil then
        return MEASURES[DEFAULT_TIMEOUT_MEASURE]
    end
    if type(measure) == "function" then
        return measure
    end
    if type(measure) == "string" and MEASURES[measure] then
        return MEASURES[measure]
    end
    local names = {}
    for name in pairs(MEASURES) do
        names[#names + 1] = '"' .. name .. '"'
    end
    table.sort(names)
    error(
        "policy.verdict: timeout.measure must be one of "
            .. table.concat(names, ", ")
            .. " or fn(events, kind) -> seconds|nil, got "
            .. tostring(measure),
        3
    )
end

--- Build the check a loop runs after every beat.
---
---     local verdict = policy.verdict({ run = function() return build() end })
---     ...
---     local out = knl.beat(s, device)
---     local v = verdict(s, out)
---     if v.ok then break end        -- the run is done because the check says so
---
--- Three things can end a loop, and they belong in three places. The BUDGET
--- is the kernel's: a quota an owner granted, refused before the call, and
--- the one stop the caller cannot forget to check. The MODEL's own ending —
--- it asked for no tools, it says it is finished — is a fact about the last
--- beat and the loop reads it off `out`. A VERDICT is neither: it is
--- something outside the conversation being true, and only the caller knows
--- what to run to find out.
---
--- What this adds is that the verdict is not a tool. A tool the model can
--- decline to call cannot carry "it compiles": a run that ended because the
--- model said so has no evidence, and a loop written to trust that will
--- report success it never checked. Here `run` is called after every beat,
--- whatever the model asked for and whatever it answered, and its answer is
--- appended to the log under `kind` (default `"verify"`) stamped with the
--- beat it judges — so the record says what was checked and when, and a
--- `stagnation` signature can read it.
---
--- `changed` is the second half of the same honesty. On a task whose
--- deliverable is the test that proves it, the check passes before any work
--- is done; a loop that stopped there would report a pass for an empty
--- diff. When `changed` is given, a green counts only if it also answers
--- true, and the verdict says why it was withheld.
---
--- `run` is optional. Without it this answers `{ ok = false, checked =
--- false }` on every beat: there is no verdict, so nothing here ever ends
--- the run, and the loop is left with the budget and the model's own ending.
--- That is the honest default — the alternative, a green with nothing
--- checked, is the exact claim this module exists to refuse.
---
--- The seconds a check may take, when the loop wants that decided here
---
---   `timeout = <secs>`
---   `timeout = { first = <secs>, factor? = <n>, floor? = <secs>, measure? = <how> }`
---
--- Given one, `run` is called with a number of seconds and is expected to
--- apply it — `sh.exec`'s own `timeout`, or whatever the check is made of;
--- the option carries that name because it is that number. Lua has no
--- preemption, so nothing here can cut a check short: `run` is a call, and a
--- call that does not return does not return. What is decided here is the
--- number; applying it is the seam's.
---
--- It is not the budget. The budget is the kernel's quota — an allocation
--- the owner granted, spent and never refilled — and this is a limit, given
--- whole to every check; the kernel's header says the two have different
--- arithmetic and do not share a counter, and they do not share a word here
--- either. `budget` in these opts is refused as the state key it is.
---
--- A number is the whole of it: handed to every check as it is, nothing
--- read. That is the form for a caller that has measured the check itself
--- — run it once, whole, before the loop, and hand in a multiple of what it
--- took — and it is the form that decides the number where it can be
--- known, which is outside the loop; one beat does not know which files
--- changed or what the check costs in this repository.
---
--- A table reads the number off the log. Every record the kernel writes
--- carries `epoch_ms`, so what a check took is the gap between its own
--- record and the record before it — the beat it judged, in a loop that
--- checks straight after the beat; whatever the loop did in between is
--- counted with it. `measure` says WHICH check's time is read (the names
--- above `MEASURES`: `"longest"`, `"last"`, `"last_ok"`, `"first"`, or the
--- caller's own `fn(events, kind) -> seconds | nil`), and the next check
--- may take `taken * factor`, held to `floor` below and `first` above.
--- Until the measure has something to read `first` stands — it covers a
--- check that has to build from nothing. The measure is the caller's
--- choice and not inferred here, because which reading is right is a fact
--- about the repository the log does not carry: whether a failing check is
--- quicker than a passing one is a tendency, not a rule, and a factory
--- that assumed it would be wrong wherever it does not hold. Defaults:
--- `factor` 3, `floor` 60, `measure` `"longest"`, the one reading that
--- assumes nothing about direction.
---
--- Read rather than kept for the reason the header gives: a factory closure
--- holds only what it was configured with, so a resumed session does not
--- start from `first` again and two loops on one session hand `run` the
--- same number. That makes a table `timeout` a log-reading policy — it
--- reads `session:events()` on every call and is refused a log that does
--- not fit in one read (`whole_log`), exactly as `stagnation` is. Without
--- `timeout`, or with a number, nothing is read. A record with no stamp — a
--- stand-in session's, or one appended by hand — measures nothing rather
--- than something wrong.
---
--- A check that did not answer
---
--- `run` may say so with `ran = false` — a timeout, a spawn that failed,
--- anything where the check was attempted and no answer came back. It reaches
--- the caller as `ran = false` on the verdict and in the log, and it is not
--- the same event as a check that ran and said no: one is the thing under
--- test being wrong, the other is not knowing. Absent the field a result
--- counts as answered, which is what every result meant before it existed.
---
--- @param opts table|nil  { run?, changed?, kind?, timeout? }
--- @return function verdict  fn(session, out) -> { ok, checked, ran, changed?, result?, reason? }
function M.verdict(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.verdict: opts must be a table", 2)
    end
    only(opts, { run = true, changed = true, kind = true, timeout = true }, "policy.verdict")
    if opts.run ~= nil and type(opts.run) ~= "function" then
        error("policy.verdict: run must be a function (fn(timeout?) -> { ok, ... })", 2)
    end
    if opts.timeout ~= nil then
        if type(opts.timeout) == "number" then
            if opts.timeout <= 0 then
                error("policy.verdict: timeout must be a positive number of seconds", 2)
            end
        elseif type(opts.timeout) == "table" then
            only(opts.timeout, { first = true, factor = true, floor = true, measure = true }, "policy.verdict timeout")
            if type(opts.timeout.first) ~= "number" or opts.timeout.first <= 0 then
                error("policy.verdict: timeout.first must be a positive number of seconds", 2)
            end
            for _, k in ipairs({ "factor", "floor" }) do
                local v = opts.timeout[k]
                if v ~= nil and (type(v) ~= "number" or v <= 0) then
                    error("policy.verdict: timeout." .. k .. " must be a positive number", 2)
                end
            end
        else
            error(
                "policy.verdict: timeout must be a number of seconds or a table { first, factor?, floor?, measure? }",
                2
            )
        end
    end
    if opts.changed ~= nil and type(opts.changed) ~= "function" then
        error("policy.verdict: changed must be a function (fn(session) -> boolean)", 2)
    end
    if opts.kind ~= nil and (type(opts.kind) ~= "string" or opts.kind == "") then
        error("policy.verdict: kind must be a non-empty string, got " .. tostring(opts.kind), 2)
    end
    shape.assert_dev(opts, VERDICT_OPTS, "policy.verdict opts")

    local run, changed = opts.run, opts.changed
    local kind = opts.kind or DEFAULT_VERDICT_KIND
    -- Frozen at construction, like every other factory's opts. Nothing about
    -- a particular run lives here; what the checks took is in the log. A
    -- number is `first` with no measure: handed whole, nothing read.
    local first, factor, floor, measure
    if type(opts.timeout) == "number" then
        first = opts.timeout
    elseif opts.timeout then
        first = opts.timeout.first
        factor = opts.timeout.factor or DEFAULT_TIMEOUT_FACTOR
        floor = opts.timeout.floor or DEFAULT_TIMEOUT_FLOOR
        measure = measure_of(opts.timeout.measure)
    end

    return function(session, out)
        if run == nil then
            return { ok = false, checked = false }
        end
        needs_session(session, "policy.verdict")

        local timeout = first
        if measure then
            local taken = measure(whole_log(session, "policy.verdict"), kind)
            if taken ~= nil then
                if type(taken) ~= "number" or taken < 0 then
                    error("policy.verdict: timeout.measure must answer seconds >= 0 or nil, got " .. tostring(taken), 2)
                end
                timeout = taken * factor
                if timeout < floor then
                    timeout = floor
                end
                if timeout > first then
                    timeout = first
                end
            end
        end

        local result = run(timeout)
        if type(result) ~= "table" or type(result.ok) ~= "boolean" then
            error("policy.verdict: run must answer { ok = <boolean>, ... }, got " .. tostring(result), 2)
        end
        local ran = result.ran ~= false

        -- Recorded before it is judged, and recorded either way: the log is
        -- what says the check happened, and a check whose answer decided
        -- nothing is still a fact about the run. What the check took is not
        -- written here: the kernel's `epoch_ms` on this record and the one
        -- before it already say it, and a second copy could disagree.
        session:append({
            kind = kind,
            beat = type(out) == "table" and out.beat or nil,
            data = {
                ok = result.ok,
                ran = ran,
                timeout_s = timeout,
                stdout = tostring(result.stdout or ""),
                stderr = tostring(result.stderr or ""),
                exit_code = result.exit_code,
            },
        })

        if not result.ok then
            return { ok = false, checked = true, ran = ran, result = result }
        end
        if changed == nil then
            return { ok = true, checked = true, ran = ran, result = result }
        end
        local moved = changed(session) == true
        if moved then
            return { ok = true, checked = true, ran = ran, changed = true, result = result }
        end
        return {
            ok = false,
            checked = true,
            ran = ran,
            changed = false,
            result = result,
            reason = "unchanged",
        }
    end
end

-- ============================================================
-- result_cap — one tool result may not outgrow the window
-- ============================================================

--- Build a wrapper over a device's `tools` map that refuses a result larger
--- than `share` of the model's window.
---
---     tools = policy.result_cap({ port = port, conf = conf })(
---         knl_adapter.tools({ read_spec, edit_spec })
---     )
---
--- Why this is not the fold's job. `window{ fit }` drops whole BEATS until
--- the request fits, and the newest beat is the one it must not drop — it
--- holds the tool_result the model is waiting for. So a single result larger
--- than the window is the one shape no fold can absorb: nothing is left to
--- drop and the run stops. Bounding what one call may answer is what keeps
--- that from happening, and it can only be done where the result is, which
--- is after the tool ran.
---
--- Why it is not the tool's job either. What counts as large is the window's
--- fraction, and the window belongs to the model: a 16 KB read is a fifth of
--- a 32k context and a rounding error in a million. A byte limit written into
--- a tool would be a constant standing in for something the Port knows, so
--- the rule lives here, in the shell, and reads the Port for the number —
--- the same arrangement `window{ fit }` and `tokens` already have.
---
--- A refused call is answered, not raised: the tool returns
--- `{ ok = false, reason = "result_too_large", ... }` — the shape `std.fs`'
--- own refusals use — carrying the size, the limit and what to do instead,
--- so the model narrows its next call rather than being told nothing.
--- Everything else passes through untouched, including a handler that
--- answered a refusal of its own.
---
--- @param opts table  { port, conf?, share? }
--- @return function bind  fn(tools) -> tools (a new map; the argument is not changed)
function M.result_cap(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.result_cap: opts must be a table", 2)
    end
    only(opts, { port = true, conf = true, share = true }, "policy.result_cap")
    if type(opts.port) ~= "table" or type(opts.port.count) ~= "function" or type(opts.port.profile) ~= "function" then
        error("policy.result_cap: port must answer count(request, conf) and profile(conf)", 2)
    end
    if opts.conf ~= nil and type(opts.conf) ~= "table" then
        error("policy.result_cap: conf must be a table when given", 2)
    end
    if opts.share ~= nil and (type(opts.share) ~= "number" or opts.share <= 0 or opts.share > 1) then
        error("policy.result_cap: share must be a number in (0, 1], got " .. tostring(opts.share), 2)
    end
    shape.assert_dev(opts, RESULT_CAP_OPTS, "policy.result_cap opts")

    local port, conf = opts.port, opts.conf
    local share = opts.share or DEFAULT_RESULT_SHARE

    return function(tools)
        if type(tools) ~= "table" then
            error("policy.result_cap: tools must be the device's map of name -> entry", 2)
        end
        local out = {}
        for name, entry in pairs(tools) do
            if type(entry) ~= "table" or type(entry.handler) ~= "function" then
                error("policy.result_cap: tool '" .. tostring(name) .. "' has no handler", 2)
            end
            local capped = {}
            for key, value in pairs(entry) do
                capped[key] = value
            end
            local handler = entry.handler
            capped.handler = function(args)
                local result = handler(args)
                -- Measured as the kernel will render it — a string verbatim,
                -- anything else as JSON — because that rendering is what
                -- reaches the request, not the table.
                local text = type(result) == "string" and result or std.json.encode(result)
                local limit = math.floor(request_limit(port:profile(conf), "policy.result_cap") * share)
                local tokens = port:count({ messages = { { role = "user", content = text } } }, conf)
                if type(tokens) ~= "number" then
                    error("policy.result_cap: port:count must answer a number, got " .. tostring(tokens), 2)
                end
                if tokens <= limit then
                    return result
                end
                return {
                    ok = false,
                    reason = "result_too_large",
                    tokens = tokens,
                    limit = limit,
                    error = string.format(
                        "'%s' answered %d tokens and one result may take at most %d — the whole conversation "
                            .. "has to fit the model's window. Ask for a smaller piece: a narrower range, "
                            .. "fewer items, one file at a time.",
                        tostring(name),
                        tokens,
                        limit
                    ),
                }
            end
            out[name] = capped
        end
        return out
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
        -- Checked where it is bound rather than at the first beat: a filter
        -- that raised on its first call would be reported as a filter
        -- failure, which is not what went wrong.
        needs_session(session, "policy.carry bind")
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
    verdict = {
        args = { arg_of(VERDICT_ARG, "opts") },
        returns = "verdict — fn(session, out) -> { ok, checked, changed?, result?, reason? }",
        members = {
            verdict = {
                args = { SESSION_ARG, arg_of(T.table, "out (the beat's answer)") },
                returns = "table — { ok, checked, changed?, result?, reason? }",
            },
            run = {
                args = {},
                returns = "table — { ok, stdout?, stderr?, exit_code? }",
            },
            changed = {
                args = { SESSION_ARG },
                returns = "boolean — did this run change anything",
            },
        },
    },
    result_cap = {
        args = { arg_of(RESULT_CAP_ARG, "opts") },
        returns = "bind — fn(tools) -> tools",
        members = {
            bind = {
                args = { arg_of(T.table, "tools (the device's map of name -> entry)") },
                returns = "table — the same map, each handler capped",
            },
        },
    },
    repeat_cap = {
        args = { arg_of(REPEAT_CAP_ARG, "opts") },
        returns = "bind — fn(session) -> fn(tools) -> tools",
        members = {
            bind = {
                args = { SESSION_ARG },
                returns = "wrap — fn(tools) -> tools",
            },
            wrap = {
                args = { arg_of(T.table, "tools (the device's map of name -> entry)") },
                returns = "table — the same map, each handler refusing a repeated call",
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
