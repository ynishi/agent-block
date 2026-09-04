--- knl — the Lua kernel (session + device / beat / fold / Outcome).
---
--- What this is
---   The driving half of the kernel/shell split: Rust is the pure syscall
---   layer (session / append / events / view / reserve / spend / close), and
---   this module runs a beat — one model call plus the tools that call asks
---   for — over it, returning an `Outcome`. See `session-device-design.md`
---   (the two-argument beat) over `knl-kernel-design.md`.
---
--- Two arguments, two owners (session-device-design.md §1/§4)
---   `knl.open{ owner?, budget?, store? }` hands back the kernel's session
---   userdata verbatim — the durable half: the fact-log and the quota, owned
---   by the kernel, advanced only by appending. `knl.device{ llm?, tools?,
---   tool_policy?, fold?, filters?, system?, cost? }` builds the policy half
---   — a stateless value whose defaults are resolved once at construction
---   and then frozen. `knl.beat(session, device)` takes both. They are not
---   bundled into one handle because they differ in owner (kernel / caller),
---   lifetime (durable / per-process) and mutability (append-only / frozen),
---   and the config is consumed at construction rather than carried.
---   Per-beat policy variation is not an override argument: derive another
---   device with `d:with{ llm = strong }` and beat with that one.
---
--- Lifecycle belongs to the session (session-device-design.md §9-e)
---   The canonical bracket is the callback form — the kernel opens, runs the
---   body, and closes, so an error escaping the body still records the
---   boundary:
---
---       local result = knl.session({ owner = "u", budget = { amount = 10 } },
---           function(s) return knl.beat(s, d) end)
---
---   `knl.session` resumes instead of opening when the opts name a session.
---   `local s <close> = knl.open{...}` is the alternative and the Rust Drop
---   backstop is the last resort; a body error always wins over a close that
---   failed on its way out (§9-f).
---
--- Beats are declared, not numbered (session-device-design.md §9-a)
---   The kernel does not count beats. `knl.beat` mints one id per beat with
---   `knl.new_beat_id()` (time-ordered, session-free) and stamps it on every
---   event that beat writes — llm_request, llm_response, the tool pair, and
---   a failed call's note. The kernel stores a `beat` it is given and asks
---   only that it be a string; grouping and ordering read it back, nothing
---   more. `resp.beat` carries the same id out to the caller.
---
--- The budget (budget-design.md §2)
---   The budget is a quota the owner granted the session, not a tally of
---   what it used. beat asks for permission BEFORE it calls —
---   `session:reserve(n)`, after the request is known and before anything is
---   recorded — and a refusal is a planned stop, not a failure and not a
---   model decision: `Outcome.stopped("budget", tag)`, with no `llm_request`
---   event and no call. How much a beat asks for is the device's policy:
---   `device.cost(request)`, one unit per beat by default. What a unit
---   *means* is whatever the owner tagged the grant with — the kernel reads
---   the number and nothing else, and token usage (`view("usage")`) is a
---   separate reading that beat never folds back into the budget.
---
--- What the POC deliberately leaves out
---   Real provider adapters (llm is `fn(req) -> {status, content, usage,
---   stop_reason}`), the full fold vocabulary, realistic tool / filter
---   policies, and fork (branching the history — deferred until the basic
---   shape is wired into real loops).

--- The Rust syscall bridge, captured before `require("knl")` shadows the
--- name (see the header). `nil` in a VM that has no bridge (e.g. the pure
--- lspec runner): `fold` / `Outcome` / `device` never touch it, and the
--- entry points that do report its absence rather than indexing nil.
local syscall = knl

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

local M = {}

--- The bridge function `method` names, resolved at call time.
---
--- The load-time capture above is the primary source; the global is read
--- again as a fallback so a spec that installs a fake bridge after this
--- module loaded still reaches it. Missing either way is a loud error, not
--- an index of nil.
local function bridge(method)
    local b = syscall
    if type(b) ~= "table" or type(b[method]) ~= "function" then
        b = rawget(_G, "knl")
    end
    if type(b) ~= "table" or type(b[method]) ~= "function" then
        error("knl." .. method .. ": the knl syscall bridge is not available in this VM", 3)
    end
    return b[method]
end

-- ============================================================
-- Outcome — the result type of a beat
-- ============================================================
--
-- Plain-data status tag tables, never metatable methods: an Outcome crosses
-- the JSON boundary with the kernel, and a metatable does not survive the
-- round trip. Predicates and the match are free functions for the same
-- reason — the value carries data, the module carries behaviour.

local Outcome = {}

--- The status values the kernel knows. Exactly these four, provider-neutral.
local STATUSES = { "ok", "refused", "error", "stopped" }

--- A beat that ran: `out` is the call's return (content / usage /
--- stop_reason, plus the `beat` id and the tool summary this layer added).
function Outcome.ok(out)
    return { status = "ok", out = out }
end

--- The model produced a response but refused to make progress. `detail`
--- carries the call's `out` so a caller can inspect what came back.
function Outcome.refused(reason, detail)
    return { status = "refused", reason = reason, detail = detail }
end

--- The beat did not come off: the mechanism failed. `kind` is one of the
--- kernel's own failure points — "conf", "filter", "call" or "state" (a
--- record that could not be laid down: closed session, CAS head conflict,
--- event validation).
function Outcome.err(kind, detail)
    return { status = "error", kind = kind, detail = detail }
end

--- The beat stopped on purpose before calling: an allowance would not cover
--- it. Not a failure (nothing broke) and not a refusal (the model was never
--- asked) — the branch a caller's loop exits on. `tag` names the grant that
--- stopped it, when the owner gave it one.
function Outcome.stopped(reason, tag)
    return { status = "stopped", reason = reason, tag = tag }
end

function Outcome.is_ok(o)
    return type(o) == "table" and o.status == "ok"
end

function Outcome.is_refused(o)
    return type(o) == "table" and o.status == "refused"
end

function Outcome.is_error(o)
    return type(o) == "table" and o.status == "error"
end

function Outcome.is_stopped(o)
    return type(o) == "table" and o.status == "stopped"
end

