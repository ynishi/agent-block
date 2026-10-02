--- policy.compact — when the window is nearly full, and the facts a summary
--- must not lose.
---
--- Two of the fourteen, on a file of their own because they are the two the
--- loop reaches for when the request has grown past what dropping beats can
--- answer: `compact` says WHEN the record should be folded into a summary
--- and builds the one request that writes it, and `ledger` reads the facts
--- of the run off the log — which tools ran, on what, what the checks said
--- — so the summary the model writes is carried beside a reading nobody
--- wrote from memory.
---
--- Neither appends. The summarising beat is the LOOP's (`knl.beat` with the
--- device `compact` hands the fold for), and the `summary` event is the
--- loop's to append (`policy.shapes.summary`); `policy.window{ from_summary }`
--- then folds the request from that event instead of from the beats it
--- stands for. The three together are the whole mechanism, and each is a
--- value in a seam the kernel already has.
---
--- Exported through `policy`; see that header for the rules every policy
--- keeps. This file is `require`d by `policy/init.lua` and not meant to be
--- reached for directly.

local kernel = require("knl")
local lshape = require("lshape")
local shared = require("policy.shared")
local T = lshape.t
local shape = lshape.check

local record CompactOpts
    at: number
    min_beats: integer
    prompt: string
    fold: function({shared.Event}, kernel.Device): kernel.Request, kernel.WindowReport
end
local record LedgerOpts
    track: {string}
    kinds: {string}
end

local record M
    type Fold = function({shared.Event}, kernel.Device): kernel.Request, kernel.WindowReport
    --- The question the loop asks between beats: nil, or why a compaction
    --- is due — `"dropped"` (the last fold left beats out) or `"share"`
    --- (the last request took `at` of the window or more).
    type Due = function(kernel.Session): string
    type CompactFactory = function(CompactOpts): Due, Fold
    type LedgerFn = function(kernel.Session): shared.Ledger
    type LedgerFactory = function(?LedgerOpts): LedgerFn
    shapes: {string:lshape.Schema}
    api: {string:shared.ApiEntry}
end

-- What this file shares with its siblings, by the names the code was
-- written against (`policy.shared`).
local DEFAULT_COMPACT_AT = shared.DEFAULT_COMPACT_AT
local DEFAULT_COMPACT_MIN_BEATS = shared.DEFAULT_COMPACT_MIN_BEATS
local DEFAULT_COMPACT_PROMPT = shared.DEFAULT_COMPACT_PROMPT
local DEFAULT_LEDGER_TRACK = shared.DEFAULT_LEDGER_TRACK
local DEFAULT_LEDGER_KINDS = shared.DEFAULT_LEDGER_KINDS
local SUMMARY_KIND = shared.SUMMARY_KIND
local SKIPPED_KIND = shared.COMPACT_SKIPPED_KIND
local ARRAY_TAG = shared.ARRAY_TAG
local whole_at_least = shared.whole_at_least
local opts_of = shared.opts_of
local whole_log = shared.whole_log
local beat_of = shared.beat_of
local data_of = shared.data_of
local needs_session = shared.needs_session
local opts_contract = shared.opts_contract
local arg_of = shared.arg_of
local SESSION_ARG = shared.SESSION_ARG
local EVENTS_ARG = shared.EVENTS_ARG
local LEDGER = shared.LEDGER

--- What `policy.compact` is configured with. `fold` is the caller's own
--- window fold — the request the summary is written from is the request the
--- model has been reading, sized by the same window, and this file builds no
--- second one.
local COMPACT_OPTS, COMPACT_ARG = opts_contract({
    at = T.number
    :describe("the share of the window the last request may take before a compaction is due, 0 < at <= 1; default 0.8")
    :is_optional(),
    min_beats = T.number
    :describe("beats since the last summary (or the start) before one is due again; a whole number >= 1; default 2")
    :is_optional(),
    prompt = T.string:describe("the instruction the summarising request ends with; default: the one in this file"):is_optional(),
    fold = T.fn:describe("the loop's own fold — fn(events, device) -> request, report — that the summarising request is built with"),
})

