--- policy.room — the policies that size things against the model's window.
---
--- Four of the eleven, and the one split they all read: where the prompt ends
--- and the reply begins, decided off the Port's profile (`context_window`,
--- `max_output`) by `request_limit` for the prompt's side and `reply_room`
--- for the reply's. `window` is the fold that keeps as many beats as fit,
--- `tokens` the cost that charges a beat its request, `result_cap` the wrap
--- that keeps one tool result under a share of the window, and
--- `thinking_cap` the filter that tells the model where its reasoning has to
--- stop for the call after it to fit. None of them holds a number about the
--- window of its own — the Port declares it, and this file asks.
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
local DEFAULT_RESULT_SHARE = shared.DEFAULT_RESULT_SHARE
local ARRAY_TAG = shared.ARRAY_TAG
local whole_at_least = shared.whole_at_least
local only = shared.only
local whole_log = shared.whole_log
local beat_of = shared.beat_of
local needs_session = shared.needs_session
local opts_contract = shared.opts_contract
local arg_of = shared.arg_of
local EVENTS_ARG = shared.EVENTS_ARG
local SESSION_ARG = shared.SESSION_ARG

--- What `policy.window` is configured with.
local WINDOW_OPTS, WINDOW_ARG = opts_contract({
    tail = T.number:describe("how many beats the request keeps; a whole number >= 1"):is_optional(),
    keep_seed = T.boolean
        :describe("also keep every event before the first beat (the caller's seed), ahead of the window; default false")
        :is_optional(),
    fit = T.table
        :describe(
            "{ port, conf?, reserve? }: keep as many beats as fit the port's context window — "
                .. "port:count(request, conf) + profile.max_output + reserve <= profile.context_window. "
                .. "`reserve` is a whole number of tokens held back for the reply when the wire sends no "
                .. "cap; it holds back only what profile.max_output does not already cover"
        )
        :is_optional(),
})

--- What `policy.split` is asked with: the Port and conf the numbers come
--- from, the `reserve` the fold was given, and — for the reply's side — what
--- the request already costs.
local SPLIT_OPTS, SPLIT_ARG = opts_contract({
    port = T.table:describe("an LLM Port: profile(conf) for the window and the reply's cap"),
    conf = T.table:describe("the conf the port is opened with"):is_optional(),
    reserve = T.number
        :describe("tokens the fold holds back for the reply — the same number window{ fit.reserve } was given")
        :is_optional(),
    used = T.number
        :describe("what a request costs by the Port's count; given, the answer carries the reply's `room`")
        :is_optional(),
})

--- What `policy.split` answers.
local SPLIT = T.shape({
    window = T.number:describe("profile.context_window"),
    max_output = T.number:describe("profile.max_output, or 0 for a wire with no cap"),
    limit = T.number:describe("the most a request may cost — the prompt's side"),
    held = T.number:describe("what `reserve` held back beyond max_output, and so is not in `limit`"),
    room = T.number
        :describe("the reply's side for a request costing `used`: min(window - used, max_output)")
        :is_optional(),
}, { open = false })