--- Match an Outcome against `arms = { ok = fn, refused = fn, error = fn,
--- stopped = fn }`.
---
--- Exhaustive: every one of the four arms must be present, and the actual
--- status must be one the kernel knows. A missing arm is a loud error rather
--- than a silently-dropped case (the dynamic-language stand-in for a
--- compiler's exhaustiveness check).
function Outcome.match(o, arms)
    if type(arms) ~= "table" then
        error("Outcome.match: arms must be a table")
    end
    for _, status in ipairs(STATUSES) do
        if type(arms[status]) ~= "function" then
            error("Outcome.match: missing arm for '" .. status .. "'")
        end
    end
    local arm = arms[o.status]
    if arm == nil then
        error("Outcome.match: unknown status '" .. tostring(o.status) .. "'")
    end
    return arm(o)
end

M.Outcome = Outcome

-- ============================================================
-- fold — events -> request (pure)
-- ============================================================

--- The JSON-array tag the bridge's converter honours (`lua_to_json` reads
--- `__jsontype = "array"`).  Every array fold builds is tagged, so the empty
--- case crosses the boundary — into the durable `llm_request` event and onto
--- the provider wire — as `[]`, not `{}` (the empty-array boundary class
--- this repo has prior fixes for).
local ARRAY_TAG = { __jsontype = "array" }

local function tag_array(t)
    return setmetatable(t, ARRAY_TAG)
end

--- Render a tool_result's payload as request text: a string verbatim, any
--- other value JSON-encoded (the envelope already carries the rest).
local function result_text(result)
    if type(result) == "string" then
        return result
    end
    return std.json.encode(result)
end