--- What `policy.ledger` is configured with.
local LEDGER_OPTS, LEDGER_ARG = opts_contract({
    track = T.array_of(T.string)
    :describe('the tool-call argument keys whose distinct values are kept per tool; default { "path" }')
    :is_optional(),
    kinds = T.array_of(T.string)
    :describe('the event kinds read as checks — `data.ok` is the verdict; default { "verify" }')
    :is_optional(),
})

-- ============================================================
-- compact — when the window is nearly full
-- ============================================================

--- The index of the latest compaction mark — a `summary`, or a
--- `compact_skipped` the loop left where a summary came back empty — or 0
--- when there is none.
local function last_cut_at(events: {shared.Event}): integer
    for i = #events, 1, -1 do
        local ev = events[i]
        if ev ~= nil and (ev.kind == SUMMARY_KIND or ev.kind == SKIPPED_KIND) then
            return i
        end
    end
    return 0
end

--- Why a compaction is due, read off what the folds since the last summary
--- reported, or nil.
---
--- The reading is the log's and nothing is counted here: `knl.beat` records
--- what the fold left out and what it cost on every `llm_request`
--- (`knl.shapes.window_report`), so the two facts this decides on are
--- already written. `"dropped"` is the last fold having left beats out — the
--- model has already stopped seeing part of the run, and from here on it
--- forgets more every beat. `"share"` is the last request having cost `at`
--- of the room the fold had, or more, with nothing dropped yet: the next
--- beats will start dropping, and a summary written now is written from a
--- request that still holds everything.
---
--- `min_beats` counts from the latest `summary` OR `compact_skipped`. The
--- second is the mark a loop leaves when the summarising beat answered
--- nothing (`policy.shapes.compact_skipped`); without it a model that
--- answers the summary prompt with an empty reply would be asked again
--- before every beat, one wasted call per beat [measured 2026-10-01 against
--- gemini-2.5-flash over its OpenAI-compatible endpoint: 9 of 12 summary
--- requests in one run came back with 0 output tokens, and each was
--- re-asked on the next beat until this mark existed].
---
--- A fold that counts nothing (`window{ tail }` alone) reports no numbers,
--- and then only `"dropped"` can ever be answered.
---
--- @param events table  the whole log, in seq order
--- @param at number  the share of the window that makes a request full
--- @param min_beats number  how many beats must follow the last summary first
--- @return string|nil  "dropped" | "share" | nil
local function due_of(events: {shared.Event}, at: number, min_beats: integer): string
    local from = last_cut_at(events) + 1
    local beats, seen: integer, {string:boolean} = 0, {}
    local last_report: kernel.WindowReport = nil
    for i = from, #events do
        local ev = events[i]
        local id = beat_of(ev)
        if id ~= nil and not seen[id] then
            seen[id] = true
            beats = beats + 1
        end
        if ev.kind == "llm_request" then
            local report = data_of(ev).window
            if type(report) == "table" then
                last_report = report as kernel.WindowReport
            end
        end
    end
    if beats < min_beats or last_report == nil then
        return nil
    end
    if type(last_report.dropped) == "table" and #last_report.dropped > 0 then
        return "dropped"
    end
    local after, limit = last_report.after, last_report.limit
    if type(after) == "number" and type(limit) == "number" and limit > 0 and after / limit >= at then
        return "share"
    end
    return nil
end