--- What `policy.tokens` is configured with: the Port whose counting the cost
--- delegates to, and the conf that Port was (or will be) opened with.
local TOKENS_OPTS, TOKENS_ARG = opts_contract({
    port = T.table:describe("an LLM Port: anything answering count(request, conf) -> integer"),
    conf = T.table:describe("the conf the port is opened with; forwarded to count"):is_optional(),
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

--- What `policy.thinking_cap` is configured with. One number of tokens and
--- the Port that knows the window and the reply's cap; `budget` is the
--- caller's own ceiling on the reasoning, when it has one. There is no
--- `reserve`: what the fold held back for the reply is read off the same
--- profile the fold read it from, not named a second time.
local THINKING_CAP_OPTS, THINKING_CAP_ARG = opts_contract({
    port = T.table:describe("an LLM Port: profile(conf) for the window and cap, count(request, conf) for the request"),
    conf = T.table:describe(
        "the conf the port is opened with; its `thinking` has to turn reasoning on (true, or a table whose "
            .. "enabled is not false)"
    ),
    call_reserve = T.number:describe("tokens kept out of the reasoning for the tool call that follows it"),
    budget = T.number:describe("the most reasoning this run may take, whatever the room says"):is_optional(),
})

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
--- WHAT IT REPORTS, beside the slice. A window is a decision about what the
--- model is not going to be told, and that decision leaves no trace in the
--- request it produces — the request simply begins later. So the cut says
--- what it did: which beats went (oldest first), how many stayed, and whether
--- the seed is still ahead of them. `knl.beat` records it on the
--- `llm_request` (`knl.shapes.window_report`), which is what turns "the model
--- forgot" into a fact somebody can read back out of the log.
---
--- The three counted fields of that report (`before` / `after` / `limit`) are
--- not this function's: a window of n beats counts nothing, and the tokens
--- are `fit`'s to fill in.
---
--- @param events table|nil  a session's events, in seq order
--- @param tail number  how many beats to keep
--- @param keep_seed boolean|nil  keep the events before the first beat too
--- @return table  the slice, in seq order
--- @return table  what the cut did: { dropped, kept, seed_kept }
local function window_slice(events, tail, keep_seed)
    events = events or {}
    local order, first_at = {}, {}
    for i, ev in ipairs(events) do
        local id = beat_of(ev)
        if id ~= nil and first_at[id] == nil then
            first_at[id] = i
            order[#order + 1] = id
        end
    end
    if #order <= tail then
        -- Nothing went, so nothing ahead of the window went either: the
        -- whole log IS the request.
        return events, { dropped = setmetatable({}, ARRAY_TAG), kept = #order, seed_kept = true }
    end
    local cut = #order - tail
    local dropped = setmetatable({}, ARRAY_TAG)
    for i = 1, cut do
        dropped[i] = order[i]
    end
    local from = first_at[order[cut + 1]]
    local slice = {}
    if keep_seed then
        for i = 1, first_at[order[1]] - 1 do
            slice[#slice + 1] = events[i]
        end
    end
    for i = from, #events do
        slice[#slice + 1] = events[i]
    end
    return slice, { dropped = dropped, kept = tail, seed_kept = keep_seed == true }
end

--- How many beats `events` holds.
local function beat_count(events)
    local seen, n = {}, 0
    for _, ev in ipairs(events or {}) do
        local id = beat_of(ev)
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
    -- A whole number of tokens and nothing else. A reserve computed per fold
    -- would be a second policy hiding inside an opt, and the report could no
    -- longer say, before the counting, how much of the window is not the
    -- request's. The type and the value are both in the raise: the two ways
    -- to get this wrong are a function where a number goes and a negative
    -- number, and they read nothing alike.
    if fit.reserve ~= nil and not whole_at_least(fit.reserve, 0) then
        error(
            who
                .. ": fit.reserve must be a whole number >= 0, got "
                .. type(fit.reserve)
                .. " ("
                .. tostring(fit.reserve)
                .. ")",
            3
        )
    end
    return fit.port, fit.conf, fit.reserve
end

--- The two numbers the window is split by, read off the Port's profile and
--- checked once for every policy that sizes anything against them: the
--- window, and the cap the wire puts on the reply (0 when it puts none —
--- `profile.max_output`, the conf's `max_tokens` or the impl's default). Loud
--- when the profile does not say — a window that guessed would be the 400 it
--- exists to prevent, one step later.
---
--- `request_limit` and `reply_room` below are the two sides of that one split
--- — how much the prompt may take, how much the reply then has — and both
--- read these numbers here rather than each doing its own arithmetic, so
--- there is one answer to "where does the prompt end and the reply begin".
---
--- @param profile table  the Port's answer to profile(conf)
--- @param who string  the policy's name, for the raise
--- @param level number  where the raise points, as `error` counts it
--- @return number window  profile.context_window
--- @return number output  profile.max_output, or 0 for a wire with no cap
local function profile_split(profile, who, level)
    if type(profile) ~= "table" then
        error(who .. ": port:profile(conf) must answer a table, got " .. tostring(profile), level)
    end
    local window, output = profile.context_window, profile.max_output
    if not whole_at_least(window, 1) then
        error(
            who
                .. ": the port's profile names no context_window; declare it in the conf the port is opened with "
                .. "(context_window = <tokens>) or on the port",
            level
        )
    end
    if output == nil then
        output = 0
    elseif not whole_at_least(output, 0) then
        error(who .. ": profile.max_output must be a whole number >= 0, got " .. tostring(output), level)
    end
    if output >= window then
        error(
            string.format("%s: profile.max_output (%d) leaves no room in context_window (%d)", who, output, window),
            level
        )
    end
    return window, output
end

--- The tokens a request may take, read off the Port's profile: the window
--- less the room the answer needs, less what `reserve` holds back for a reply
--- the wire puts no cap on.
---
--- `reserve` holds back only what `profile.max_output` does not already: that
--- cap comes out of the window here anyway, and a cap on the wire bounds the
--- reply by itself, so a reserve at or under one that is there holds nothing
--- extra. The held amount is `max(0, reserve - max_output)`, and it is
--- answered beside the limit so the fold can report what it gave up.
---
--- @param profile table  the Port's answer to profile(conf)
--- @param who string  the factory's name, for the raise
--- @param reserve number|nil  tokens held back for the reply; nil is none
--- @return number limit  the tokens one request may take
--- @return number held  what the reserve held back beyond profile.max_output
local function request_limit(profile, who, reserve)
    local window, output = profile_split(profile, who, 4)
    local held = math.max(0, (reserve or 0) - output)
    local limit = window - output - held
    if limit < 1 then
        error(
            string.format(
                "%s: fit.reserve (%d) beside profile.max_output (%d) leaves no room in context_window (%d)",
                who,
                reserve or 0,
                output,
                window
            ),
            3
        )
    end
    return limit, held
end

--- The tokens the reply may take once the request is known — the other side
--- of the split `request_limit` reads: the window less the request, and no
--- more than the cap the wire carries when it carries one. The fold has
--- already held `reserve` back to make this room; it is not subtracted again
--- here, because it IS this room.
---
--- May be zero or negative — a request the fold let through on an estimate
--- that the server counts higher — and that is the caller's to read as "no
--- room", not a raise: the request is already built and on its way.
---
--- @param profile table  the Port's answer to profile(conf)
--- @param who string  the policy's name, for the raise
--- @param used number  what the request costs, by the Port's count
--- @param level number  where a raise about the profile points
--- @return number room  tokens the reply has, cap and window both honoured
local function reply_room(profile, who, used, level)
    local window, output = profile_split(profile, who, level)
    local room = window - used
    if output > 0 and output < room then
        room = output
    end
    return room
end

--- The split, as a value: where the prompt ends and the reply begins, for
--- the Port and conf named, read out rather than acted on.
---
---     policy.split({ port = port, conf = conf, reserve = 6144 })
---     -- { window = 32768, max_output = 0, limit = 26624, held = 6144 }
---     policy.split({ port = port, conf = conf, reserve = 6144, used = 24984 })
---     -- { ..., room = 7784 }
---
--- The same three readings `window`, `result_cap` and `thinking_cap` make
--- for themselves (`profile_split` / `request_limit` / `reply_room`), handed
--- back together so a caller can see the numbers a run is being sized by —
--- to log them beside a beat, to check a conf before opening a session, or
--- to size something of its own against the same split rather than a second
--- guess at it. It is not a policy: it plugs into no seam, and nothing here
--- changes because it was called.
---
--- `room` is on the answer only when `used` was given: without a request
--- there is no reply's side to read, and a zero there would be a number
--- about nothing. The raises are the profile's — no window, a cap that
--- leaves no room — and they point at the caller.
---
--- @param opts table  { port, conf?, reserve?, used? }
--- @return table  policy.shapes.split
function M.split(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.split: opts must be a table", 2)
    end
    only(opts, { port = true, conf = true, reserve = true, used = true }, "policy.split")
    if type(opts.port) ~= "table" or type(opts.port.profile) ~= "function" then
        error("policy.split: port must answer profile(conf)", 2)
    end
    if opts.conf ~= nil and type(opts.conf) ~= "table" then
        error("policy.split: conf must be a table when given", 2)
    end
    if opts.reserve ~= nil and not whole_at_least(opts.reserve, 0) then
        error("policy.split: reserve must be a whole number >= 0 (tokens), got " .. tostring(opts.reserve), 2)
    end
    if opts.used ~= nil and not whole_at_least(opts.used, 0) then
        error("policy.split: used must be a whole number >= 0 (tokens), got " .. tostring(opts.used), 2)
    end
    shape.assert_dev(opts, SPLIT_OPTS, "policy.split opts")

    local profile = opts.port:profile(opts.conf)
    local window, output = profile_split(profile, "policy.split", 2)
    local limit, held = request_limit(profile, "policy.split", opts.reserve)
    local out = { window = window, max_output = output, limit = limit, held = held }
    if opts.used ~= nil then
        out.room = reply_room(profile, "policy.split", opts.used, 2)
    end
    return out
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
--- `fit.reserve` is the room held back for the REPLY. `profile.max_output` is
--- a cap the wire carries and the limit above already subtracts it, but a
--- port that sends no cap leaves the whole window to the request — and a
--- fold that filled it would leave the model nowhere to answer. Measured
--- 2026-09-13 in a sibling lane: in=32,718 / out=50 for two beats in a row.
--- `reserve` holds back what the cap does not, `max(0, reserve - max_output)`,
--- so a reserve at or under a cap that is there holds nothing extra and the
--- window is the same one it was. It is a whole number of tokens; a reserve
--- that would leave the request less than one token raises, naming the three
--- numbers it was reached from.
---
--- A number, and not the largest reply seen so far. That largest is a peak
--- and not a choice: it holds nothing back until a reply has already been cut
--- off once, and half the window afterwards [measured 2026-09-14 in the same
--- lane]. How much room the reply needs is the caller's to say, and a number
--- is how it is said.
---
---     policy.window({ fit = { port = port, conf = conf, reserve = 4096 }, keep_seed = true })
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
--- What the fold reports
---   The fold answers the request and, BESIDE IT, a report of what it left
---   out — `knl.shapes.window_report`:
---
---       { dropped = { <beat id>, ... },  -- oldest first, empty when none
---         kept = <beats kept>, seed_kept = <boolean>,
---         before = <tokens>?, after = <tokens>?, limit = <tokens>?,
---         reserve = <tokens>? }
---
---   `before` is what the whole log cost (or the `tail` window, when `tail`
---   caps it), `after` what was sent, `limit` the room the candidate was
---   measured against, and `reserve` how much of the window is held back for
---   the reply and therefore not in `limit` (0 when `fit.reserve` is absent
---   or the wire's cap already covered it). All four are absent for a window
---   of n beats, which never counts anything.
---
---   A caller that takes one value is unaffected — Lua drops the extra
---   return, and `knl.fold` itself answers nothing beside the request.
---   `knl.beat` is the one reader: it records the report on the `llm_request`
---   event as `data.window`, so what a run dropped is in the log rather than
---   only in the moment. Nothing in the kernel decides anything on it.
---
--- @param opts table  { tail = <whole number >= 1>?, keep_seed = <boolean>?, fit = { port, conf?, reserve = <whole number >= 0>? }? } — `tail` is required without `fit`
--- @return function fold  fn(events, device) -> request, report
--- @return function|nil fits  fn(session, device) -> nil | "context", tokens, limit (with `fit` only)
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
    local port, conf, reserve
    if opts.fit ~= nil then
        port, conf, reserve = port_for_fit(opts.fit, "policy.window")
    end
    shape.assert_dev(opts, WINDOW_OPTS, "policy.window opts")

    local tail = opts.tail
    local keep_seed = opts.keep_seed == true
    if port == nil then
        return function(events, device)
            local slice, report = window_slice(events, tail, keep_seed)
            -- No `before` / `after` / `limit`: this form counts nothing, and
            -- a number here would be one this fold never asked for.
            return kernel.fold(slice, device), report
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
    ---
    --- It answers in one of two forms, and the failing one says HOW the
    --- number was reached — because the two ways of failing want two
    --- different sentences. `"beat"` is the newest beat costing more than the
    --- window; `"seed"` is a history with no `meta.beat` on any event, where
    --- there was no beat to keep and the whole list was counted as the seed.
    --- The second is a caller's mistake and not a model's limit, and a raise
    --- that called it "the newest beat" would send them looking for a beat
    --- that is not there.
    ---
    --- @return table|nil request  the fitted request, or nil when none fits
    --- @return table|nil report  what it dropped to get there (with a request)
    --- @return number|nil tokens  what the smallest candidate cost (with nil)
    --- @return number|nil limit  the room the profile left (with nil)
    --- @return string|nil counted  "beat" | "seed" — what those tokens are of
    local function largest_fitting(events, device)
        -- The room a request may take, and what `fit.reserve` held back to
        -- leave it. Both come off the profile read for this fold, so a Port
        -- that learns its window late is read at its word every time.
        local limit, held = request_limit(port:profile(conf), "policy.window", reserve)
        local n = beat_count(events)
        local most = tail and math.min(tail, n) or n

        local function fold_at(k)
            local slice, report = window_slice(events, k, keep_seed)
            local request = kernel.fold(slice, device)
            local tokens = port:count(request, conf)
            if type(tokens) ~= "number" then
                error("policy.window: port:count must answer a number, got " .. tostring(tokens), 3)
            end
            return request, tokens, report
        end

        --- The chosen candidate's report, with the counting written onto it.
        --- `limit` is the effective one — the number this candidate was
        --- measured against — and `reserve` is how much of the window is not
        --- in it, so a reader of the log can tell a short window from one cut
        --- short to leave the reply room.
        local function counted(report, before, after)
            report.before, report.after, report.limit = before, after, limit
            report.reserve = held
            return report
        end

        if most < floor then
            -- No beat yet: the seed alone is the whole conversation, and
            -- there is nothing to choose between.
            local request, tokens, report = fold_at(0)
            if tokens <= limit then
                return request, counted(report, tokens, tokens)
            end
            return nil, nil, tokens, limit, "seed"
        end

        local whole, whole_tokens, whole_report = fold_at(most)
        if whole_tokens <= limit then
            return whole, counted(whole_report, whole_tokens, whole_tokens)
        end

        -- Everything fits at `lo` or below and nothing at `hi` or above;
        -- `floor` is the smallest window there is, and it has already failed
        -- when the loop ends without an answer.
        local lo, hi = floor, most
        local best, best_tokens, best_report = nil, nil, nil
        local smallest_tokens = nil
        while lo <= hi do
            local mid = (lo + hi) // 2
            local request, tokens, report = fold_at(mid)
            if tokens <= limit then
                best, best_tokens, best_report = request, tokens, report
                lo = mid + 1
            else
                hi = mid - 1
                if mid == floor then
                    smallest_tokens = tokens
                end
            end
        end
        if best then
            -- `before` is the largest candidate's count — the whole log, or
            -- the `tail` window when `tail` capped it — which is the number
            -- the dropping was measured against.
            return best, counted(best_report, whole_tokens, best_tokens)
        end
        if smallest_tokens == nil then
            local _, tokens = fold_at(floor)
            smallest_tokens = tokens
        end
        return nil, nil, smallest_tokens, limit, "beat"
    end

    local fold = function(events, device)
        local request, report, tokens, limit, counted = largest_fitting(events, device)
        if request then
            return request, report
        end
        if counted == "seed" then
            error(
                string.format(
                    "policy.window: no event in this history is marked with a beat, so the whole of it was "
                        .. "counted as the seed: %d tokens > %d. Mark the events of each beat with meta.beat "
                        .. "(knl.beat does), or pass the session so the window can see the beats.",
                    tokens,
                    limit
                ),
                2
            )
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

    --- The same question, asked before the beat rather than inside it. When
    --- the answer is `"context"`, the tokens the smallest candidate cost and
    --- the limit come beside it, so the caller can say by how much.
    local fits = function(session, device)
        needs_session(session, "policy.window fits")
        local request, _, tokens, limit = largest_fitting(whole_log(session, "policy.window fits"), device or {})
        if request == nil then
            return "context", tokens, limit
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
-- thinking_cap — where the reasoning has to stop for the call to fit
-- ============================================================

--- Build a `filter` that sends the reasoning's stop point with every request.
---
---     local device = knl.device({
---         llm = port:open(conf), -- conf.thinking turns reasoning on
---         filters = {
---             policy.carry({ max_bytes = 512 })(session),
---             policy.thinking_cap({ port = port, conf = conf, call_reserve = 3072 }),
---         },
---     })
---
--- THE MODEL CANNOT SEE ITS ROOM, and its reasoning comes out of the same
--- allowance as the answer. A beat whose prompt has grown has less left for
--- both, and nothing in the conversation says so; the model reasons at the
--- length it would have reasoned at on an empty window, fills what is left,
--- and the tool call it was about to make never gets written
--- [measured 2026-09-13 in a sibling lane: a prompt of 24,984 tokens in a 32k
---  window, 7,121 of them spent thinking, and the call that followed arrived
---  as `{}`]. This filter computes where the reasoning has to stop for the
--- call to still fit, and sends that number with the request.
---
---     room = window - port:count(request, conf)      -- and no more than
---                                                    -- profile.max_output,
---                                                    -- where the wire has a cap
---     T    = min(room - call_reserve, budget)
---
--- The room is the reply's, and it is read where the window's split between
--- prompt and reply is decided — the same profile `policy.window` sized the
--- request against, through `reply_room` beside its `request_limit` — rather
--- than computed again here. So a `max_tokens` on the conf bounds it the way
--- it bounds the reply on the wire, and what the fold held back as `reserve`
--- is not named again: it IS this room, and a filter that subtracted it a
--- second time would leave the most crowded beat — the one the stop point is
--- for — with no stop point at all [the prompt above, 24,984 in 32k with a
--- reserve of 6,144: the room is 7,784; 7,784 less 6,144 again is less than
--- a `call_reserve` of 3,072, and nothing would be sent]. `call_reserve` is
--- the part of the room the tool call needs. When `T` is under one token the
--- request goes through untouched: there is no stop point worth sending, and
--- saying "stop after 0 tokens" is not one.
---
--- REASONING HAS TO BE ON IN THE CONF — `thinking = true`, or a table whose
--- `enabled` is not false — and a conf that says nothing is refused when the
--- filter is built. The adapter reads any `thinking` table on the request as
--- reasoning turned on (`llm_proto.normalize_thinking`: `enabled` defaults to
--- true), and the request's `thinking` replaces the conf's wholesale
--- (`knl_adapter`'s build merges the conf and then the request, field by
--- field). A budget sent to a conf that never asked for reasoning would
--- therefore switch reasoning on by itself, on the wire, on every beat — a
--- side effect no `call_reserve` asked for. The conf's own keys (`effort`,
--- `kwarg`, `mode`) are carried across under the budget.
---
--- It reads no log and holds nothing between beats: the room is derived from
--- the request it is handed, every time, through the Port's own count. When
--- no filter before this one changed the request, that is the count the fold
--- already asked for, and the Port answers it from cache.
---
--- WHERE IT REACHES THE WIRE. `request.thinking.budget_tokens` is a
--- per-request budget, and vLLM is the dialect that takes one
--- (`thinking_token_budget`); on llama.cpp and Ollama the equivalent is a
--- server flag, and `llm_proto.openai` logs a warning and sends nothing.
--- Anthropic deprecated its own `budget_tokens` in 4.6. So this is opt-in and
--- does nothing on a wire with nowhere to put it.
---
--- NO OTHER HARNESS SIZES REASONING PER REQUEST. Claude Code, Codex,
--- OpenHands, Aider, Cline, SWE-agent and mini-swe-agent all fix an effort for
--- the run; what varies with the remaining room is done provider-side where it
--- is done at all. The evidence for doing it here is a sibling lane's
--- [measured 2026-09-14: on a 32k window, the beats where the stop point fired
---  — the model thinking exactly to the budget — still delivered their tool
---  call, where the unbounded beats had filled the window and delivered
---  nothing].
---
--- The request is not changed in place: what comes back is a copy with
--- `thinking` on it.
---
--- @param opts table  { port, conf, call_reserve, budget? }
--- @return function filter  fn(request) -> request
function M.thinking_cap(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.thinking_cap: opts must be a table", 2)
    end
    only(opts, { port = true, conf = true, call_reserve = true, budget = true }, "policy.thinking_cap")
    if type(opts.port) ~= "table" or type(opts.port.count) ~= "function" or type(opts.port.profile) ~= "function" then
        error("policy.thinking_cap: port must answer count(request, conf) and profile(conf)", 2)
    end
    if type(opts.conf) ~= "table" then
        error("policy.thinking_cap: conf must be the table the port is opened with", 2)
    end
    -- The conf has to have turned reasoning on itself. The adapter reads a
    -- thinking table as "on" unless it says `enabled = false`, and the
    -- request's table replaces the conf's — so a budget sent over a conf that
    -- said nothing would be the thing that turned reasoning on.
    local declared = opts.conf.thinking
    local on = declared == true or (type(declared) == "table" and declared.enabled ~= false)
    if not on then
        error(
            "policy.thinking_cap: the conf does not turn reasoning on (thinking = true, or { enabled = true, ... }); "
                .. "a stop point is sent as a thinking table, and one sent over a conf that asked for no reasoning "
                .. "would switch it on by itself",
            2
        )
    end
    -- Required, and the one opt that is: a cap with nothing kept back for the
    -- call is a cap that lets the reasoning run to the end of the window,
    -- which is what happens without this policy at all.
    if not whole_at_least(opts.call_reserve, 0) then
        error(
            "policy.thinking_cap: call_reserve must be a whole number >= 0 (tokens kept out of the reasoning "
                .. "for the call that follows it), got "
                .. tostring(opts.call_reserve),
            2
        )
    end
    if opts.budget ~= nil and not whole_at_least(opts.budget, 1) then
        error("policy.thinking_cap: budget must be a whole number >= 1 (tokens), got " .. tostring(opts.budget), 2)
    end
    shape.assert_dev(opts, THINKING_CAP_OPTS, "policy.thinking_cap opts")

    local port, conf = opts.port, opts.conf
    local call_reserve, budget = opts.call_reserve, opts.budget

    return function(request)
        local used = port:count(request, conf)
        if type(used) ~= "number" then
            error("policy.thinking_cap: port:count must answer a number, got " .. tostring(used), 0)
        end
        -- The reply's room, off the profile the fold split the window by: the
        -- window less this request, under the wire's cap where there is one.
        local room = reply_room(port:profile(conf), "policy.thinking_cap", used, 0)
        local stop = math.floor(room - call_reserve)
        if budget and budget < stop then
            stop = budget
        end
        if stop < 1 then
            -- Nothing sensible to send. The request goes as it is and the
            -- server's own default decides, which is the same thing that
            -- happens on every beat without this filter.
            return request
        end
        -- The conf's own thinking keys, under the budget: `true` carries as
        -- `enabled`, a table as its keys — the construction above has already
        -- said it is one of those two.
        local thinking = {}
        if type(declared) == "table" then
            for key, value in pairs(declared) do
                thinking[key] = value
            end
        else
            thinking.enabled = true
        end
        thinking.budget_tokens = stop
        local out = {}
        for key, value in pairs(request) do
            out[key] = value
        end
        out.thinking = thinking
        return out
    end
end

--- The contracts this file publishes; `policy.shapes` gathers them.
M.shapes = {
    window_opts = WINDOW_OPTS,
    thinking_cap_opts = THINKING_CAP_OPTS,
    split_opts = SPLIT_OPTS,
    split = SPLIT,
}

--- This file's entries in `policy.shapes.api`, gathered there.
M.api = {
    split = {
        args = { arg_of(SPLIT_ARG, "opts") },
        returns = SPLIT,
    },
    window = {
        args = { arg_of(WINDOW_ARG, "opts") },
        returns = 'fold — fn(events, device) -> request, report; and, with `fit`, fits — fn(session, device) -> nil | "context"',
        members = {
            fold = {
                -- Two values: the request, and what the window left out
                -- (`knl.shapes.window_report`, which `knl.beat` records on
                -- the llm_request). A caller taking one is unaffected.
                args = { EVENTS_ARG, arg_of(T.table, "device (read for system / tools)") },
                returns = "knl.shapes.request, knl.shapes.window_report",
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
    thinking_cap = {
        args = { arg_of(THINKING_CAP_ARG, "opts") },
        returns = "filter — fn(request) -> request",
        members = {
            filter = {
                args = { arg_of(kernel.shapes.request, "request") },
                returns = kernel.shapes.request,
            },
        },
    },
}

return M