--- The wire declarations for `tools` (name -> { description, input_schema,
--- handler }), handler stripped: the request carries what the model may
--- call, not how to call it. Sorted by name so fold stays deterministic
--- (pure), which is what lets a KV cache hold across beats.
local function wire_tools(tools)
    local names = {}
    for name in pairs(tools) do
        names[#names + 1] = name
    end
    table.sort(names)
    local out = tag_array({})
    for _, name in ipairs(names) do
        local spec = tools[name]
        out[#out + 1] = {
            name = name,
            description = spec.description,
            input_schema = spec.input_schema or spec.schema,
        }
    end
    return out
end

--- Fold an event list into a provider-neutral request.
---
--- The default fold, for chat-shaped providers. Pure: it reads `events` and
--- the `device`'s policy fields and writes nothing. Three kinds map to
--- messages and the rest are skipped (tool_call / session_* / budget_* /
--- llm_call_failed / llm_request):
---
---   msg_user     -> { role = "user", content = <verbatim> }
---   llm_response -> { role = "assistant", content = <verbatim> }
---   tool_result  -> collected, in seq order, into the user message that
---                   follows the assistant turn they answer (consecutive
---                   tool_results batch together, which for a well-formed
---                   history is the same as grouping by beat)
---
--- `system` and `tools` are composed from the device each beat, not read
--- from the history.
---
--- @param events table  array of stored events (from `session:events()`)
--- @param device table  a knl device (any table carrying system / tools)
--- @return table request  { system?, messages, tools? }
function M.fold(events, device)
    device = device or {}
    local messages = tag_array({})
    local batch = tag_array({})

    -- The tool_use ids of the most recent assistant message, in block
    -- order, and which of them a recorded tool_result has answered.  A
    -- history can legitimately end (or be interrupted) between the
    -- llm_response append and the tool results — the state a crash
    -- mid-tool leaves behind — and the provider rejects an assistant
    -- message whose tool_use ids have no answering tool_result.  fold
    -- repairs that read-side: any id still unanswered when the next
    -- message begins (or the history ends) is closed with a synthetic
    -- is_error result, so a resumed stream stays foldable instead of
    -- 400-ing forever.
    local pending, answered = {}, {}

    local function close_dangling()
        for _, id in ipairs(pending) do
            if not answered[id] then
                batch[#batch + 1] = {
                    type = "tool_result",
                    tool_use_id = id,
                    content = "tool execution was interrupted before a result was recorded",
                    is_error = true,
                }
            end
        end
        pending, answered = {}, {}
    end

    local function flush()
        if #batch > 0 then
            messages[#messages + 1] = { role = "user", content = batch }
            batch = tag_array({})
        end
    end

    for _, ev in ipairs(events or {}) do
        local kind = ev.kind
        if kind == "msg_user" then
            close_dangling()
            flush()
            messages[#messages + 1] = { role = "user", content = ev.content }
        elseif kind == "llm_response" then
            close_dangling()
            flush()
            messages[#messages + 1] = { role = "assistant", content = ev.content }
            if type(ev.content) == "table" then
                for _, block in ipairs(ev.content) do
                    if block.type == "tool_use" and block.id ~= nil then
                        pending[#pending + 1] = block.id
                    end
                end
            end
        elseif kind == "tool_result" then
            if ev.call_id ~= nil then
                answered[ev.call_id] = true
            end
            batch[#batch + 1] = {
                type = "tool_result",
                tool_use_id = ev.call_id,
                content = result_text(ev.result),
                is_error = (ev.ok == false) or nil,
            }
        end
        -- everything else (tool_call / session_* / budget_* /
        -- llm_call_failed / llm_request) is not part of a request: skip it.
    end
    close_dangling()
    flush()

    local request = { messages = messages }
    if device.system ~= nil then
        request.system = device.system
    end
    if device.tools ~= nil then
        request.tools = wire_tools(device.tools)
    end
    return request
end

-- ============================================================
-- shapes — the public contracts of this module (handwritten lshape)
-- ============================================================
--
-- Every public IF is defined here and published through `M.shapes`
-- (session-device-design.md §9-k), so a caller reads the contract as data
-- rather than out of prose, and `M.shapes.api` (§9-m) names the shape of
-- every export so a spec can check the registry is complete instead of a
-- person remembering to update it.
--
-- The shapes are asserted at the boundaries in dev mode (LSHAPE_CHECK=1)
-- and are a no-op in prod. That is why every construction check that must
-- be loud — `knl.device`'s — also exists as an explicit check beside the
-- assert: a dev-only gate would let a broken device through in prod.
--
-- Two limits of the DSL are worked around in the open rather than hidden:
-- lshape has no numeric range, so `cost_result` says "number" and the
-- whole-number >= 1 rule is checked in beat [3]; and "callable" (a
-- function, or a table / userdata carrying `__call`) is wider than any one
-- prim, so the shape admits the three types and `device_problem` makes the
-- exact judgement.
--
-- What is deliberately NOT here: the kernel's own event vocabulary
-- (msg_user / llm_request / llm_response / llm_call_failed / tool_call /
-- tool_result). The Rust validator is its single source of truth
-- (session-device-design.md §11 R7); two copies of it drifted apart in
-- three fields. What this layer adds — and therefore all it checks — is
-- the `beat` id it stamps on the events it writes.

--- A Lua function, as a shape. lshape's `prim` handler is `type(v) ==
--- schema.prim` for any type name, but `lshape.t` exposes only the five it
--- names, so these two are built from the same plain-data schema form
--- (Schema-as-Data: the state is the table; the metatable carries only the
--- `:is_optional()` / `:describe()` sugar).
local FUNCTION = setmetatable({ kind = "prim", prim = "function" }, lshape.t._internal.schema_mt)
local USERDATA = setmetatable({ kind = "prim", prim = "userdata" }, lshape.t._internal.schema_mt)

--- Something a beat can call: a function, or a table / userdata carrying
--- `__call` — a Port shim may hand back either. The exact test is
--- `callable` below, run loudly at construction; this is its data shape.
local CALLABLE = T.any_of({ FUNCTION, T.table, USERDATA })

--- An Outcome, as data. Discriminated on `status` — exactly the kernel's
--- four, each variant carrying only its own fields.
local OUTCOME = T.discriminated("status", {
    ok = T.shape({
        status = T.literal("ok"),
        out = T.table,
    }),
    refused = T.shape({
        status = T.literal("refused"),
        reason = T.string,
        detail = T.table:is_optional(),
    }),
    error = T.shape({
        status = T.literal("error"),
        kind = T.one_of({ "conf", "filter", "call", "state" }),
        detail = T.any,
    }),
    stopped = T.shape({
        status = T.literal("stopped"),
        reason = T.string,
        tag = T.string:is_optional(),
    }),
})

--- One wire tool declaration inside a request (handler already stripped).
local WIRE_TOOL = T.shape({
    name = T.string,
    description = T.string:is_optional(),
    input_schema = T.any:is_optional(),
})

--- The provider-neutral request fold/filters hand to the llm. `system` is
--- provider vocabulary (string or blocks) — any. Open: a filter may attach
--- keys the default fold does not know about.
local REQUEST = T.shape({
    messages = T.array_of(T.table),
    system = T.any:is_optional(),
    tools = T.array_of(WIRE_TOOL):is_optional(),
})

--- Every event, in the only terms this layer owns: a `kind` string, and —
--- when the event is one a beat wrote — the `beat` id stamped on it, which
--- is an opaque string and nothing more.
---
--- Per-kind field contracts are deliberately absent. The kernel's
--- validator holds them (session-device-design.md §11 R7), the vocabulary
--- is open (session_* / budget_* and a caller's own kinds exist beyond
--- what this module writes), and a second copy here is a source of truth
--- that can only drift.
local EVENT_BASE = T.shape({
    kind = T.string,
    beat = T.string:is_optional(),
})

--- One entry of a device's `tools` map: what the model may call
--- (description / input_schema, both optional and both provider
--- vocabulary) plus how to call it. Open: `schema` is still accepted as
--- the legacy alias for `input_schema` by fold's wire_tools.
local TOOL_ENTRY = T.shape({
    description = T.string:is_optional(),
    input_schema = T.any:is_optional(),
    handler = FUNCTION,
})

--- What a `tool_policy` may decide (session-device-design.md §9-l): `nil`
--- is "no opinion" and runs, and the two words are the whole vocabulary.
--- Anything else is a device-contract violation, not a third meaning.
local TOOL_POLICY_DECISION = T.one_of({ "run", "deny" }):is_optional()

--- What `device.cost` must answer: a whole number >= 1, which is what
--- makes the budget a ranking function and the run finite. lshape has no
--- numeric range or integrality combinator, so the shape carries the type
--- and beat [3] carries the bound — loudly, in prod as well as dev.
local COST_RESULT = T.number:describe("a whole number >= 1 (the bound is checked in beat, not by this shape)")

--- The config `knl.device` consumes. Closed: a policy typo must not
--- quietly become a no-op, which is also why `knl.device` rejects unknown
--- keys loudly rather than leaving it to this dev-mode assert.
local DEVICE_CONFIG = T.shape({
    llm = CALLABLE:is_optional(),
    tools = T.map_of(T.string, TOOL_ENTRY):is_optional(),
    tool_policy = FUNCTION:is_optional(),
    fold = FUNCTION:is_optional(),
    filters = T.array_of(FUNCTION):is_optional(),
    system = T.any:is_optional(),
    cost = FUNCTION:is_optional(),
}, { open = false })

--- What an owner grants a session. `amount` is a count of whatever `tag`
--- names (the kernel reads the number and nothing else); lshape has no
--- integer prim, so the whole-number expectation rides in the doc.
local BUDGET_GRANT = T.shape({
    amount = T.number:describe("a whole number of units"),
    tag = T.string:is_optional(),
    desc = T.string:is_optional(),
})

--- `knl.open` opts: state only. Policy has its own constructor.
local OPEN_OPTS = T.shape({
    owner = T.string:is_optional(),
    budget = BUDGET_GRANT:is_optional(),
    store = T.any:is_optional(),
})

--- `knl.resume` opts: the store and the session to reopen, plus the grant
--- this process runs under.
local RESUME_OPTS = T.shape({
    store = T.any,
    session = T.string,
    budget = BUDGET_GRANT:is_optional(),
})

--- The token accounting an llm answer promises: three counts, always
--- present as numbers. An adapter normalizes a provider that reported none
--- to zeros, so this stays strict rather than admitting a missing field.
--- Closed so a stray usage key cannot ride across the boundary.
local USAGE = T.shape({
    input_tokens = T.number,
    output_tokens = T.number,
    thinking_tokens = T.number,
}, { open = false })

--- The refusal detail an llm answer carries alongside a "refused" status.
--- `kind` normalizes *why* the beat did not progress across providers:
--- "model" is the model declining, "content_filter" a provider safety
--- filter blocking it — a distinction the kernel's status cannot carry, so
--- it rides here. Present iff status == "refused".
local REFUSAL = T.shape({
    kind = T.one_of({ "model", "content_filter" }),
    detail = T.string:is_optional(),
}, { open = false })

--- What `device.llm(request)` hands back — the shape beat reads at [5],
--- and the one an adapter's Mapper is held to on the way out (knl_adapter
--- asserts against this very table). Closed, so a contract gap in one
--- provider's parse cannot leak past the boundary: `content` is an array
--- of blocks (tagged as an empty array when the model said nothing, so it
--- crosses the JSON bridge as `[]`), `usage` is the strict count above,
--- `stop_reason` is absent when no reason was given, and `refusal` is
--- present exactly on "refused".
---
--- The third status a beat can meet — a transport / provider failure — is
--- not a variant here: that path answers `nil, err` (or raises), which
--- beat records as `llm_call_failed` and reports as `err("call")`.
local LLM_RESULT = T.shape({
    content = T.array_of(T.table),
    usage = USAGE,
    stop_reason = T.string:is_optional(),
    status = T.one_of({ "ok", "refused" }),
    refusal = REFUSAL:is_optional(),
}, { open = false })

--- The contracts this module holds itself to, as data.
M.shapes = {
    outcome = OUTCOME,
    request = REQUEST,
    event_base = EVENT_BASE,
    device_config = DEVICE_CONFIG,
    tool_entry = TOOL_ENTRY,
    tool_policy_decision = TOOL_POLICY_DECISION,
    cost_result = COST_RESULT,
    llm_result = LLM_RESULT,
    open_opts = OPEN_OPTS,
    resume_opts = RESUME_OPTS,
    budget_grant = BUDGET_GRANT,
}

--- The API registry (session-device-design.md §9-m): one entry per public
--- export, naming the shape of what goes in and what comes out. It exists
--- so the completeness of the contract is *checked* rather than remembered
--- — `knl/spec/api_spec.lua` walks this module and fails on an export with
--- no entry, an entry with no export, and a device field that
--- `device_config` does not describe.
---
--- `args` is a shape, or an ordered list of shapes and descriptions for
--- the arguments a shape cannot express (a session handle, a callback).
--- `returns` is the same. `members` names the functions of an export that
--- is itself a namespace (`Outcome`).
---
--- This registry covers the Lua module. The bridge declares its own surface
--- through `knl.api()` (SESSION_API / MODULE_API in bridge/knl.rs), and
--- `M.shapes.session` / `M.shapes.module` below describe that surface from
--- this side; `tests/fixtures/knl_turn_test.lua` (inv10, runs with the
--- bridge) checks the two against each other in both directions, so a
--- syscall added on one side and not the other goes red.
M.shapes.session = {
    id = { args = "none", returns = "string (stream id)" },
    scope_id = { args = "none", returns = "string (kernel-issued scope id)" },
    owner = { args = "none", returns = "string (principal, or anon / system)" },
    append = { args = { EVENT_BASE }, returns = "integer seq; refused after session_closed and for kernel-only kinds" },
    events = { args = { "integer from?" }, returns = T.array_of(EVENT_BASE) },
    len = { args = "none", returns = "integer" },
    view = { args = { T.one_of({ "usage", "tail" }), "table opts?" }, returns = "table (the named fold)" },
    reserve = { args = { "integer n >= 0" }, returns = "true | false, tag — decided inside the store" },
    spend = { args = { "integer n >= 0" }, returns = "integer remaining | nil (no grant)" },
    remaining = { args = "none", returns = "integer | nil (no grant)" },
    exhausted = { args = "none", returns = "boolean" },
    close = {
        args = { T.string:is_optional(), T.string:is_optional() },
        returns = "nothing; records session_closed{reason, detail?} once per stream",
    },
    __close = { args = { "session", "any err" }, returns = "nothing; scope_exit or error(+detail)" },
}

M.shapes.module = {
    open = { args = OPEN_OPTS, returns = "session" },
    resume = { args = RESUME_OPTS, returns = "session" },
    new_beat_id = { args = "none", returns = "string" },
    api = { args = "none", returns = "{ session = { {name, doc}... }, module = { {name, doc}... } }" },
}

M.shapes.api = {
    open = {
        args = OPEN_OPTS,
        returns = "session (the kernel's userdata, unwrapped)",
    },
    resume = {
        args = RESUME_OPTS,
        returns = "session (pre-loaded with the persisted log)",
    },
    session = {
        args = { "open_opts | resume_opts", "function fn(session)" },
        returns = "whatever fn returned (the session is closed either way)",
    },
    device = {
        args = DEVICE_CONFIG,
        returns = "device (frozen; d:with derives another)",
    },
    beat = {
        args = { "session", "device" },
        returns = OUTCOME,
    },
    fold = {
        args = { T.array_of(EVENT_BASE), "device (read for system / tools)" },
        returns = REQUEST,
    },
    new_beat_id = {
        args = "none",
        returns = "string (time-ordered, session-free)",
    },
    Outcome = {
        args = "none (a namespace table, not a function)",
        returns = "the Outcome constructors, predicates and match",
        members = {
            ok = { args = { "table out" }, returns = OUTCOME },
            refused = { args = { "string reason", "table detail?" }, returns = OUTCOME },
            err = { args = { T.one_of({ "conf", "filter", "call", "state" }), "any detail" }, returns = OUTCOME },
            stopped = { args = { "string reason", "string tag?" }, returns = OUTCOME },
            is_ok = { args = { "any" }, returns = "boolean" },
            is_refused = { args = { "any" }, returns = "boolean" },
            is_error = { args = { "any" }, returns = "boolean" },
            is_stopped = { args = { "any" }, returns = "boolean" },
            match = {
                args = { OUTCOME, "table arms { ok, refused, error, stopped }" },
                returns = "whatever the taken arm returned",
            },
        },
    },
    shapes = {
        args = "none (a data table)",
        returns = "this registry: every shape above, plus `api`, `session` and `module`",
    },
}

--- Dev-mode gate for an event a beat is about to write: the contract this
--- layer owns (a `kind`, and a `beat` that is a string when present). The
--- kernel's validator holds the per-kind contract and refuses what it does
--- not accept — this is not a second copy of it. No-op in prod.
---
--- It guards only what *beat* writes. A caller's own `session:append` goes
--- straight to the kernel validator: the session handle is the kernel's,
--- not something this module wraps.
local function assert_event_dev(ev)
    return shape.assert_dev(ev, EVENT_BASE, "knl_event")
end

--- Dev-mode gate for an Outcome on its way out of beat. No-op in prod.
local function emit(o)
    return shape.assert_dev(o, OUTCOME, "knl_outcome")
end

-- ============================================================
-- device — the resolved, frozen policy half
-- ============================================================

--- The fields a device carries. The config is consumed at construction, so
--- this is both the accepted key set and the field set of the result.
--- Everything that names the session itself (owner / store / session /
--- budget) is state and belongs to `knl.open` / `knl.resume`.
local DEVICE_KEYS = { "llm", "tools", "tool_policy", "fold", "filters", "system", "cost" }

local IS_DEVICE_KEY = {}
for _, k in ipairs(DEVICE_KEYS) do
    IS_DEVICE_KEY[k] = true
end

--- The metatable name `beat` recognises a device by. A protected metatable
--- (`__metatable`), so the tag cannot be forged by assignment either.
local DEVICE_TAG = "knl.device"

--- The tag on a device's frozen tools map.
local TOOLS_TAG = "knl.device.tools"

--- How much one beat asks the budget for when the device names no policy.
---
--- One beat, one unit: a beat is the thing being bounded, so the default
--- makes the grant a count of beats. It must be >= 1 — that is what makes
--- the budget a ranking function and the run finite. What the unit *means*
--- is the owner's (`budget = { amount = N, tag = "tokens" }` counts tokens,
--- and then a device supplies `cost` to say how many one beat may take).
local function default_cost()
    return 1
end

--- Whether `v` can be called like a function (a callable table / userdata
--- counts: a Port shim may hand back either).
local function callable(v)
    if type(v) == "function" then
        return true
    end
    local mt = getmetatable(v)
    return type(mt) == "table" and mt.__call ~= nil
end

--- A read-only view over `fields`: reads pass through, `pairs` walks the
--- underlying table, and every assignment raises — including one to a key
--- that already exists, which a plain `__newindex` on the table itself
--- would let through.
local function frozen(fields, what, tag)
    return setmetatable({}, {
        __index = fields,
        __newindex = function(_, k)
            error(what .. " is frozen: '" .. tostring(k) .. "' cannot be assigned", 2)
        end,
        __pairs = function()
            return next, fields, nil
        end,
        __len = function()
            return #fields
        end,
        __metatable = tag,
    })
end

local function shallow_copy(t)
    local out = {}
    for k, v in pairs(t) do
        out[k] = v
    end
    return out
end

local function copy_array(t)
    local out = {}
    for i, v in ipairs(t or {}) do
        out[i] = v
    end
    return out
end

--- Minimal device config check. Returns an error string, or nil when the
--- config is usable. `llm` is not required here — a device may be built for
--- its tools alone; beat's gate demands one.
---
--- This is the loud half of the pair: `DEVICE_CONFIG` says the same thing
--- as data and is asserted beside it, but a dev-mode assert is a no-op in
--- prod and a device built out of a mistyped config would then fail at the
--- first beat instead of at the line that built it. It also makes the two
--- judgements a shape cannot: "callable" (function / `__call`), and "a map
--- of entries, not an array of flat specs".
local function device_problem(config)
    if config.llm ~= nil and not callable(config.llm) then
        return "llm must be a function (or a callable)"
    end
    if config.tools ~= nil then
        if type(config.tools) ~= "table" then
            return "tools must be a map of name -> { description?, input_schema?, handler }"
        end
        if config.tools[1] ~= nil then
            return "tools must be a map (name -> entry); bind an array of specs with knl_adapter.tools first"
        end
    end
    if config.tool_policy ~= nil and type(config.tool_policy) ~= "function" then
        return "tool_policy must be a function"
    end
    if config.fold ~= nil and type(config.fold) ~= "function" then
        return "fold must be a function"
    end
    if config.cost ~= nil and type(config.cost) ~= "function" then
        return "cost must be a function (fn(request) -> integer >= 1)"
    end
    if config.filters ~= nil then
        if type(config.filters) ~= "table" then
            return "filters must be an array of functions"
        end
        for i, filter in ipairs(config.filters) do
            if type(filter) ~= "function" then
                return "filters[" .. i .. "] must be a function"
            end
        end
    end
    return nil
end

local device_with -- forward declaration (served by the device's __index)

--- Build a device: the resolved, frozen policy half of a beat.
---
--- The config is *consumed* here — defaults are resolved once (`fold` ->
--- `knl.fold`, `filters` -> `{}`, `cost` -> one unit per beat), types are
--- checked once, and the result carries only resolved values. There is no
--- `_config` to read back: what the device does is what its fields say.
--- Unknown keys raise, because a typo in a policy field must not silently
--- become a no-op.
---
--- The device is stateless: share one across sessions, and give one session
--- several (escalation) — nothing about a beat is remembered here.
---
--- @param config table  { llm?, tools?, tool_policy?, fold?, filters?,
---                        system?, cost? }
--- @return table device  frozen, with `with` for derivation
function M.device(config)
    config = config or {}
    if type(config) ~= "table" then
        error("knl.device: config must be a table", 2)
    end
    for k in pairs(config) do
        if not IS_DEVICE_KEY[k] then
            error(
                "knl.device: unknown option '"
                    .. tostring(k)
                    .. "' (owner/budget/store/session are state — open or resume a session with them)",
                2
            )
        end
    end
    -- Loud in prod (a construction error must not wait for the first beat)
    -- and shaped in dev (the same contract as data, which is what
    -- `knl.shapes.device_config` publishes).
    local problem = device_problem(config)
    if problem then
        error("knl.device: " .. problem, 2)
    end
    shape.assert_dev(config, DEVICE_CONFIG, "knl.device config")

    local fields = {
        llm = config.llm,
        tool_policy = config.tool_policy,
        system = config.system,
        fold = config.fold or M.fold,
        cost = config.cost or default_cost,
        -- The caller's arrays/maps are copied, so writing to them after
        -- construction cannot reach the device (session-device-design §9-d).
        filters = copy_array(config.filters),
        tools = config.tools and frozen(shallow_copy(config.tools), "a device's tools map", TOOLS_TAG) or nil,
    }

    return setmetatable({}, {
        __index = function(_, k)
            if k == "with" then
                return device_with
            end
            return fields[k]
        end,
        __newindex = function(_, k)
            error("a device is frozen: derive a new one with d:with{ " .. tostring(k) .. " = ... }", 2)
        end,
        __pairs = function()
            return next, fields, nil
        end,
        __metatable = DEVICE_TAG,
    })
end

--- Derive a new device: this one's resolved fields with `delta` over them,
--- re-resolved through `knl.device`. The original is untouched and stays
--- usable — a device is a value, and `with` is how a beat gets a different
--- one (`knl.beat(s, d:with{ llm = strong })`).
---
--- @param d table  a knl device
--- @param delta table  device fields to override
--- @return table device'  a new frozen device
device_with = function(d, delta)
    if type(delta) ~= "table" then
        error("device:with: delta must be a table", 2)
    end
    local merged = {}
    for _, k in ipairs(DEVICE_KEYS) do
        merged[k] = d[k]
    end
    for k, v in pairs(delta) do
        if not IS_DEVICE_KEY[k] then
            error(
                "device:with: '"
                    .. tostring(k)
                    .. "' is not a device field (owner/store/session name a session — open one instead)",
                2
            )
        end
        merged[k] = v
    end
    return M.device(merged)
end

-- ============================================================
-- open / resume / session — the state half
-- ============================================================

local OPEN_STATE_KEYS = { owner = true, budget = true, store = true }
local RESUME_STATE_KEYS = { store = true, session = true, budget = true }

--- Reject anything that is not a state key. Policy has its own constructor
--- now, so `knl.open{ llm = ... }` is a typo, not a shorthand.
local function state_only(opts, allowed, who)
    if type(opts) ~= "table" then
        error(who .. ": opts must be a table", 3)
    end
    for k in pairs(opts) do
        if not allowed[k] then
            local hint = IS_DEVICE_KEY[k] and " (policy belongs to knl.device)" or ""
            error(who .. ": unknown option '" .. tostring(k) .. "'" .. hint, 3)
        end
    end
end

--- Open a session. The kernel's userdata comes back as-is — this module
--- wraps nothing: `s:append`, `s:events`, `s:reserve`, `s:view`, `s:close`
--- and `<close>` are the kernel's own surface.
---
--- @param opts table  { owner?, budget? = { amount, tag?, desc? }, store? }
--- @return userdata session
function M.open(opts)
    opts = opts or {}
    state_only(opts, OPEN_STATE_KEYS, "knl.open")
    shape.assert_dev(opts, OPEN_OPTS, "knl.open opts")
    return bridge("open")({
        owner = opts.owner,
        budget = opts.budget,
        store = opts.store,
    })
end

--- Resume a persisted session. The state comes back from the store (the
--- bridge reopens the durable stream and re-folds the log); policy does NOT
--- — it is a non-serializable closure bundle, so every process builds its
--- own device.
---
--- @param opts table  { store = { sqlite = <path> }, session = <id>, budget? }
--- @return userdata session  pre-loaded with the log
function M.resume(opts)
    opts = opts or {}
    state_only(opts, RESUME_STATE_KEYS, "knl.resume")
    shape.assert_dev(opts, RESUME_OPTS, "knl.resume opts")
    return bridge("resume")({
        store = opts.store,
        session = opts.session,
        budget = opts.budget,
    })
end

--- The canonical bracket (session-device-design.md §9-e): open (or resume),
--- run the body with the session, close it either way.
---
---     local out = knl.session({ owner = "u" }, function(s)
---         return knl.beat(s, d)
---     end)
---
--- Opts naming a `session` resume that stream instead of opening a new one.
--- The body's return values are the bracket's. When the body raises, the
--- session is closed with reason "error" first and the body's error is then
--- re-raised unchanged: a bookkeeping failure must not replace the failure
--- it is bookkeeping for (§9-f), so a close that itself fails on that path
--- is dropped. On the clean path a failing close raises, since a bracket
--- that reports success with no boundary recorded is the one outcome this
--- exists to rule out.
---
--- A close that fails on the error path is not silent either: it goes to
--- the host `log` global as a warning when the VM has one. It cannot be
--- raised (that would replace the body's error) and it cannot be returned
--- (this path does not return).
---
--- The reason vocabulary is the kernel's, and a normal exit has one word in
--- it: "scope_exit" — what this bracket closes with and what `<close>`
--- records, because leaving the scope is the same event whichever form
--- wrote it. "error" is the failing path (here and in `<close>`), "dropped"
--- the Drop backstop, and "closed" stays the bridge's DEFAULT_CLOSE_REASON
--- for a bare `s:close()`. The message of a body error does not ride along
--- — `s:close` takes a reason and nothing else — so it stays with the
--- error that is propagating.
---
--- @param opts table  knl.open / knl.resume opts
--- @param fn function  fn(session) -> ...
--- @return ...  whatever `fn` returned
function M.session(opts, fn)
    if type(fn) ~= "function" then
        error("knl.session: the second argument must be a function fn(session)", 2)
    end
    opts = opts or {}
    local s
    if opts.session ~= nil then
        s = M.resume(opts)
    else
        s = M.open(opts)
    end

    local returned = table.pack(pcall(fn, s))
    if returned[1] then
        s:close("scope_exit")
        return table.unpack(returned, 2, returned.n)
    end
    -- The body is failing: close best-effort with the body's error as the
    -- boundary's `detail`, then let that error through. A close that fails
    -- here is reported, not raised and not swallowed — the body's error is
    -- the one that propagates (§9-f, the suppressed exception of
    -- try-with-resources).
    local closed_ok, cerr = pcall(s.close, s, "error", tostring(returned[2]))
    if not closed_ok then
        local host_log = rawget(_G, "log")
        if type(host_log) == "table" and type(host_log.warn) == "function" then
            pcall(host_log.warn, "knl.session: close failed after body error: " .. tostring(cerr))
        end
    end
    error(returned[2], 0)
end

--- Mint a beat id: a time-ordered, session-free string the caller stamps on
--- the events of one beat (session-device-design.md §9-a). A module
--- function, not a direct bridge call, so a spec can stand in for it.
---
--- @return string beat_id
function M.new_beat_id()
    return bridge("new_beat_id")()
end

-- ============================================================
-- beat — one complete beat (the primitive; there is no loop here)
-- ============================================================

--- Whether `s` answers the part of the session surface a beat uses. A
--- duck-type on purpose: the real handle is Rust userdata with no metatable
--- name of its own, and a faithful Lua stand-in must be beatable too.
local function is_session(s)
    local t = type(s)
    if t ~= "table" and t ~= "userdata" then
        return false
    end
    local ok, append, reserve, events = pcall(function()
        return s.append, s.reserve, s.events
    end)
    return ok and callable(append) and callable(reserve) and callable(events)
end

--- Append an event this beat is writing, through the dev-mode contract.
local function record(session, ev)
    return session:append(assert_event_dev(ev))
end

--- Ask the policy about one tool_use block (session-device-design.md §9-l).
---
--- The contract is `tool_policy(tool_use_block, out) -> decision, reason?`
--- with a decision of `nil` (no opinion — run), `"run"` or `"deny"`, and
--- nothing else: a fourth word is a device-contract violation rather than a
--- fourth meaning this layer gets to guess at. A policy that RAISES is
--- fail-closed — a gate written to veto tools must not fall open on its own
--- bug — so the raise denies the call and its message becomes the reason.
---
--- @param policy function|nil  device.tool_policy
--- @param block table  the tool_use block being decided
--- @param out table  the model response it came in
--- @return string|nil action  "run" / "deny", or nil on a violation
--- @return string|nil reason  the denial's reason, when there is one
--- @return string|nil problem  the contract violation, when there is one
local function decide_tool(policy, block, out)
    if policy == nil then
        return "run"
    end
    local ok, decided, reason = pcall(policy, block, out)
    if not ok then
        return "deny", "policy raised: " .. tostring(decided)
    end
    -- The published shape is the vocabulary, checked (not asserted) so the
    -- judgement is the same in prod as in dev: a decision outside it is a
    -- contract violation to report, not a raise.
    if not shape.check(decided, TOOL_POLICY_DECISION) then
        return nil,
            nil,
            "tool_policy returned "
                .. (type(decided) == "string" and string.format("%q", decided) or type(decided))
                .. ' (a decision must be nil, "run" or "deny")'
    end
    -- Past the check there are three values left: nil (no opinion), "run",
    -- and "deny".
    if decided ~= "deny" then
        return "run"
    end
    -- A reason is optional and a string; anything else is not made up into
    -- one, it is left out.
    return "deny", (type(reason) == "string" and reason ~= "" and reason) or nil
end

--- Run the tool_use blocks of a response, closing every one with a
--- tool_result. What runs / is denied is `device.tool_policy`, the success
--- result is the handler's, and the pair-closing record — including the
--- machine-minimal error for an unknown tool, a denied call or a raising
--- handler — is the kernel's. `beat_id` stamps both halves of every pair,
--- so the pair reads back as part of its beat.
---
--- Every block is decided before any of them runs. A policy that breaks its
--- contract therefore stops the beat with nothing dispatched and no
--- tool_call written, rather than half a response's worth of side effects
--- and a report that the config was wrong.
---
--- @param session userdata|table  a knl session
--- @param device table  a knl device
--- @param out table  the model response being executed
--- @param beat_id string  the id of the beat writing these events
--- @return table|nil summary  one { call_id, name, ok } per tool_use
--- @return string|nil problem  a device-contract violation (nothing ran)
local function execute_tools(session, device, out, beat_id)
    local tools = device.tools or {}
    local policy = device.tool_policy

    -- [a] decide everything first (nothing has run, nothing is recorded)
    local planned = {}
    for _, block in ipairs(out.content or {}) do
        if block.type == "tool_use" then
            local action, reason, problem = decide_tool(policy, block, out)
            if problem then
                return nil, problem
            end
            planned[#planned + 1] = { block = block, action = action, reason = reason }
        end
    end

    -- [b] then run them, each pair recorded around its call
    local summary = {}
    for _, item in ipairs(planned) do
        local block = item.block
        local call_id = tostring(block.id or "")
        local name = tostring(block.name or "")
        local args = block.input or {}

        -- Record the call before running it: a run that dies mid-tool
        -- leaves a history that says a call was made.
        record(session, {
            kind = "tool_call",
            beat = beat_id,
            call_id = call_id,
            name = name,
            args = args,
        })

        local ok, result
        if item.action == "deny" then
            local why = item.reason and (": " .. item.reason) or ""
            ok, result = false, "tool '" .. name .. "' denied by policy" .. why
        else
            local spec = tools[name]
            if spec == nil or spec.handler == nil then
                -- Unknown tool: close the pair with a machine-minimal
                -- error. This is the kernel's skeleton, not a policy.
                ok, result = false, "tool '" .. name .. "' not found"
            else
                local pok, pres = pcall(spec.handler, args)
                if pok then
                    ok, result = true, pres
                else
                    ok, result = false, "tool '" .. name .. "' raised: " .. tostring(pres)
                end
            end
        end

        -- `result` must be present for a tool_result; a handler that
        -- answered with nil gets the empty string in the record.
        if result == nil then
            result = ""
        end

        record(session, {
            kind = "tool_result",
            beat = beat_id,
            call_id = call_id,
            ok = ok,
            result = result,
        })

        summary[#summary + 1] = { call_id = call_id, name = name, ok = ok }
    end

    return summary
end

--- One complete beat: gate, name itself, fold, filter, reserve, record,
--- call, record, run its tools. Two arguments and no bundle — the session
--- is the kernel's state, the device is the caller's policy, and neither is
--- mutated. Per-beat variation is the caller's: `knl.beat(s, d:with{ llm =
--- strong })`.
---
--- Re-entrant and stateless: a beat is decided entirely by its two
--- arguments, so it can be called from any driver, resumed, or interleaved.
---
--- @param session userdata|table  a knl session (knl.open / knl.resume)
--- @param device table  a knl device (knl.device / d:with)
--- @return table outcome  an `Outcome`
function M.beat(session, device)
    -- [0] gate ------------------------------------------------------------
    if not is_session(session) then
        return emit(Outcome.err("conf", "beat takes a knl session first (from knl.open / knl.resume)"))
    end
    if getmetatable(device) ~= DEVICE_TAG then
        return emit(Outcome.err("conf", "beat takes a knl device second (build one with knl.device)"))
    end
    -- The llm is beat's to call, off the device.
    if device.llm == nil then
        return emit(Outcome.err("conf", "no llm in the device (knl.device{ llm = ... } / d:with{ llm = ... })"))
    end

    -- [0.5] the beat names itself ----------------------------------------
    -- One id per beat, minted here and stamped on every event this beat
    -- writes. The kernel neither numbers nor requires it (§9-a).
    local id_ok, beat_id = pcall(M.new_beat_id)
    if not id_ok or type(beat_id) ~= "string" then
        return emit(Outcome.err("conf", "no beat id: " .. tostring(beat_id)))
    end

    -- [1] request <- fold(events, device) ---------------------------------
    -- Reading the log is a kernel call like any other in this function: a
    -- store that cannot be read (a closed session, a failed query) is
    -- `err("state")`, not a raise escaping beat's Outcome contract. It is
    -- pcall'd separately from the fold so the two failures keep their own
    -- kinds — the state that could not be read, or the policy that could
    -- not fold it.
    local read_ok, events = pcall(session.events, session)
    if not read_ok then
        return emit(Outcome.err("state", "events read failed: " .. tostring(events)))
    end
    local folded_ok, request = pcall(device.fold, events, device)
    if not folded_ok then
        return emit(Outcome.err("conf", "fold failed: " .. tostring(request)))
    end

    -- [2] filter chain (fn(req) -> req) -----------------------------------
    for i, filter in ipairs(device.filters) do
        local filtered_ok, filtered = pcall(filter, request)
        if not filtered_ok then
            return emit(Outcome.err("filter", tostring(filtered)))
        end
        -- A filter's return replaces the request wholesale, so a filter
        -- that mutates and forgets to return (nil) or returns a
        -- non-table would corrupt the write-ahead record and the wire.
        -- Loud in prod, not just under the dev assert below.
        if type(filtered) ~= "table" then
            return emit(
                Outcome.err(
                    "filter",
                    "filter #" .. i .. " returned " .. type(filtered) .. " (a filter must return the request table)"
                )
            )
        end
        request = filtered
    end
    -- Dev-mode contract on what actually goes to the llm — a custom fold or
    -- a filter that broke the request shape fails loud here, not in the wire.
    shape.assert_dev(request, REQUEST, "knl_request")

    -- [3] reserve before anything is recorded or called -------------------
    -- The quota decides here, with the request known and nothing spent yet:
    -- a refusal leaves no `llm_request` event, makes no call, and is a planned
    -- stop rather than a failure (`stopped`, carrying the grant's tag so a
    -- caller can name what stopped it). How much to ask for is the device's
    -- policy — `device.cost(request)` — and it is never derived from token
    -- counts: the budget and the `usage` view are separate readings
    -- (budget-design.md §4-8).
    local est_ok, amount = pcall(device.cost, request)
    if not est_ok then
        return emit(Outcome.err("conf", "cost failed: " .. tostring(amount)))
    end
    -- `cost_result` carries the type; the bound and the integrality are
    -- here, because lshape has no combinator for either. Checked rather than
    -- asserted: this must be loud in prod too — an unbounded beat is how a
    -- run stops being finite.
    if not shape.check(amount, COST_RESULT) or amount < 1 or amount % 1 ~= 0 then
        return emit(Outcome.err("conf", "cost must return a whole number >= 1, got " .. tostring(amount)))
    end
    -- The handle raises on a closed session, and beat's contract is an
    -- Outcome, so the call is pcall'd.
    local res_ok, granted, tag = pcall(session.reserve, session, amount)
    if not res_ok then
        return emit(Outcome.err("state", "reserve failed: " .. tostring(granted)))
    end
    if not granted then
        return emit(Outcome.stopped("budget", tag))
    end

    -- [4] record the request write-ahead (open kind "llm_request") --------
    -- The request as actually sent is a fact in the history before the call,
    -- so a call that then fails leaves the llm_request event behind.  An append
    -- can fail (closed session, CAS head conflict, validation) — beat's
    -- contract is an Outcome, so a state failure is Error("state"), never
    -- a raw raise.
    local rec_ok, rec_err = pcall(record, session, {
        kind = "llm_request",
        beat = beat_id,
        request = request,
    })
    if not rec_ok then
        return emit(Outcome.err("state", "llm_request append failed: " .. tostring(rec_err)))
    end

    -- [5] beat calls the llm directly ------------------------------------
    -- resp = { status = "ok"|"refused"|"error", content, usage, stop_reason }.
    -- The status is the adapter's judgement; beat reads it, it does not
    -- invent one (status is llm-supplied).  The call is pcall'd: an adapter
    -- that raises (instead of returning nil, err) is still a call failure,
    -- not an escape from the Outcome contract.
    local call_ok, resp, berr = pcall(device.llm, request)
    if not call_ok then
        berr = "llm raised: " .. tostring(resp)
        resp = nil
    end

    -- [6] status branch — beat lays down the record -----------------------
    -- error / transport failure: the beat did not come off. Note it and stop.
    -- The failure note is best-effort (the state may be what failed); the
    -- call error stays the primary detail either way.
    --
    -- Anything that is not a table is that same failure: an adapter whose
    -- contract is `resp | nil, err` and that answered `false` / a string /
    -- a number has not produced a response, and reading `.status` off it
    -- would raise (or, for a string, quietly find the string library).
    if type(resp) ~= "table" or resp.status == "error" then
        local reason = berr
        if reason == nil then
            if type(resp) == "table" then
                reason = resp.detail or "llm reported error"
            elseif resp == nil then
                reason = "llm reported error"
            else
                reason = "llm returned " .. type(resp) .. " (the contract is a table, or nil and an error)"
            end
        end
        local noted_ok, note_err = pcall(record, session, {
            kind = "llm_call_failed",
            beat = beat_id,
            error = tostring(reason),
        })
        if not noted_ok then
            return emit(
                Outcome.err(
                    "state",
                    "call failed ("
                        .. tostring(reason)
                        .. ") and the failure note could not be recorded: "
                        .. tostring(note_err)
                )
            )
        end
        return emit(Outcome.err("call", tostring(reason)))
    end
    -- ok / refused: the model answered. Appending the llm_response is
    -- what records it, under this beat's id, and it charges nothing (the
    -- quota was settled at [3]).  `usage` defaults to an empty count: the
    -- llm contract leaves it optional, but the kernel validator requires
    -- the field on an llm_response (the Lua/Rust contract meet in the
    -- middle here).
    local resp_ok, resp_err = pcall(record, session, {
        kind = "llm_response",
        beat = beat_id,
        content = resp.content,
        usage = resp.usage or {},
        stop_reason = resp.stop_reason,
    })
    if not resp_ok then
        return emit(Outcome.err("state", "llm_response append failed: " .. tostring(resp_err)))
    end
    resp.beat = beat_id
    -- A refusal is a recorded response the model would not build on — and
    -- the beat that produced it reserved its amount like any other.
    if resp.status == "refused" then
        return emit(Outcome.refused(resp.stop_reason or "refused", resp))
    end

    -- [7] tool execution (skeleton) --------------------------------------
    -- execute_tools raises only on a state failure (an append that did not
    -- land); handler and policy failures close their pair as data
    -- (ok=false). A tool_policy that broke its contract is the one thing it
    -- reports instead: a config error, returned before any tool ran or any
    -- tool_call was written. The llm_response above is already recorded
    -- either way — the beat happened, it is the device that is wrong.
    local tools_ok, summary, policy_problem = pcall(execute_tools, session, device, resp, beat_id)
    if not tools_ok then
        return emit(Outcome.err("state", "tool record append failed: " .. tostring(summary)))
    end
    if policy_problem then
        return emit(Outcome.err("conf", policy_problem))
    end
    resp.tools = summary

    return emit(Outcome.ok(resp))
end

-- Internals exposed for the spec (the fixture drives these directly).
M._execute_tools = execute_tools
M._wire_tools = wire_tools

return M
