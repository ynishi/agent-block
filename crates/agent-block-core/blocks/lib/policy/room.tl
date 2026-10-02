--- policy.room — the window as one value, and the policies that read it.
---
--- THE ROOM is a value (`policy.room{ port, conf, reserve?, result_share?,
--- beat_share?, call_reserve? }`): the Port's profile read once, and the one
--- division of the window every policy here sizes against — where the prompt
--- ends and the reply begins (`limit` / `held`, `reply(used)`), what one tool
--- result may take (`result_limit`), what one beat's results may take
--- together (`beat_budget`), where the reasoning has to stop
--- (`thinking_stop(used)`), and the Port's own count behind all of them. It
--- is frozen at construction like a device, holds no state, and the division
--- is checked where it is declared: a share that cannot fit is refused there,
--- not found at the beat it breaks.
---
--- Six of the fifteen read it: `window` is the fold that keeps as many beats
--- as fit (and, from a `summary` on, the summary in place of the beats it
--- stands for), `tokens` the cost that charges a beat its request,
--- `result_cap` the wrap that keeps one tool result under `result_limit`,
--- `beat_cap` the wrap that keeps one beat's results under `beat_budget`,
--- `thinking_cap` the filter that tells the model where its reasoning has to
--- stop for the call after it to fit, and `room_note` the filter that tells
--- the model how much of the window its request has taken. None of them
--- holds a number about the window of its own — the Port declares it, the
--- room divides it, and the six read. Each still takes the older
--- `{ port, conf, <share> }` form and builds a room from it; that form is on
--- its way out.
---
--- Exported through `policy`; see that header for the rules every policy
--- keeps. This file is `require`d by `policy/init.lua` and not meant to be
--- reached for directly.

require("host_types")

local kernel = require("knl")
local lshape = require("lshape")
local shared = require("policy.shared")
local T = lshape.t
local shape = lshape.check

--- What `split` answers (`policy.shapes.split`); `room` only with `used`.
local record Split ---@struct
    window: number
    max_output: number
    limit: number
    held: number
    room: number ---@optional
end
--- What `std.fs.tool_specs{ limits }` takes: what one result may cost, and
--- how to count it (`room:limits_for_tools()`).
local record ToolLimits ---@struct
    result_tokens: integer
    count: function(string): number
end

local record M
    --- A fold: the request, and what the window left out.
    type Fold = function({shared.Event}, kernel.Device): kernel.Request, kernel.WindowReport
    --- The same question asked before the beat: `"context"` with the
    --- tokens and the limit, or nil.
    type Fits = function(kernel.Session, kernel.Device): string, number, number
    type Cost = function(kernel.Request): integer
    type ToolsWrap = function({string:kernel.ToolEntry}): {string:kernel.ToolEntry}
    type Bind = function(kernel.Session): ToolsWrap
    type Filter = function(kernel.Request): kernel.Request

    --- The window as one value: the Port's profile read once, and the
    --- division of it every policy here reads. Frozen; built by `room`.
    record Room
        --- The Port and the conf the numbers were read with.
        port: shared.Port
        conf: {string:any}
        --- `profile.context_window`, and the cap the wire carries (0 for none).
        window: integer
        max_output: integer
        --- The prompt's side: the most a request may cost, and what
        --- `reserve` held back beyond `max_output` to leave the reply room.
        limit: integer
        held: integer
        reserve: integer
        --- What one tool result may take, and what one beat's results may
        --- take together — shares of `limit`, and the numbers they come to.
        result_share: number
        result_limit: integer
        beat_share: number
        beat_budget: integer
        --- Tokens kept out of the reasoning for the call after it; nil when
        --- the room was built without one (then `thinking_stop` refuses).
        call_reserve: integer
        --- The division as a value (`policy.shapes.split`); `room` on it
        --- only with `used`.
        split: function(Room, ?number): Split
        --- The reply's side for a request costing `used`: the window less
        --- it, and no more than the wire's cap. May be zero or negative.
        reply: function(Room, number): number
        --- Where the reasoning has to stop for a request costing `used`:
        --- `reply(used) - call_reserve`, under `budget` when one is given,
        --- or nil when that is under one token.
        thinking_stop: function(Room, number, ?integer): integer
        --- The Port's count of a request, and of one piece of text as the
        --- kernel would render it in a request.
        count: function(Room, kernel.Request): number
        count_text: function(Room, string): number
        --- `result_limit` and `count_text`, in the form `std.fs.tool_specs{
        --- limits }` takes, so a tool can fit its own answer to the room.
        limits_for_tools: function(Room): ToolLimits
    end
    --- What `room` is built from.
    record RoomOpts
        port: shared.Port
        conf: {string:any}
        reserve: integer
        result_share: number
        beat_share: number
        call_reserve: integer
    end
    --- What `window{ fit }` is: the Port that counts and answers a profile,
    --- the conf it is opened with, and the tokens held back for the reply.
    record Fit
        port: shared.Port
        conf: any
        reserve: number
    end
    record WindowOpts
        room: Room
        tail: number
        keep_seed: boolean
        fit: Fit
        from_summary: boolean
    end
    record RoomNoteOpts
        room: Room
        port: shared.Port
        conf: any
    end
    record SplitOpts
        port: shared.Port
        conf: any
        reserve: number
        used: number
    end
    record TokensOpts
        room: Room
        port: shared.Port
        conf: any
    end
    record ResultCapOpts
        room: Room
        port: shared.Port
        conf: any
        share: number
    end
    record BeatCapOpts
        room: Room
    end
    record ThinkingCapOpts
        room: Room
        port: shared.Port
        conf: {string:any}
        call_reserve: number
        budget: integer
    end

    type RoomFactory = function(RoomOpts): Room
    type IsRoom = function(any): boolean
    type WindowFactory = function(WindowOpts): Fold, Fits
    type SplitFn = function(SplitOpts): Split
    type TokensFactory = function(TokensOpts): Cost
    type ResultCapFactory = function(ResultCapOpts): ToolsWrap
    type BeatCapFactory = function(BeatCapOpts): Bind
    type ThinkingCapFactory = function(ThinkingCapOpts): Filter
    type RoomNoteFactory = function(RoomNoteOpts): Filter
    shapes: {string:lshape.Schema}
    api: {string:shared.ApiEntry}
