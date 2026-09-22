--- policy.loop — what a caller's loop asks between beats.
---
--- Four of the eleven, and the kernel never sees any of them: `verdict` (is
--- the thing outside the conversation true yet?), `stagnation` (is this run
--- going in circles?), `retry` (is this failure worth asking again?) and
--- `escalate` (the device for the next beat). Each is a value the loop calls
--- with a session or an Outcome and reads the answer of; none plugs into a
--- device seam.
---
--- Exported through `policy`; see that header for the rules every policy
--- keeps. This file is `require`d by `policy/init.lua` and not meant to be
--- reached for directly.

local kernel = require("knl")
local lshape = require("lshape")
local shared = require("policy.shared")
local T = lshape.t
local shape = lshape.check

local M = {}

-- What this file shares with its siblings, by the names the code was
-- written against (`policy.shared`).
local DEFAULT_SAME = shared.DEFAULT_SAME
local DEFAULT_NO_PROGRESS = shared.DEFAULT_NO_PROGRESS
local DEFAULT_MAX_ATTEMPTS = shared.DEFAULT_MAX_ATTEMPTS
local DEFAULT_VERDICT_KIND = shared.DEFAULT_VERDICT_KIND
local DEFAULT_TIMEOUT_FACTOR = shared.DEFAULT_TIMEOUT_FACTOR
local DEFAULT_TIMEOUT_FLOOR = shared.DEFAULT_TIMEOUT_FLOOR
local DEFAULT_TIMEOUT_MEASURE = shared.DEFAULT_TIMEOUT_MEASURE
local callable = shared.callable
local whole_at_least = shared.whole_at_least
local only = shared.only
local whole_log = shared.whole_log
local beats_of = shared.beats_of
local canonical = shared.canonical
local data_of = shared.data_of
local FUNCTION = shared.FUNCTION
local CALLABLE = shared.CALLABLE
local needs_session = shared.needs_session
local opts_contract = shared.opts_contract
local BEAT_RECORD = shared.BEAT_RECORD
local arg_of = shared.arg_of
local SESSION_ARG = shared.SESSION_ARG
local OUTCOME_ARG = shared.OUTCOME_ARG

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
        -- Stamped with the beat it judges, in the envelope's label bag: the
        -- id is `meta.beat`. A verdict that was handed no beat carries no
        -- label rather than an empty one.
        local judged = type(out) == "table" and out.beat or nil
        session:append({
            kind = kind,
            meta = judged ~= nil and { beat = judged } or nil,
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

--- The contracts this file publishes; `policy.shapes` gathers them.
M.shapes = {
    stagnation_opts = STAGNATION_OPTS,
    retry_opts = RETRY_OPTS,
    escalate_opts = ESCALATE_OPTS,
}

--- This file's entries in `policy.shapes.api`, gathered there.
M.api = {
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
}

return M