--- `request` with `prompt` as one more user message at the end.
---
--- At the END, because the summary is asked about everything above it, and
--- after a tool_result batch rather than inside one: a user message appended
--- whole breaks no `tool_use` / `tool_result` pairing. The request is rebuilt
--- rather than edited, for the reason `carry` gives: a filter's table reaches
--- the fold's caller and, through the `llm_request` record, the log.
local function append_prompt(request: kernel.Request, prompt: string): kernel.Request
    local out: {string:any} = {}
    for k, v in pairs(request as {string:any}) do
        out[k] = v
    end
    local messages = setmetatable({} as any, ARRAY_TAG as metatable<any>) as {any}
    for _, message in ipairs(request.messages or {}) do
        messages[#messages + 1] = message
    end
    messages[#messages + 1] = { role = "user", content = prompt }
    out.messages = messages
    -- The summary is prose, and a request that still offered the tools
    -- would invite one more call instead; the device the loop beats with
    -- carries none, and this is what makes the fold say so as well.
    out.tools = nil
    return out as kernel.Request
end

--- Build the two values a loop needs to fold its record into a summary: the
--- question it asks between beats, and the fold the summarising beat runs
--- with.
---
---     local room = policy.room({ port = port, conf = conf, reserve = 4096 })
---     local fold, fits = policy.window({ room = room, keep_seed = true, from_summary = true })
---     local due, summarise = policy.compact({ at = 0.8, fold = fold })
---     local device = knl.device({ llm = llm, tools = tools, fold = fold })
---     local writer = knl.device({ llm = llm, fold = summarise })   -- no tools
---     ...
---     local why = due(session)
---     if why ~= nil then
---         local out = knl.beat(session, writer)             -- one beat, the summary
---         session:append({ kind = "summary", meta = { label = "compact", reason = why },
---                          data = { content = <the answer's text>, ledger = ledger(session) } })
---     end
---     local out = knl.beat(session, device)                 -- folds from the summary on
---
--- WHY A SUMMARY AND NOT ONLY A WINDOW. `policy.window` keeps the request
--- under the window by dropping whole beats, oldest first, and says so in the
--- record — but what it drops is gone from the model's view, and a run long
--- enough to drop beats is a run whose model is working from a conversation
--- that begins in the middle. Every harness that runs long has the same
--- answer: past a share of the window, the old part is replaced by a summary
--- the model writes, and the log keeps all of it [Claude Code, Codex,
--- OpenHands, pi, Goose; surveyed 2026-10-01]. The summary alone is not
--- enough — a model summarising its own run drops the facts it got wrong —
--- which is why `ledger` is beside it.
---
--- WHAT IS THE LOOP'S HERE AND WHAT IS NOT. The summarising beat is a beat
--- like any other: the loop runs it, it costs what a beat costs, and its
--- request and answer are in the log under their own beat id. The `summary`
--- event is the loop's to append — nothing in this module writes — and it
--- carries the answer's text and the ledger as the loop read it
--- (`policy.shapes.summary`). `policy.window{ from_summary = true }` is the
--- reader: from the latest `summary` on, the request is the seed, the
--- summary as one user message, and the beats after it — and the summary's
--- place in the log is the cut, so the summarising beat itself is behind it.
---
--- `due` reads the log and holds nothing (the header's rule): the window
--- report `knl.beat` records on every `llm_request` says whether beats were
--- dropped and what the request cost against the fold's limit, and that is
--- the whole reading. It answers nil until `min_beats` beats have followed
--- the last summary, so a seed that alone fills `at` of the window cannot
--- make every beat a compaction. A SUMMARY THAT CAME BACK EMPTY is the
--- loop's to mark: it appends `compact_skipped`
--- (`policy.shapes.compact_skipped`) in the summary's place, `due` counts
--- `min_beats` from that mark as it would from a summary, and
--- `window{ from_summary }` folds through it as if it were not there — the
--- request stays what it was, and the model is not asked again on the very
--- next beat.
---
--- The fold it answers is the caller's `fold` with `prompt` appended as the
--- last user message and the tools left off the request; the report beside
--- it is the caller's fold's, unchanged. A `from_summary` fold handed here
--- means a second compaction summarises the first summary and the beats
--- since — the shape pi and OpenHands take, and the one that keeps the
--- request bounded however long the run.
---
--- @param opts table  { at?, min_beats?, prompt?, fold }
--- @return function due  fn(session) -> nil | "dropped" | "share"
--- @return function fold  fn(events, device) -> request, report — for the summarising beat
function M.compact(opts: CompactOpts): M.Due, M.Fold
    opts = opts_of(opts, { at = true, min_beats = true, prompt = true, fold = true }, "policy.compact")
    if opts.at ~= nil and (type(opts.at) ~= "number" or opts.at <= 0 or opts.at > 1) then
        error("policy.compact: at must be a number in (0, 1], got " .. tostring(opts.at), 2)
    end
    if opts.min_beats ~= nil and not whole_at_least(opts.min_beats, 1) then
        error("policy.compact: min_beats must be a whole number >= 1, got " .. tostring(opts.min_beats), 2)
    end
    if opts.prompt ~= nil and (type(opts.prompt) ~= "string" or opts.prompt == "") then
        error("policy.compact: prompt must be a non-empty string when given", 2)
    end
    if type(opts.fold) ~= "function" then
        error("policy.compact: fold is required — the loop's own fold, fn(events, device) -> request", 2)
    end
    shape.assert_dev(opts, COMPACT_OPTS, "policy.compact opts")

    local at = opts.at or DEFAULT_COMPACT_AT
    local min_beats = opts.min_beats or DEFAULT_COMPACT_MIN_BEATS
    local prompt = opts.prompt or DEFAULT_COMPACT_PROMPT
    local inner = opts.fold

    local due = function(session: kernel.Session): string
        needs_session(session, "policy.compact due")
        return due_of(whole_log(session, "policy.compact due"), at, min_beats)
    end
    local fold = function(events: {shared.Event}, device: kernel.Device): kernel.Request, kernel.WindowReport
        local request, report = inner(events, device)
        return append_prompt(request, prompt), report
    end
    return due, fold
end

-- ============================================================
-- ledger — the facts of the run, read off the log
-- ============================================================

--- The facts of the run so far, as `policy.shapes.ledger` (`ledger_of`).
---
--- Everything here is a count or a name read off an event, and nothing is
--- interpreted: how many beats there were and how many summaries, how many
--- times each tool was called, the distinct values each tool was handed
--- under the `track` keys (the files a read tool read and an edit tool
--- edited, under the usual `path`), and for each `kinds` entry how many
--- checks were recorded, how many passed, and what the last one said.
---
--- @param events table  the whole log, in seq order
--- @param track table  the argument keys kept per tool
--- @param kinds table  the event kinds read as checks
--- @return table  policy.shapes.ledger
local function ledger_of(events: {shared.Event}, track: {string}, kinds: {string}): shared.Ledger
    local is_check: {string:boolean} = {}
    for _, kind in ipairs(kinds) do
        is_check[kind] = true
    end
    local seen: {string:boolean} = {}
    local out: shared.Ledger = { beats = 0, summaries = 0, calls = {}, touched = {}, checks = {} }
    -- The distinct values, as sets while they are gathered; rendered as
    -- sorted arrays below so two readings of one log are one ledger.
    local values: {string:{string:{string:boolean}}} = {}
    for _, ev in ipairs(events) do
        local id = beat_of(ev)
        if id ~= nil and not seen[id] then
            seen[id] = true
            out.beats = out.beats + 1
        end
        local kind = ev.kind as string
        local data = data_of(ev)
        if kind == SUMMARY_KIND then
            out.summaries = out.summaries + 1
        elseif kind == "tool_call" then
            local name = tostring(data.name)
            out.calls[name] = (out.calls[name] or 0) + 1
            local args = data.args
            if type(args) == "table" then
                for _, key in ipairs(track) do
                    local value = (args as {string:any})[key]
                    if type(value) == "string" then
                        local by_key = values[name]
                        if by_key == nil then
                            by_key = {}
                            values[name] = by_key
                        end
                        local set = by_key[key]
                        if set == nil then
                            set = {}
                            by_key[key] = set
                        end
                        set[value as string] = true
                    end
                end
            end
        elseif is_check[kind] then
            local tally = out.checks[kind] or { count = 0, passed = 0 }
            tally.count = tally.count + 1
            if data.ok == true then
                tally.passed = tally.passed + 1
            end
            tally.last_ok = data.ok == true
            out.checks[kind] = tally
        end
    end
    for name, by_key in pairs(values) do
        local touched: {string:{string}} = {}
        for key, set in pairs(by_key) do
            local list: {string} = {}
            for value in pairs(set) do
                list[#list + 1] = value
            end
            table.sort(list)
            touched[key] = setmetatable(list as any, ARRAY_TAG as metatable<any>) as {string}
        end
        out.touched[name] = touched
    end
    return out
end

--- Build the reading a loop takes beside a summary: the facts of the run so
--- far, off the log.
---
---     local ledger = policy.ledger({ track = { "path" }, kinds = { "verify" } })
---     ...
---     session:append({ kind = "summary", data = { content = text, ledger = ledger(session) } })
---
--- A summary is the model's account of its own run, and the facts it is most
--- likely to lose are the ones it got wrong: which file it edited, whether
--- the check passed. Those are in the log, and this reads them back as
--- counts and names — never as sentences, so there is nothing in it to
--- disagree with the record. `policy.window{ from_summary }` renders what is
--- here under the summary, one line per fact, where the model reads the
--- summary (`policy.shapes.ledger` says the shape).
---
--- The whole log, and not the part since the last summary: a file edited
--- before the first compaction is still a file this run edited. It reads
--- nothing a loop has to hand it beyond the session, and holds nothing.
---
--- @param opts table|nil  { track?, kinds? }
--- @return function ledger  fn(session) -> policy.shapes.ledger
function M.ledger(opts?: LedgerOpts): M.LedgerFn
    opts = opts_of(opts, { track = true, kinds = true }, "policy.ledger")
    for _, key in ipairs({ "track", "kinds" }) do
        local list = (opts as {string:any})[key]
        if list ~= nil then
            if type(list) ~= "table" then
                error("policy.ledger: " .. key .. " must be an array of strings", 2)
            end
            for i, item in ipairs(list as {any}) do
                if type(item) ~= "string" or item == "" then
                    error("policy.ledger: " .. key .. "[" .. i .. "] must be a non-empty string", 2)
                end
            end
        end
    end
    shape.assert_dev(opts, LEDGER_OPTS, "policy.ledger opts")

    local track = opts.track or DEFAULT_LEDGER_TRACK
    local kinds = opts.kinds or DEFAULT_LEDGER_KINDS
    return function(session: kernel.Session): shared.Ledger
        needs_session(session, "policy.ledger")
        return ledger_of(whole_log(session, "policy.ledger"), track, kinds)
    end
end

--- The contracts this file publishes; `policy.shapes` gathers them.
M.shapes = {
    compact_opts = COMPACT_OPTS,
    ledger_opts = LEDGER_OPTS,
    compact_skipped = shared.COMPACT_SKIPPED,
}

--- This file's entries in `policy.shapes.api`, gathered there.
M.api = {
    compact = {
        args = { arg_of(COMPACT_ARG, "opts") },
        returns = 'due — fn(session) -> nil | "dropped" | "share"; and fold — fn(events, device) -> request, report, for the summarising beat',
        members = {
            due = {
                args = { SESSION_ARG },
                returns = 'nil | "dropped" | "share"',
            },
            fold = {
                args = { EVENTS_ARG, arg_of(T.table, "device (read for system / tools)") },
                returns = "knl.shapes.request, knl.shapes.window_report",
            },
        },
    },
    ledger = {
        args = { arg_of(LEDGER_ARG, "opts") },
        returns = "ledger — fn(session) -> policy.shapes.ledger",
        members = {
            ledger = {
                args = { SESSION_ARG },
                returns = LEDGER,
            },
        },
    },
}

return M