end

-- What this file shares with its siblings, by the names the code was
-- written against (`policy.shared`).
local DEFAULT_RESULT_SHARE = shared.DEFAULT_RESULT_SHARE
local DEFAULT_BEAT_SHARE = shared.DEFAULT_BEAT_SHARE
local ARRAY_TAG = shared.ARRAY_TAG
local whole_at_least = shared.whole_at_least
local opts_of = shared.opts_of
local whole_log = shared.whole_log
local beat_of = shared.beat_of
local data_of = shared.data_of
local needs_session = shared.needs_session
local opts_contract = shared.opts_contract
local arg_of = shared.arg_of
local EVENTS_ARG = shared.EVENTS_ARG
local SESSION_ARG = shared.SESSION_ARG
local SUMMARY_KIND = shared.SUMMARY_KIND
local render_summary = shared.render_summary

--- The room, as an opt: a value `policy.room` built. Checked by tag
--- (`is_room`) in both modes; the shape says only that it is a table.
local ROOM_ARG_SHAPE = T.table:describe("a room, from policy.room{ port, conf, ... }"):is_optional()

--- What `policy.room` is built from.
local ROOM_OPTS, ROOM_ARG = opts_contract({
    port = T.table:describe("an LLM Port: profile(conf) for the window and the cap, count(request, conf) for the request"),
    conf = T.table:describe("the conf the port is opened with"):is_optional(),
    reserve = T.number
    :describe("tokens held back for the reply when the wire sends no cap; a whole number >= 0; default 0")
    :is_optional(),
    result_share = T.number
    :describe("the most of `limit` one tool result may take, 0 < share <= 1; default 0.25")
    :is_optional(),
    beat_share = T.number
    :describe("the most of `limit` one beat's tool results may take together, result_share <= share <= 1; default 0.5")
    :is_optional(),
    call_reserve = T.number
    :describe("tokens kept out of the reasoning for the tool call after it; a whole number >= 0; absent: no stop point")
    :is_optional(),
})

--- What `policy.beat_cap` is configured with.
local BEAT_CAP_OPTS, BEAT_CAP_ARG = opts_contract({
    room = T.table:describe("a room, from policy.room{ port, conf, beat_share, ... }"),
})

--- What `policy.window` is configured with.
local WINDOW_OPTS, WINDOW_ARG = opts_contract({
    room = ROOM_ARG_SHAPE,
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
    from_summary = T.boolean
    :describe(
        "fold from the latest `summary` event on: the seed, the summary as one user message, and the beats "
            .. "after it; needs keep_seed. default false"
    )
    :is_optional(),
})

--- What `policy.room_note` is configured with: the Port whose count and
--- profile the numbers come from.
local ROOM_NOTE_OPTS, ROOM_NOTE_ARG = opts_contract({
    room = ROOM_ARG_SHAPE,
    port = T.table
    :describe("an LLM Port: profile(conf) for the window and the cap, count(request, conf) for the request")
    :is_optional(),
    conf = T.table:describe("the conf the port is opened with"):is_optional(),
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
    room = ROOM_ARG_SHAPE,
    port = T.table:describe("an LLM Port: anything answering count(request, conf) -> integer"):is_optional(),
    conf = T.table:describe("the conf the port is opened with; forwarded to count"):is_optional(),
})

--- What `policy.result_cap` is configured with. The share is of the window
--- the Port declares, never a count of bytes: what is large depends on the
--- model, and the model is what the Port knows.
local RESULT_CAP_OPTS, RESULT_CAP_ARG = opts_contract({
    room = ROOM_ARG_SHAPE,
    port = T.table
    :describe("an LLM Port: profile(conf) for the window, count(request, conf) for the size")
    :is_optional(),
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
    room = ROOM_ARG_SHAPE,
    port = T.table
    :describe("an LLM Port: profile(conf) for the window and cap, count(request, conf) for the request")
    :is_optional(),
    conf = T.table
    :describe(
        "the conf the port is opened with; its `thinking` has to turn reasoning on (true, or a table whose "
            .. "enabled is not false)"
    )
    :is_optional(),
    call_reserve = T.number
    :describe("tokens kept out of the reasoning for the tool call that follows it; with `room`, the room's")
    :is_optional(),
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
local function window_slice(events: {shared.Event}, tail: number, keep_seed?: boolean): {shared.Event}, kernel.WindowReport
    events = events or {}
    local order, first_at: {string}, {string:integer} = {}, {}
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
        local none = setmetatable({} as any, ARRAY_TAG as metatable<any>) as {string}
        return events, { dropped = none, kept = #order, seed_kept = true }
    end
    local cut = math.floor(#order - tail)
    local dropped = setmetatable({} as any, ARRAY_TAG as metatable<any>) as {string}
    for i = 1, cut do
        dropped[i] = order[i]
    end
    local from = first_at[order[cut + 1]]
    local slice: {shared.Event} = {}
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

--- `events` from the latest `summary` on: the seed (everything before the
--- first beat), the summary rendered as one user message in the place the
--- beats it stands for had, and every event after it. A log with no summary
--- is handed back as it is.
---
--- The summary's position in the log is the cut, and nothing in its data
--- says what it covers: everything before it is what it stands for, the
--- summarising beat included — that beat's request and answer are in the
--- log under their own id, and in the request they are what the summary
--- is. An older summary is before the latest one and goes with the rest.
---
--- The rendered message carries no `meta.beat`, so to `window_slice` it is
--- part of the seed and stays ahead of the window under `keep_seed`, and
--- the beats after it are counted and dropped as beats always were. The
--- request then says something the log does not — a conversation that
--- begins with a summary of itself — which is the whole point, and it is
--- said in the message's first line (`shared.SUMMARY_PREFIX`).
---
--- @param events table  a session's events, in seq order
--- @return table  the events to fold, in seq order
local function from_latest_summary(events: {shared.Event}): {shared.Event}
    local at: integer = nil
    for i = #events, 1, -1 do
        local ev = events[i]
        if ev ~= nil and ev.kind == SUMMARY_KIND then
            at = i
            break
        end
    end
    if at == nil then
        return events
    end
    local out: {shared.Event} = {}
    for i = 1, at - 1 do
        if beat_of(events[i]) ~= nil then
            break
        end
        out[#out + 1] = events[i]
    end
    out[#out + 1] = { kind = "msg_user", data = { content = render_summary(data_of(events[at])) } }
    for i = at + 1, #events do
        out[#out + 1] = events[i]
    end
    return out
end

--- How many beats `events` holds.
local function beat_count(events: {shared.Event}): integer
    local seen, n: {string:boolean}, integer = {}, 0
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
local function port_for_fit(fit: M.Fit, who: string): shared.Port, any, number
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
local function profile_split(profile: shared.Profile, who: string, level: integer): integer, integer
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
            string.format(
                "%s: profile.max_output (%d) leaves no room in context_window (%d)",
                who,
                math.floor(output),
                math.floor(window)
            ),
            level
        )
    end
    -- Whole numbers, checked above; said so to the checker.
    return math.floor(window), math.floor(output)
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
local function request_limit(profile: shared.Profile, who: string, reserve?: number): number, number
    local window, output = profile_split(profile, who, 4)
    local held = math.max(0, (reserve or 0) - output)
    local limit = window - output - held
    if limit < 1 then
        error(
            string.format(
                "%s: fit.reserve (%d) beside profile.max_output (%d) leaves no room in context_window (%d)",
                who,
                math.floor(reserve or 0),
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
local function reply_room(profile: shared.Profile, who: string, used: number, level: integer): number
    local window, output = profile_split(profile, who, level)
    local room = window - used
    if output > 0 and output < room then
        room = output
    end
    return room
end

-- ============================================================
-- room — the window as one value
-- ============================================================

--- The tag a room's metatable carries, so `is_room` answers by identity and
--- not by the shape of a table someone else built.
local ROOM_TAG = "policy.room"

--- Whether `v` is a room this file built.
function M.is_room(v: any): boolean
    return type(v) == "table" and (getmetatable(v) as any) == ROOM_TAG
end

--- A Port's count, held to being a number.
local function count_with(port: shared.Port, conf: any, request: any, who: string): number
    local n = port:count(request, conf)
    if type(n) ~= "number" then
        error(who .. ": port:count must answer a number, got " .. tostring(n), 0)
    end
    return n
end

--- Build the room: the Port's profile read once, and the division of the
--- window every policy in this file reads.
---
---     local room = policy.room({ port = port, conf = conf, reserve = 6144,
---                                result_share = 0.3, beat_share = 0.5, call_reserve = 3072 })
---     room.limit            -- the most a request may cost
---     room.result_limit     -- the most one tool result may take
---     room.beat_budget      -- the most one beat's results may take together
---     room:reply(used)      -- the reply's side once the request is known
---     room:split(used)      -- all of it as a value (policy.shapes.split)
---     room:limits_for_tools()  -- for std.fs.tool_specs{ limits }
---
--- WHY A VALUE. Five policies size against the window — the fold, the cost,
--- the two tool caps, the reasoning's stop point — and the window line
--- reads it; before this each took `{ port, conf, <its share> }` and derived
--- its own number, which left the division of the window stated nowhere and
--- checked nowhere. A single beat whose parallel reads were each under the
--- result share and together over the window was the shape that found the
--- gap [measured 2026-10-01: three reads of ~1,480 tokens each, each under a
--- share of 0.3 of a 5,952 limit, 6,534 together with the seed]. The room
--- is where the division is written once and refused once: a share that
--- cannot hold is an error at construction, in the caller's frame, before
--- any beat.
---
--- WHAT IT READS, AND WHEN. The profile is read here, once, so a room is a
--- value of one window: a Port whose window is learned late (a served model
--- asked for it) is asked before the room is built, which is where the
--- consumers already ask it. The division is `request_limit` / `reply_room`
--- — the same arithmetic `policy.split` has always answered — so a reading
--- taken off the room and a request sized by it agree by construction.
---
--- `beat_share` is at least `result_share`: one result under its own limit
--- always fits an empty beat, so the first result of a beat is never refused
--- by the beat cap alone. `call_reserve` is optional; a room built without
--- it refuses `thinking_stop`, since a stop point with nothing kept back for
--- the call is the one this policy exists to prevent.
---
--- Frozen, like a device: a field write raises, and a different division is
--- a different room. It holds the Port and the conf it was built with, and
--- nothing it learned from a beat.
---
--- @param opts table  { port, conf?, reserve?, result_share?, beat_share?, call_reserve? }
--- @return table  the room
function M.room(opts: M.RoomOpts): M.Room
    opts = opts_of(
        opts,
        { port = true, conf = true, reserve = true, result_share = true, beat_share = true, call_reserve = true },
        "policy.room"
    )
    if type(opts.port) ~= "table" or type(opts.port.count) ~= "function" or type(opts.port.profile) ~= "function" then
        error("policy.room: port must answer count(request, conf) and profile(conf)", 2)
    end
    if opts.conf ~= nil and type(opts.conf) ~= "table" then
        error("policy.room: conf must be a table when given", 2)
    end
    if opts.reserve ~= nil and not whole_at_least(opts.reserve, 0) then
        error("policy.room: reserve must be a whole number >= 0 (tokens), got " .. tostring(opts.reserve), 2)
    end
    for _, key in ipairs({ "result_share", "beat_share" }) do
        local share = (opts as {string:any})[key]
        if share ~= nil and (type(share) ~= "number" or (share as number) <= 0 or (share as number) > 1) then
            error("policy.room: " .. key .. " must be a number in (0, 1], got " .. tostring(share), 2)
        end
    end
    if opts.call_reserve ~= nil and not whole_at_least(opts.call_reserve, 0) then
        error(
            "policy.room: call_reserve must be a whole number >= 0 (tokens kept out of the reasoning), got "
                .. tostring(opts.call_reserve),
            2
        )
    end
    shape.assert_dev(opts, ROOM_OPTS, "policy.room opts")

    local port, conf = opts.port, opts.conf
    local result_share = opts.result_share or DEFAULT_RESULT_SHARE
    local beat_share = opts.beat_share or DEFAULT_BEAT_SHARE
    if beat_share < result_share then
        error(
            string.format(
                "policy.room: beat_share (%s) is under result_share (%s) — one result under its own limit would "
                    .. "not fit an empty beat",
                tostring(beat_share),
                tostring(result_share)
            ),
            2
        )
    end
    local profile = port:profile(conf)
    local window, output = profile_split(profile, "policy.room", 2)
    local limit, held = request_limit(profile, "policy.room", opts.reserve)

    local fields: {string:any} = {
        port = port,
        conf = conf,
        window = window,
        max_output = output,
        limit = math.floor(limit),
        held = math.floor(held),
        reserve = math.floor(opts.reserve or 0),
        result_share = result_share,
        result_limit = math.floor(limit * result_share),
        beat_share = beat_share,
        beat_budget = math.floor(limit * beat_share),
        call_reserve = opts.call_reserve,
    }
    local methods: {string:any} = {}
    methods.split = function(_: M.Room, used?: number): Split
        if used ~= nil and not whole_at_least(used, 0) then
            error("room:split: used must be a whole number >= 0 (tokens), got " .. tostring(used), 2)
        end
        local out: Split = {
            window = fields.window as integer,
            max_output = fields.max_output as integer,
            limit = fields.limit as integer,
            held = fields.held as integer,
        }
        if used ~= nil then
            out.room = reply_room(profile, "room:split", used, 2)
        end
        return out
    end
    methods.reply = function(_: M.Room, used: number): number
        return reply_room(profile, "room:reply", used, 2)
    end
    methods.thinking_stop = function(self: M.Room, used: number, budget?: integer): integer
        if fields.call_reserve == nil then
            error("room:thinking_stop: this room was built without call_reserve", 2)
        end
        local stop = math.floor(self:reply(used) - (fields.call_reserve as number))
        if budget ~= nil and budget < stop then
            stop = budget
        end
        if stop < 1 then
            return nil
        end
        return stop
    end
    methods.count = function(_: M.Room, request: kernel.Request): number
        return count_with(port, conf, request, "room:count")
    end
    methods.count_text = function(_: M.Room, text: string): number
        return count_with(port, conf, { messages = { { role = "user", content = text } } }, "room:count_text")
    end
    methods.limits_for_tools = function(self: M.Room): ToolLimits
        return {
            result_tokens = fields.result_limit as integer,
            count = function(text: string): number
                return self:count_text(text)
            end,
        }
    end

    local mt: {string:any} = {
        __index = function(_: any, k: any): any
            local method = methods[k as string]
            if method ~= nil then
                return method
            end
            return fields[k as string]
        end,
        __newindex = function(_: any, k: any)
            error("a room is frozen: build another with policy.room{ ... " .. tostring(k) .. " = ... }", 2)
        end,
        __pairs = function(): (any, any, any)
            return next, fields, nil
        end,
        __metatable = ROOM_TAG,
    }
    return setmetatable({} as M.Room, mt as metatable<M.Room>)
end

--- The room a policy was handed, or the one its older opts describe.
---
--- Each policy in this file takes `room` or the older `{ port, conf, ...}`
--- and not both; `build` is the policy's own reading of the old form, run
--- lazily so a Port whose window is learned late is asked at the first
--- fold, as before, and not at construction.
---
--- @param opts table  the policy's opts, with `room` or the old keys
--- @param who string  the policy's name, for the raise
--- @param old table  the old keys, `name -> true`, to refuse beside `room`
--- @param build function  fn() -> room, from the old keys
--- @return function  fn() -> room
local function room_of(opts: {string:any}, who: string, old: {string:boolean}, build: function(): M.Room): function(): M.Room
    if opts.room ~= nil then
        if not M.is_room(opts.room) then
            error(who .. ": room must be a value from policy.room{ ... }", 3)
        end
        for key in pairs(old) do
            if opts[key] ~= nil then
                error(who .. ": `" .. key .. "` is the room's; pass `room` or the older form, not both", 3)
            end
        end
        local given = opts.room as M.Room
        return function(): M.Room
            return given
        end
    end
    local built: M.Room = nil
    return function(): M.Room
        if built == nil then
            built = build()
        end
        return built
    end
end

--- The split, as a value: where the prompt ends and the reply begins, for
--- the Port and conf named, read out rather than acted on.
---
---     policy.split({ port = port, conf = conf, reserve = 6144 })
---     -- { window = 32768, max_output = 0, limit = 26624, held = 6144 }
---     policy.split({ port = port, conf = conf, reserve = 6144, used = 24984 })
---     -- { ..., room = 7784 }
---
--- `room:split(used)` for a room built on the spot — the same division the
--- six policies read, handed back as a value so a caller can see the numbers
--- a run is being sized by: to log them beside a beat, to check a conf
--- before opening a session, or to size something of its own against the
--- same split rather than a second guess at it. It is not a policy: it plugs
--- into no seam, and nothing here changes because it was called.
---
--- `room` is on the answer only when `used` was given: without a request
--- there is no reply's side to read, and a zero there would be a number
--- about nothing. The raises are the profile's — no window, a cap that
--- leaves no room — and they point at the caller.
---
--- @param opts table  { port, conf?, reserve?, used? }
--- @return table  policy.shapes.split
function M.split(opts: M.SplitOpts): Split
    opts = opts_of(opts, { port = true, conf = true, reserve = true, used = true }, "policy.split")
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

    -- A split is read off a room; a Port that answers no count is enough
    -- for one, so the room is built over a count that is never asked.
    local port = opts.port
    local reading: shared.Port = {
        profile = function(_: shared.Port, conf: any): shared.Profile
            return port:profile(conf)
        end,
        count = function(_: shared.Port, request: any, conf: any): number
            if type(port.count) == "function" then
                return port:count(request, conf)
            end
            error("policy.split: this port answers no count", 2)
        end,
    }
    local ok, room_or_err =
        pcall(M.room, { port = reading, conf = opts.conf as {string:any}, reserve = opts.reserve as integer })
    if not ok then
        -- The room's raise names itself; a split reads as a split.
        error((tostring(room_or_err):gsub("^.-policy%.room: ", "policy.split: ")), 2)
    end
    return (room_or_err as M.Room):split(opts.used)
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
--- @param events table  the log, as the fold was handed it
--- @param device table  the device, read for system / tools
--- @param room table  the room: the limit the request is held to, and the count
--- @param tail number|nil  the most beats a window may keep, when capped
--- @param keep_seed boolean  whether the events before the first beat stay
--- @return table|nil request  the fitted request, or nil when none fits
--- @return table|nil report  what it dropped to get there (with a request)
--- @return number|nil tokens  what the smallest candidate cost (with nil)
--- @return number|nil limit  the room the profile left (with nil)
--- @return string|nil counted  "beat" | "seed" — what those tokens are of
local function largest_fitting(
    events: {shared.Event},
    device: kernel.Device,
    room: M.Room,
    tail: number,
    keep_seed: boolean
): kernel.Request, kernel.WindowReport, number, number, string
    -- The room a request may take, and what `reserve` held back to leave
    -- it: the room's, read once where it was built.
    local limit, held = room.limit, room.held
    local n = beat_count(events)
    local most = tail and math.min(tail, n) or n

    local function fold_at(k: number): kernel.Request, number, kernel.WindowReport
        local slice, report = window_slice(events, k, keep_seed)
        local request = kernel.fold(slice, device)
        return request, room:count(request), report
    end

    --- The chosen candidate's report, with the counting written onto it.
    --- `limit` is the effective one — the number this candidate was
    --- measured against — and `reserve` is how much of the window is not
    --- in it, so a reader of the log can tell a short window from one cut
    --- short to leave the reply room.
    local function counted(report: kernel.WindowReport, before: number, after: number): kernel.WindowReport
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
    local lo, hi: number, number = floor, most
    local best, best_tokens, best_report: kernel.Request, number, kernel.WindowReport = nil, nil, nil
    local smallest_tokens: number = nil
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
--- From a summary on
---   With `from_summary = true` the fold reads the log from the latest
---   `summary` event (`policy.shapes.summary`, the one a loop appends after
---   a `policy.compact` beat): the request is the seed, the summary rendered
---   as one user message, and the beats after it, windowed as above. The
---   summary's place in the log is the cut — everything before it is what it
---   stands for — and the events it stands for stay in the log untouched;
---   this changes what is SENT, as every window does, and nothing about what
---   is recorded. It needs `keep_seed`, because the summary is kept the way
---   the seed is: ahead of the window, and never dropped.
---
---       policy.window({ fit = { port = port, conf = conf, reserve = 4096 },
---                       keep_seed = true, from_summary = true })
---
---   Between compactions the request's head — seed and summary — does not
---   move, which is also what keeps a provider's prompt cache warm across
---   the beats between them; a window that only dropped beats moved its
---   head on every beat once it was full.
---
--- With a room
---   `room = policy.room{ ... }` stands for `fit`: the window is sized by the
---   room's `limit` and counted by its Port, the same division every other
---   policy on the device reads. `fit` is the older form and builds a room
---   of its own at the first fold; one or the other, not both.
---
---       policy.window({ room = room, keep_seed = true, from_summary = true })
---
--- @param opts table  { room = <policy.room>? | fit = { port, conf?, reserve = <whole number >= 0>? }?, tail = <whole number >= 1>?, keep_seed = <boolean>?, from_summary = <boolean>? } — `tail` is required without `room` / `fit`
--- @return function fold  fn(events, device) -> request, report
--- @return function|nil fits  fn(session, device) -> nil | "context", tokens, limit (with `room` / `fit` only)
function M.window(opts: M.WindowOpts): M.Fold, M.Fits
    opts = opts_of(opts, { room = true, tail = true, keep_seed = true, fit = true, from_summary = true }, "policy.window")
    if opts.room ~= nil and opts.fit ~= nil then
        error("policy.window: pass `room` or `fit`, not both", 2)
    end
    if opts.fit == nil and opts.room == nil and not whole_at_least(opts.tail, 1) then
        error("policy.window: tail must be a whole number >= 1, got " .. tostring(opts.tail), 2)
    end
    if opts.tail ~= nil and not whole_at_least(opts.tail, 1) then
        error("policy.window: tail must be a whole number >= 1, got " .. tostring(opts.tail), 2)
    end
    if opts.keep_seed ~= nil and type(opts.keep_seed) ~= "boolean" then
        error("policy.window: keep_seed must be a boolean, got " .. tostring(opts.keep_seed), 2)
    end
    if opts.from_summary ~= nil and type(opts.from_summary) ~= "boolean" then
        error("policy.window: from_summary must be a boolean, got " .. tostring(opts.from_summary), 2)
    end
    if opts.from_summary == true and opts.keep_seed ~= true then
        error("policy.window: from_summary needs keep_seed = true — the summary is kept the way the seed is", 2)
    end
    local sized = opts.room ~= nil or opts.fit ~= nil
    local get: function(): M.Room = nil
    if sized then
        local fit = opts.fit
        get = room_of(opts as {string:any}, "policy.window", { fit = true }, function(): M.Room
            local port, conf, reserve = port_for_fit(fit, "policy.window")
            return M.room({ port = port, conf = conf as {string:any}, reserve = reserve as integer })
        end)
        if fit ~= nil then
            -- The old form's own checks, at construction as they always were.
            port_for_fit(fit, "policy.window")
        end
    end
    shape.assert_dev(opts, WINDOW_OPTS, "policy.window opts")

    local tail = opts.tail
    local keep_seed = opts.keep_seed == true
    -- What the fold is handed, as this window reads it: the whole log, or
    -- the log from its latest summary on.
    local read: function({shared.Event}): {shared.Event} = function(events: {shared.Event}): {shared.Event}
        return events or {}
    end
    if opts.from_summary == true then
        read = from_latest_summary
    end
    if not sized then
        return function(events: {shared.Event}, device: kernel.Device): kernel.Request, kernel.WindowReport
            local slice, report = window_slice(read(events), tail, keep_seed)
            -- No `before` / `after` / `limit`: this form counts nothing, and
            -- a number here would be one this fold never asked for.
            return kernel.fold(slice, device), report
        end
    end

    local fold = function(events: {shared.Event}, device: kernel.Device): kernel.Request, kernel.WindowReport
        local request, report, tokens, limit, counted = largest_fitting(read(events), device, get(), tail, keep_seed)
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
    local fits = function(session: kernel.Session, device: kernel.Device): string, number, number
        needs_session(session, "policy.window fits")
        local request, _, tokens, limit =
            largest_fitting(read(whole_log(session, "policy.window fits")), device or {}, get(), tail, keep_seed)
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
function M.tokens(opts: M.TokensOpts): M.Cost
    opts = opts_of(opts, { room = true, port = true, conf = true }, "policy.tokens")
    local count: function(kernel.Request): number
    if opts.room ~= nil then
        if not M.is_room(opts.room) then
            error("policy.tokens: room must be a value from policy.room{ ... }", 2)
        end
        if opts.port ~= nil or opts.conf ~= nil then
            error("policy.tokens: `port` / `conf` are the room's; pass `room` or the older form, not both", 2)
        end
        local room = opts.room
        count = function(request: kernel.Request): number
            return room:count(request)
        end
    else
        -- The older form: a Port that counts is all a cost needs, so no room
        -- (and no profile) is built for it.
        if type(opts.port) ~= "table" or type(opts.port.count) ~= "function" then
            error("policy.tokens: port must be a table answering count(request, conf)", 2)
        end
        if opts.conf ~= nil and type(opts.conf) ~= "table" then
            error("policy.tokens: conf must be a table when given", 2)
        end
        local port, conf = opts.port, opts.conf
        count = function(request: kernel.Request): number
            return port:count(request, conf)
        end
    end
    shape.assert_dev(opts, TOKENS_OPTS, "policy.tokens opts")

    return function(request: kernel.Request): integer
        local n = count(request)
        if not whole_at_least(n, 0) then
            error("policy.tokens: port:count must answer a whole number >= 0, got " .. tostring(n), 2)
        end
        if n < 1 then
            return 1
        end
        return math.floor(n)
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
--- A tool's result as the kernel will render it into the request — a string
--- verbatim, anything else as JSON — because that rendering is what the
--- window pays for, not the table.
local function rendered(result: any): string
    if type(result) == "string" then
        return result as string
    end
    return std.json.encode(result)
end

--- The wrap over a tools map that every cap here builds: each handler
--- replaced by `wrap(name, handler)`, the rest of the entry copied.
local function wrap_tools(
    who: string,
    tools: {string:kernel.ToolEntry},
    wrap: function(string, function(any): any): function(any): any
): {string:kernel.ToolEntry}
    if type(tools) ~= "table" then
        error(who .. ": tools must be the device's map of name -> entry", 2)
    end
    local out: {string:kernel.ToolEntry} = {}
    for name, entry in pairs(tools) do
        if type(entry) ~= "table" or type(entry.handler) ~= "function" then
            error(who .. ": tool '" .. tostring(name) .. "' has no handler", 2)
        end
        local capped: {string:any} = {}
        for key, value in pairs(entry as {string:any}) do
            capped[key] = value
        end
        capped.handler = wrap(name, entry.handler)
        out[name] = capped as kernel.ToolEntry
    end
    return out
end

function M.result_cap(opts: M.ResultCapOpts): M.ToolsWrap
    opts = opts_of(opts, { room = true, port = true, conf = true, share = true }, "policy.result_cap")
    if opts.room == nil then
        if type(opts.port) ~= "table" or type(opts.port.count) ~= "function" or type(opts.port.profile) ~= "function" then
            error("policy.result_cap: port must answer count(request, conf) and profile(conf)", 2)
        end
        if opts.conf ~= nil and type(opts.conf) ~= "table" then
            error("policy.result_cap: conf must be a table when given", 2)
        end
        if opts.share ~= nil and (type(opts.share) ~= "number" or opts.share <= 0 or opts.share > 1) then
            error("policy.result_cap: share must be a number in (0, 1], got " .. tostring(opts.share), 2)
        end
    end
    local port, conf, share = opts.port, opts.conf, opts.share
    local get = room_of(opts as {string:any}, "policy.result_cap", { port = true, conf = true, share = true }, function(): M.Room
        return M.room({ port = port, conf = conf as {string:any}, result_share = share })
    end)
    shape.assert_dev(opts, RESULT_CAP_OPTS, "policy.result_cap opts")

    return function(tools: {string:kernel.ToolEntry}): {string:kernel.ToolEntry}
        return wrap_tools("policy.result_cap", tools, function(name: string, handler: function(any): any): function(any): any
            return function(args: any): any
                local result = handler(args)
                local room = get()
                local text = rendered(result)
                local limit = room.result_limit
                local tokens = room:count_text(text)
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
        end)
    end
end

-- ============================================================
-- beat_cap — one beat's results may not outgrow the window together
-- ============================================================

--- The tokens the results already recorded in the beat now running come to:
--- the beat is the one the latest `tool_call` was written under — the kernel
--- records the call before it runs the handler — and its results are the
--- `tool_result`s carrying the same id.
local function beat_so_far(events: {shared.Event}, room: M.Room): integer
    local current: string = nil
    for i = #events, 1, -1 do
        local ev = events[i]
        if ev ~= nil and ev.kind == "tool_call" then
            current = beat_of(ev)
            break
        end
    end
    if current == nil then
        return 0
    end
    local used: number = 0
    for _, ev in ipairs(events) do
        if ev.kind == "tool_result" and beat_of(ev) == current then
            used = used + room:count_text(rendered(data_of(ev).result))
        end
    end
    return math.floor(used)
end

--- Build a BINDER that answers a wrapper over a device's `tools` map,
--- refusing a result once the beat's results together would outgrow the
--- room's `beat_budget`.
---
---     tools = policy.beat_cap({ room = room })(session)(
---         policy.result_cap({ room = room })(knl_adapter.tools({ ... }))
---     )
---
--- WHY A SECOND CAP. `result_cap` holds ONE result under `result_limit`, and
--- a model may ask for several in one response: three reads, each under the
--- limit, together over the window the next request has — and the newest
--- beat is the one the fold cannot drop [measured 2026-10-01: three reads of
--- ~1,480 tokens each under a 1,785 limit; 6,534 with the seed against a
--- limit of 5,952; the run stopped with `context` before its second beat].
--- This is the cap on the sum. It reads the beat's results so far off the
--- log — the kernel writes each pair as it runs, in response order, so the
--- calls after the one that crossed the line are the ones refused — and
--- holds nothing between calls, which is what lets it be a binder like
--- `repeat_cap` rather than a counter.
---
--- A refused call is answered, not raised, in the shape `result_cap` uses:
--- `{ ok = false, reason = "beat_budget", tokens, used, limit, error }`,
--- naming what this result cost, what the beat had already taken, and what a
--- beat may take, so the model asks again on its next turn rather than
--- being told nothing. The first result of a beat is never refused by this
--- cap alone: `room` holds `beat_share >= result_share`.
---
--- Order it OUTSIDE `result_cap`: one result's own limit is judged first, and
--- the sum is judged over results the first cap let through.
---
--- @param opts table  { room }
--- @return function bind  fn(session) -> fn(tools) -> tools
function M.beat_cap(opts: M.BeatCapOpts): M.Bind
    opts = opts_of(opts, { room = true }, "policy.beat_cap")
    if not M.is_room(opts.room) then
        error("policy.beat_cap: room must be a value from policy.room{ ... }", 2)
    end
    shape.assert_dev(opts, BEAT_CAP_OPTS, "policy.beat_cap opts")
    local room = opts.room

    return function(session: kernel.Session): M.ToolsWrap
        needs_session(session, "policy.beat_cap bind")
        return function(tools: {string:kernel.ToolEntry}): {string:kernel.ToolEntry}
            return wrap_tools("policy.beat_cap", tools, function(name: string, handler: function(any): any): function(any): any
                return function(args: any): any
                    local result = handler(args)
                    local tokens = room:count_text(rendered(result))
                    local used = beat_so_far(whole_log(session, "policy.beat_cap"), room)
                    local limit = room.beat_budget
                    if used + tokens <= limit then
                        return result
                    end
                    return {
                        ok = false,
                        reason = "beat_budget",
                        tokens = math.floor(tokens),
                        used = used,
                        limit = limit,
                        error = string.format(
                            "'%s' answered %d tokens, and the results of this turn already take %d of the %d one "
                                .. "turn may take together — the whole conversation has to fit the model's window. "
                                .. "Ask for this on your next turn, after the results you have.",
                            tostring(name),
                            math.floor(tokens),
                            used,
                            limit
                        ),
                    }
                end
            end)
        end
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
function M.thinking_cap(opts: M.ThinkingCapOpts): M.Filter
    opts = opts_of(opts, { room = true, port = true, conf = true, call_reserve = true, budget = true }, "policy.thinking_cap")
    -- With a room: the conf, the Port and `call_reserve` are the room's, and
    -- a room built without a `call_reserve` is refused here, at construction.
    local conf: {string:any}
    if opts.room ~= nil then
        if not M.is_room(opts.room) then
            error("policy.thinking_cap: room must be a value from policy.room{ ... }", 2)
        end
        if opts.port ~= nil or opts.conf ~= nil or opts.call_reserve ~= nil then
            error(
                "policy.thinking_cap: `port` / `conf` / `call_reserve` are the room's; pass `room` or the older form, not both",
                2
            )
        end
        if opts.room.call_reserve == nil then
            error("policy.thinking_cap: the room was built without call_reserve (policy.room{ call_reserve = ... })", 2)
        end
        conf = opts.room.conf or {}
    else
        if type(opts.port) ~= "table" or type(opts.port.count) ~= "function" or type(opts.port.profile) ~= "function" then
            error("policy.thinking_cap: port must answer count(request, conf) and profile(conf)", 2)
        end
        if type(opts.conf) ~= "table" then
            error("policy.thinking_cap: conf must be the table the port is opened with", 2)
        end
        -- Required, and the one opt that is: a cap with nothing kept back for
        -- the call is a cap that lets the reasoning run to the end of the
        -- window, which is what happens without this policy at all.
        if not whole_at_least(opts.call_reserve, 0) then
            error(
                "policy.thinking_cap: call_reserve must be a whole number >= 0 (tokens kept out of the reasoning "
                    .. "for the call that follows it), got "
                    .. tostring(opts.call_reserve),
                2
            )
        end
        conf = opts.conf
    end
    -- The conf has to have turned reasoning on itself. The adapter reads a
    -- thinking table as "on" unless it says `enabled = false`, and the
    -- request's table replaces the conf's — so a budget sent over a conf that
    -- said nothing would be the thing that turned reasoning on.
    local declared = conf.thinking
    local on = declared == true or (type(declared) == "table" and (declared as {string:any}).enabled ~= false)
    if not on then
        error(
            "policy.thinking_cap: the conf does not turn reasoning on (thinking = true, or { enabled = true, ... }); "
                .. "a stop point is sent as a thinking table, and one sent over a conf that asked for no reasoning "
                .. "would switch it on by itself",
            2
        )
    end
    if opts.budget ~= nil and not whole_at_least(opts.budget, 1) then
        error("policy.thinking_cap: budget must be a whole number >= 1 (tokens), got " .. tostring(opts.budget), 2)
    end
    local port, call_reserve = opts.port, opts.call_reserve
    local get = room_of(
        opts as {string:any},
        "policy.thinking_cap",
        { port = true, conf = true, call_reserve = true },
        function(): M.Room
        return M.room({ port = port, conf = conf, call_reserve = call_reserve as integer })
        end
    )
    shape.assert_dev(opts, THINKING_CAP_OPTS, "policy.thinking_cap opts")

    local budget = opts.budget

    return function(request: kernel.Request): kernel.Request
        local room = get()
        -- The reply's room, off the profile the fold split the window by: the
        -- window less this request, under the wire's cap where there is one —
        -- less what the call after the reasoning needs, under the caller's
        -- own ceiling. Nil is "nothing sensible to send": the request goes as
        -- it is and the server's own default decides, which is the same thing
        -- that happens on every beat without this filter.
        local stop = room:thinking_stop(room:count(request), budget as integer)
        if stop == nil then
            return request
        end
        -- The conf's own thinking keys, under the budget: `true` carries as
        -- `enabled`, a table as its keys — the construction above has already
        -- said it is one of those two.
        local thinking: {string:any} = {}
        if type(declared) == "table" then
            for key, value in pairs(declared as {string:any}) do
                thinking[key] = value
            end
        else
            thinking.enabled = true
        end
        thinking.budget_tokens = stop
        local out: {string:any} = {}
        for key, value in pairs(request as {string:any}) do
            out[key] = value
        end
        out.thinking = thinking
        return out as kernel.Request
    end
end

-- ============================================================
-- room_note — what the request took of the window, said to the model
-- ============================================================

--- Build a `filter` that ends every request with one line about the window:
--- what the request took of it, and what is left for the reply.
---
---     filters = { policy.room_note({ port = port, conf = conf }) }
---
--- THE MODEL CANNOT SEE ITS ROOM (the reasoning `thinking_cap` gives), and
--- the one thing it can do about a window that is nearly full — read less,
--- edit now, stop re-reading — it can only do when told. This says it, in
--- the request, as a fact: `[window] 24984 of 32768 tokens used; 7784 left
--- for the reply`. The numbers are the Port's — its count of the request
--- it is handed, and the split `reply_room` reads off its profile — so the
--- line and the fold agree by construction, and nothing here holds a number
--- of its own.
---
--- The note is one more user message at the end of the request, where the
--- `window:` line the sibling lane has sent since 2026-09-14 went; it
--- follows a tool_result batch rather than being written into one, so no
--- `tool_use` / `tool_result` pairing is touched. It is counted AFTER the
--- fold sized the request, so the request goes over the fold's limit by the
--- note's own length — a few dozen tokens, which `fit.reserve` is already
--- sized in whole thousands against; and a filter after this one that counts
--- the request (`thinking_cap`) counts the note with it, which is right,
--- because the note goes on the wire.
---
--- It reads no log and holds nothing between beats. The request is not
--- changed in place: what comes back is a copy with the note appended.
---
--- @param opts table  { port, conf? }
--- @return function filter  fn(request) -> request
function M.room_note(opts: M.RoomNoteOpts): M.Filter
    opts = opts_of(opts, { room = true, port = true, conf = true }, "policy.room_note")
    if opts.room == nil then
        if type(opts.port) ~= "table" or type(opts.port.count) ~= "function" or type(opts.port.profile) ~= "function" then
            error("policy.room_note: port must answer count(request, conf) and profile(conf)", 2)
        end
        if opts.conf ~= nil and type(opts.conf) ~= "table" then
            error("policy.room_note: conf must be a table when given", 2)
        end
    end
    local port, conf = opts.port, opts.conf
    local get = room_of(opts as {string:any}, "policy.room_note", { port = true, conf = true }, function(): M.Room
        return M.room({ port = port, conf = conf as {string:any} })
    end)
    shape.assert_dev(opts, ROOM_NOTE_OPTS, "policy.room_note opts")

    return function(request: kernel.Request): kernel.Request
        local room = get()
        local used = room:count(request)
        local note = string.format(
            "[window] %d of %d tokens used; %d left for the reply",
            math.floor(used),
            room.window,
            math.floor(math.max(0, room:reply(used)))
        )
        local out: {string:any} = {}
        for key, value in pairs(request as {string:any}) do
            out[key] = value
        end
        local messages = setmetatable({} as any, ARRAY_TAG as metatable<any>) as {any}
        for _, message in ipairs(request.messages or {}) do
            messages[#messages + 1] = message
        end
        messages[#messages + 1] = { role = "user", content = note }
        out.messages = messages
        return out as kernel.Request
    end
end

--- The contracts this file publishes; `policy.shapes` gathers them.
M.shapes = {
    room_opts = ROOM_OPTS,
    beat_cap_opts = BEAT_CAP_OPTS,
    window_opts = WINDOW_OPTS,
    thinking_cap_opts = THINKING_CAP_OPTS,
    room_note_opts = ROOM_NOTE_OPTS,
    split_opts = SPLIT_OPTS,
    split = SPLIT,
}

--- This file's entries in `policy.shapes.api`, gathered there.
M.api = {
    room = {
        args = { arg_of(ROOM_ARG, "opts") },
        returns = "room — a frozen value: window, max_output, limit, held, reserve, result_share, result_limit, "
            .. "beat_share, beat_budget, call_reserve?, port, conf; and the readings below",
        members = {
            split = {
                args = { arg_of(T.number, "used (tokens the request costs; optional)") },
                returns = SPLIT,
            },
            reply = {
                args = { arg_of(T.number, "used (tokens the request costs)") },
                returns = "number — the reply's side: window - used, and no more than max_output where there is one",
            },
            thinking_stop = {
                args = {
                    arg_of(T.number, "used (tokens the request costs)"),
                    arg_of(T.number, "budget (the most reasoning the caller allows; optional)"),
                },
                returns = "integer >= 1 — where the reasoning stops, or nil when nothing sensible is left",
            },
            count = {
                args = { arg_of(kernel.shapes.request, "request") },
                returns = "number — the Port's count of the request",
            },
            count_text = {
                args = { arg_of(T.string, "text") },
                returns = "number — the Port's count of the text as one user message",
            },
            limits_for_tools = {
                args = {},
                returns = "{ result_tokens = result_limit, count = fn(text) -> tokens } — std.fs.tool_specs{ limits }",
            },
        },
    },
    is_room = {
        args = { arg_of(T.any, "value") },
        returns = "boolean — whether the value is a room this module built",
    },
    beat_cap = {
        args = { arg_of(BEAT_CAP_ARG, "opts") },
        returns = "bind — fn(session) -> fn(tools) -> tools",
        members = {
            bind = {
                args = { SESSION_ARG },
                returns = "wrap — fn(tools) -> tools",
            },
            wrap = {
                args = { arg_of(T.table, "tools (the device's map of name -> entry)") },
                returns = "table — the same map, each handler held to the beat's budget",
            },
        },
    },
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
    room_note = {
        args = { arg_of(ROOM_NOTE_ARG, "opts") },
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
