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
--- Reading the log back (view-design.md)
---   The log is a SQLite table whose columns the kernel publishes
---   (`knl.shapes.schema`), and a caller reads it by writing SQL:
---   `session:query(sql, params?, opts?)` binds values, refuses anything
---   that is not one SELECT / WITH, and resolves `$stream` (this session)
---   and `$sessions` (`opts.sessions`, the set to read across). A "view" is
---   nothing more than a named function that runs one of those statements —
---   `knl.views.beats` / `tool_pairs` / `ledger` are the three the kernel
---   ships, and a consumer writes its own in exactly the same form. The
---   kernel's built-in `events` / `usage` / `tail` are unaffected.
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
--- When something raises (session-device-design.md §9-r)
---   Two kinds of failure meet inside a beat and they are reported
---   differently. A KERNEL SYSCALL raises attributed text — `knl:
---   <method>: <kind>: <message>` — because mlua cannot carry a table out
---   of a Rust callback; beat reads it back with `knl.error` and the
---   reading is what lands in `Outcome.err("state").detail`: `{ kind?,
---   method?, retryable, message }`, with `kind` one of
---   `knl.shapes.error_kinds` and `retryable` true for exactly one of them.
---   beat never acts on `retryable` itself — asking again is the caller's
---   loop's decision, because only the loop knows how many times and for
---   how long it may. A CALLER'S OWN FUNCTION raising (fold, a filter,
---   cost, the llm) is a bug in the device rather than a class of kernel
---   failure, so its detail is the message it raised — plus, in dev mode
---   only, the `traceback` of where it raised.
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

--- Read a raised kernel failure back as data: `{ kind?, method?, retryable,
--- message }` (session-device-design.md §9-r).
---
--- The bridge cannot raise a table, so it raises the text `knl: <method>:
--- <kind>: <message>` and publishes `knl.error` to read it back. That
--- reader is resolved through the same lazy `bridge()` lookup
--- `new_beat_id` uses, so a spec that installs a fake bridge after this
--- module loaded still reaches it.
---
--- A VM with no bridge at all (the pure lspec runner) gets the same fields
--- with nothing read out of them, so `Outcome.err("state").detail` has one
--- shape everywhere: unclassified (`kind` and `method` absent), not
--- retryable, and the raised text verbatim. That is also what an
--- unattributed raise gets from the bridge itself — a reader that raised on
--- unfamiliar input would make a second failure inside the handler for the
--- first.
---
--- @param e any  the value a pcall'd syscall raised
--- @return table  { kind?, method?, retryable, message }
local function read_error(e)
    local reader_ok, reader = pcall(bridge, "error")
    if reader_ok then
        local read_ok, structured = pcall(reader, e)
        if read_ok and type(structured) == "table" then
            return structured
        end
    end
    return { retryable = false, message = tostring(e) }
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
--- record that could not be laid down: closed session, a store that would
--- not take the write, event validation).
---
--- Two vocabularies meet on this value and they answer different questions.
--- `kind` here names the STAGE of the beat that failed. What the failure
--- *was* — and whether asking again could work — is in `detail`: a "state"
--- failure carries the kernel's reading (`detail.kind` one of
--- `knl.shapes.error_kinds`, `detail.retryable`). A loop deciding on a retry
--- reads `detail.retryable`, never this `kind`; "state" says where, not
--- whether.
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
---                   follows the assistant message they answer (consecutive
---                   tool_results batch together, which for a well-formed
---                   history is the same as grouping by beat)
---
--- `system` and `tools` are composed from the device each beat, not read
--- from the history.
---
--- What "provider-neutral" names here
---   Neutral is a choice of shape, not the absence of one: the request and
---   response this fold speaks ARE the Anthropic content-block shape. An
---   assistant message is an array of blocks, a `tool_use` block carries a call
---   and a `tool_result` block answers it by `tool_use_id`. `close_dangling`
---   below depends on exactly that — it pairs the `tool_use` ids of an
---   assistant message against the results that answered them, a repair no
---   flatter shape could express. Other providers do not arrive here in
---   their own dialect: `llm_proto`'s adapters normalise them into these
---   blocks on the way in and render them back to the provider's wire on the
---   way out, so fold never sees a provider-specific shape.
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

--- A session handle, as a shape: the kernel's own userdata, or the faithful
--- Lua stand-in a spec drives a beat with. Which methods it must answer is
--- `is_session`'s duck-type (the whole declared surface) and no schema can
--- ask a userdata that — so this says the two types the handle can have, and
--- the entry point that receives one still makes the real judgement.
local SESSION_HANDLE = T.any_of({ T.table, USERDATA })

--- The STAGE of a beat an `error` Outcome names (see `Outcome.err`). One
--- constant: the shape below closes on it and the registry declares
--- `Outcome.err`'s first argument with it, so the four words are written
--- once.
local OUTCOME_KIND = T.one_of({ "conf", "filter", "call", "state" })

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
        kind = OUTCOME_KIND,
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

--- What a `tool_use` block inside a response names: the id the tool_result
--- will answer, and the tool to run. beat reads both straight off the block
--- — an unnamed call is the llm breaking this contract, not a hole for the
--- kernel to fill with an empty string.
---
--- Open, and about `tool_use` alone: what else a block may carry (`input`,
--- and whatever a provider adds) is the provider's vocabulary, and this
--- layer has no business closing it.
local TOOL_USE_BLOCK = T.shape({
    type = T.literal("tool_use"),
    id = T.string,
    name = T.string,
})

--- What `device.llm(request)` hands back — the shape beat reads at [5],
--- and the one an adapter's Mapper is held to on the way out (knl_adapter
--- asserts against this very table). Two statuses and no more: a transport
--- or provider failure is not a variant here, because that path answers
--- `nil, err` (or raises), which beat records as `llm_call_failed` and
--- reports as `err("call")`.
---
--- Discriminated on `status` rather than one shape with an optional
--- `refusal`, because "present exactly on a refusal" is the contract and an
--- optional field says only "sometimes". A refusal that named no `kind`
--- would leave beat with nothing to report the refusal AS.
---
--- Both variants are closed, so a contract gap in one provider's parse
--- cannot leak past the boundary: `content` is an array of blocks (tagged
--- as an empty array when the model said nothing, so it crosses the JSON
--- bridge as `[]`), `usage` is the strict count above, and `stop_reason` is
--- absent when no reason was given.
---
--- The per-block rule above rides beside this shape rather than inside it:
--- it applies to one `type` of block and lshape has no combinator for
--- "strict for this variant, open for the rest" that would not also close
--- the block vocabulary.
local LLM_RESULT = T.discriminated("status", {
    ok = T.shape({
        status = T.literal("ok"),
        content = T.array_of(T.table),
        usage = USAGE,
        stop_reason = T.string:is_optional(),
    }, { open = false }),
    refused = T.shape({
        status = T.literal("refused"),
        content = T.array_of(T.table),
        usage = USAGE,
        stop_reason = T.string:is_optional(),
        refusal = REFUSAL,
    }, { open = false }),
})

--- Every class a kernel failure can have, in one closed list — the Lua
--- side of the Rust `KnlError::KINDS` (session-device-design.md §9-r).
---
--- One constant, used by the shape below and by the check that holds this
--- declaration against the bridge's own (`knl.api().errors`, compared in
--- `tests/fixtures/knl_beat_test.lua` inv10, the test that has a bridge).
--- Retyping the list beside the shape would make a second place to keep in
--- step, which is the thing §9-m exists to rule out.
---
--- `timeout` is the read side's own class (view-design.md §3 decision 2): a
--- `session:query` that ran past its deadline was interrupted, and it is not
--- retryable — the same statement over the same data would run just as long.
--- `busy` remains the one class the kernel calls worth asking about again.
local ERROR_KINDS = { "busy", "storage", "corruption", "closed", "validation", "unsupported", "timeout" }

--- A raised kernel failure, read back as data (`knl.error(e)`, §9-r).
---
--- `kind` and `method` are optional because a raise that carried no
--- attribution — a Lua-side `error("...")`, a message from another module,
--- or any raise at all in a VM with no bridge — is reported whole rather
--- than rejected: `message` then holds the entire text. So `message` is the
--- field a reader can always count on, and `kind` is the one it must ask
--- for.
---
--- `retryable` is the only judgement in the table and it is the kernel's:
--- true for contention (`busy`) and nothing else. What to *do* about it is
--- the caller's loop's — see `Outcome.err`.
local ERROR = T.shape({
    kind = T.one_of(ERROR_KINDS):is_optional(),
    method = T.string:is_optional(),
    retryable = T.boolean,
    message = T.string,
    -- The suppressed failure when two happened at once (a call that failed
    -- and then a note that could not be recorded): the winner is the record
    -- the kernel could not lay down, the cause is what the beat was trying
    -- to write about.
    cause = T.string:is_optional(),
})

--- What a caller asks for beyond the SQL itself (view-design.md §2).
---
--- `sessions` is the set `$sessions` expands to — the streams this read
--- spans, which is how one statement reads a session tree or a set of
--- sessions that were split and are being read back together. Omitted, it is
--- the session's own stream and nothing else. The kernel expands the token
--- into one bound placeholder per id and binds them; whether the caller may
--- read those streams is not the kernel's judgement (decision 3: identity
--- lives outside the kernel).
---
--- `timeout_ms` and `limit` are whole numbers with kernel defaults (5000 ms,
--- 1000 rows). lshape has no integer prim, so the whole-number expectation
--- rides in this doc, like `budget_grant`'s `amount`.
---
--- Closed: an option the kernel does not know must not quietly do nothing.
local QUERY_OPTS = T.shape({
    sessions = T.array_of(T.string):is_optional(),
    timeout_ms = T.number:is_optional(),
    limit = T.number:is_optional(),
}, { open = false })

--- The read schema, as data: the kernel's table and its columns, published
--- as the contract a caller writes SQL against (view-design.md §3 decision
--- 4, persistence-design.md §3.2).
---
--- This is plain data rather than an lshape schema on purpose — it describes
--- a SQL table, not a Lua value, and what it is FOR is to be compared:
--- `knl.api().schema` answers the same declaration from the kernel's side
--- and `tests/fixtures/knl_beat_test.lua` (inv11) holds the two against each
--- other, exactly as inv10 does for the syscall registries. Adding a column
--- is compatible; renaming or dropping one is a breaking change on the same
--- footing as changing a stored event's shape.
---
--- The event itself is the whole JSON object under `payload` — every field a
--- caller wrote, `beat` included — so a view reaches one with
--- `json_extract(payload, '$.beat')`. `kind` is the one payload field that is
--- also a column (it is what the store indexes on), and reading it from the
--- column rather than out of the JSON is what makes a kind-filtered view cost
--- the size of the filter.
---
--- The column is `payload`, which is what the table declares and therefore
--- what the kernel publishes — it reads its own answer back with `PRAGMA
--- table_info` [実測: sqlite_store.rs `SqliteEventStore::schema`], so the name
--- here is the table's, not a doc's. (persistence-design.md §3.2 writes it as
--- `payload_json` in prose; the implementation never used that spelling, and
--- the table is the contract.)
local EVENTS_SCHEMA = {
    table = "events",
    columns = {
        { name = "stream", type = "TEXT", pk = true },
        { name = "seq", type = "INTEGER", pk = true },
        { name = "epoch_ms", type = "INTEGER", pk = false },
        { name = "kind", type = "TEXT", pk = false },
        { name = "schema_version", type = "INTEGER", pk = false },
        { name = "payload", type = "TEXT", pk = false },
    },
}

--- The contracts this module holds itself to, as data.
M.shapes = {
    outcome = OUTCOME,
    error = ERROR,
    query_opts = QUERY_OPTS,
    schema = EVENTS_SCHEMA,
    -- The vocabulary as a list, next to the shape that closes on it: a
    -- caller enumerating the classes reads the same constant the shape does.
    error_kinds = ERROR_KINDS,
    request = REQUEST,
    event_base = EVENT_BASE,
    device_config = DEVICE_CONFIG,
    tool_entry = TOOL_ENTRY,
    tool_policy_decision = TOOL_POLICY_DECISION,
    cost_result = COST_RESULT,
    llm_result = LLM_RESULT,
    llm_usage = USAGE,
    tool_use_block = TOOL_USE_BLOCK,
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
--- `args` is an ordered list, one item per positional argument, each
--- `{ shape = <lshape schema>, desc = <word for it> }` — and an empty list
--- for an export that takes nothing (or is not a function at all). One
--- representation, no exceptions: an argument a schema cannot pin down gets
--- the widest shape that is still true (`T.any`, or the union a session
--- handle can be) and carries the rest of the meaning in `desc`, rather than
--- dropping out of the machine-readable half into prose. That is what lets
--- the registry be RUN — see the dev-mode gate at the foot of this file,
--- which holds every declared call to the entry above it.
---
--- `returns` stays a shape or a sentence: nothing executes it, because what
--- an export answers is already checked where it is built (`emit`, the
--- Mapper's boundary assert) rather than at the call.
---
--- `members` names the functions of an export that is itself a namespace
--- (`Outcome`), and the methods a returned value carries (`device:with`).
--- The gate reaches the first — they are functions on an exported table —
--- and not the second: `with` is reached off a device, which is a value this
--- module hands out rather than an export it owns, so its entry documents
--- and is walked, but nothing wraps it.
---
--- This registry covers the Lua module. The bridge declares its own surface
--- through `knl.api()` (SESSION_API / MODULE_API in bridge/knl.rs), and
--- `M.shapes.session` / `M.shapes.module` below describe that surface from
--- this side; `tests/fixtures/knl_beat_test.lua` (inv10, runs with the
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
    -- The SQL read (view-design.md §2). The statement is the caller's and
    -- the kernel touches two things in it: it refuses anything that is not
    -- one SELECT / WITH, and it expands `$sessions` into one bound
    -- placeholder per id. Values are bound, never interpolated — `$stream`
    -- is the session's own id, `?` / `:name` are the caller's own.
    query = {
        args = { "string sql (one SELECT / WITH; anything else raises validation)", "table params?", QUERY_OPTS },
        returns = "rows, truncated — { { col = value }, ... } and whether the row limit cut them off",
    },
    reserve = { args = { "integer n >= 0" }, returns = "true | false, tag — decided inside the store" },
    -- The write is the whole result: spend records the deduction and
    -- answers nothing. What is left is a separate reading (`remaining`), so
    -- a caller cannot mistake one call's return for a balance it can hold.
    spend = { args = { "integer n >= 0" }, returns = "nothing" },
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
    error = { args = { "string err" }, returns = ERROR },
    api = {
        args = "none",
        returns = "{ session = { {name, doc}... }, module = { {name, doc}... }, errors = { kind... } }",
    },
}

--- One declared argument: the shape it is held to, and the word for it.
--- Two fields rather than one so the widest-true shape (`T.any`, a union)
--- costs no meaning — `desc` keeps saying "session" where the schema has
--- stopped being able to.
local function arg_of(schema, desc)
    return { shape = schema, desc = desc }
end

--- The arguments every predefined view takes: the session whose store is
--- read, and the query options (the set of streams above all) passed
--- straight through to `session:query`.
local function view_args()
    return { arg_of(SESSION_HANDLE, "session"), arg_of(QUERY_OPTS, "opts?") }
end

--- The predefined views, declared (view-design.md §3 decision 8).
---
--- One table, published twice: as `knl.shapes.views` (the registry a caller
--- reads) and as the `members` of the `views` entry below (what the dev-mode
--- gate walks and what `api_spec` holds `knl.views` against). Two names for
--- one table rather than two tables, so a view cannot be declared in one
--- place and missed in the other.
local VIEWS = {
    beats = {
        args = view_args(),
        returns = "{ { beat, seq_from, seq_to, kinds }, ... }, truncated — one row per beat, in first-seq order",
    },
    tool_pairs = {
        args = view_args(),
        returns = "{ { beat, call_id, name, ok }, ... }, truncated — one row per answered tool call",
    },
    ledger = {
        args = view_args(),
        returns = "{ { seq, kind, amount, tag }, ... }, truncated — the budget_* events in seq order",
    },
}

M.shapes.views = VIEWS

M.shapes.api = {
    open = {
        args = { arg_of(OPEN_OPTS, "opts") },
        returns = "session (the kernel's userdata, unwrapped)",
    },
    resume = {
        args = { arg_of(RESUME_OPTS, "opts") },
        returns = "session (pre-loaded with the persisted log)",
    },
    session = {
        args = {
            arg_of(T.any_of({ OPEN_OPTS, RESUME_OPTS }), "open_opts | resume_opts"),
            arg_of(FUNCTION, "fn(session)"),
        },
        returns = "whatever fn returned (the session is closed either way)",
    },
    device = {
        args = { arg_of(DEVICE_CONFIG, "config") },
        returns = "device (frozen; d:with derives another)",
        members = {
            -- Reached off a device value, not off this module: declared and
            -- walked, never wrapped (see the registry's header).
            with = {
                args = { arg_of(T.table, "the device (d:with, a method call)"), arg_of(DEVICE_CONFIG, "delta") },
                returns = "device' (a new frozen device; the original is untouched)",
            },
        },
    },
    beat = {
        args = { arg_of(SESSION_HANDLE, "session"), arg_of(T.table, "device") },
        returns = OUTCOME,
    },
    fold = {
        args = { arg_of(T.array_of(EVENT_BASE), "events"), arg_of(T.table, "device (read for system / tools)") },
        returns = REQUEST,
    },
    new_beat_id = {
        args = {},
        returns = "string (time-ordered, session-free)",
    },
    Outcome = {
        -- A namespace table, not a function: nothing to hold, and the
        -- members below are what the gate reaches.
        args = {},
        returns = "the Outcome constructors, predicates and match",
        members = {
            ok = { args = { arg_of(T.table, "out") }, returns = OUTCOME },
            refused = { args = { arg_of(T.string, "reason"), arg_of(T.table, "detail?") }, returns = OUTCOME },
            err = { args = { arg_of(OUTCOME_KIND, "kind"), arg_of(T.any, "detail") }, returns = OUTCOME },
            stopped = { args = { arg_of(T.string, "reason"), arg_of(T.string, "tag?") }, returns = OUTCOME },
            is_ok = { args = { arg_of(T.any, "value") }, returns = "boolean" },
            is_refused = { args = { arg_of(T.any, "value") }, returns = "boolean" },
            is_error = { args = { arg_of(T.any, "value") }, returns = "boolean" },
            is_stopped = { args = { arg_of(T.any, "value") }, returns = "boolean" },
            match = {
                args = { arg_of(OUTCOME, "outcome"), arg_of(T.table, "arms { ok, refused, error, stopped }") },
                returns = "whatever the taken arm returned",
            },
        },
    },
    views = {
        -- A namespace table like `Outcome`: nothing to hold here, and the
        -- members are the views themselves — which is what the gate wraps
        -- and what `api_spec` walks.
        args = {},
        returns = "the predefined views: fn(session, opts?) -> rows",
        members = VIEWS,
    },
    shapes = {
        args = {},
        returns = "this registry: every shape above, plus `api`, `session`, `module` and `views`",
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
---
--- `OUTCOME` leaves `detail` as `any` — three of the four error kinds carry
--- a message a person reads — but a "state" detail is a contract: it is the
--- kernel's failure read back, and a loop decides on a retry from its
--- `retryable`. So that one is held to `ERROR` here, which is where a call
--- site that reported a raw string instead of the reading goes red.
local function emit(o)
    shape.assert_dev(o, OUTCOME, "knl_outcome")
    if o.status == "error" and o.kind == "state" then
        shape.assert_dev(o.detail, ERROR, "knl_state_detail")
    end
    return o
end

--- `xpcall`'s message handler for a function the CALLER supplied — fold, a
--- filter, cost, the llm — keeping the raised value and the stack apart.
---
--- `debug.traceback` on its own folds the two into one string, and the
--- message is what a beat reports in prod: a detail that grew a stack
--- traceback behind it would be a different report, not a richer one. So
--- the raise is kept verbatim and the stack rides beside it.
---
--- The stack is captured in dev mode only. It is a development aid — which
--- line of somebody's own fold raised, through the pcall that turned the
--- raise into an Outcome — and a production run should not pay for one on
--- every failure.
---
--- `traced` marks the capture rather than letting the traceback's presence
--- stand for it: the `debug` library is not in every VM this module runs in
--- (mlua's safe stdlib leaves it out), and a detail whose TYPE depended on
--- that would make dev mode mean two different things. Dev mode is one
--- thing — a structured detail — and the stack is a field in it that a VM
--- without `debug` simply cannot fill.
local function traced(raised)
    if shape.is_dev_mode() then
        -- Level 2: skip this handler, so the top frame is the raise itself.
        local tb
        if type(debug) == "table" and type(debug.traceback) == "function" then
            tb = debug.traceback("", 2)
        end
        return { raised = raised, traced = true, traceback = tb }
    end
    return { raised = raised }
end

--- The `detail` for a failure that came out of a caller-supplied function:
--- the message beat reports, and in dev mode the traceback beside it.
---
--- In prod this is exactly the string it has always been. In dev it is a
--- table with the same text under `message`, which is the one place the
--- two modes differ in what a beat returns — deliberately, because a
--- traceback is an answer to "where in my code", a question only the person
--- writing that code is asking.
---
--- @param message string  what beat reports about the failure
--- @param caught table  what `traced` handed back
--- @return string|table detail
local function raised_detail(message, caught)
    if type(caught) == "table" and caught.traced then
        return { message = message, traceback = caught.traceback }
    end
    return message
end

--- What a `traced` failure raised, as text.
local function raised_text(caught)
    if type(caught) == "table" then
        return tostring(caught.raised)
    end
    return tostring(caught)
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
--- is warned about rather than raised. On the clean path a failing close
--- raises instead, since a bracket
--- that reports success with no boundary recorded is the one outcome this
--- exists to rule out.
---
--- When both fail, the body error wins and the close failure is the
--- suppressed one (§9-f, try-with-resources' suppressed exception). It is
--- not silent either: it goes to the host `log` global as a warning when
--- the VM has one, as a record — `{ event =
--- "close_failed_after_body_error", body = <the winner, as text>, close =
--- <the kernel's reading, knl.shapes.error> }` — so the loser is at least
--- structured. It cannot be raised (that would replace the body's error)
--- and it cannot be returned (this path does not return), which is why a
--- log is the only place left for it.
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
            -- The suppressed failure, as a record rather than a sentence:
            -- which of the two errors this is, the body error that beat it,
            -- and the kernel's reading of the close itself (so a reader can
            -- tell a store that was busy from one that is gone).
            local warning = {
                event = "close_failed_after_body_error",
                body = tostring(returned[2]),
                close = read_error(cerr),
            }
            -- The host's `log.warn` takes a string [実測: bridge/log.rs:37,
            -- `msg: String`], so the structured form is offered first and
            -- the text is the fallback. Both are pcall'd: a warning about a
            -- failure must not become one.
            local warned = pcall(host_log.warn, warning)
            if not warned then
                pcall(
                    host_log.warn,
                    "knl.session: "
                        .. warning.event
                        .. ": body="
                        .. warning.body
                        .. "; close="
                        .. tostring(warning.close.message)
                )
            end
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

--- The session surface, as names: every method `knl.shapes.session`
--- declares that a caller can reach (`__close` is the metamethod, reached by
--- the language and not by a call).
local SESSION_METHODS = {
    "id",
    "scope_id",
    "owner",
    "append",
    "events",
    "reserve",
    "spend",
    "remaining",
    "exhausted",
    "close",
}

--- Whether `s` is a session handle.
---
--- Duck-typing is not a preference here, it is the only test available: the
--- real handle is Rust userdata whose metatable mlua protects, so
--- `getmetatable` answers a boolean rather than the table, and there is no
--- name to compare against from Lua. What is left is to ask the value what
--- it can do.
---
--- So it is asked for the WHOLE declared surface, not the three methods a
--- beat happens to call. A stand-in that answers `append` / `reserve` /
--- `events` and nothing else is not a session — it is a table that would
--- pass the gate and then fail somewhere further in, on a `remaining()` or
--- a `close()` a caller had every right to make. Widening the check is what
--- keeps a spec's fake honest to the surface it stands in for.
local function is_session(s)
    local t = type(s)
    if t ~= "table" and t ~= "userdata" then
        return false
    end
    for _, method in ipairs(SESSION_METHODS) do
        local ok, value = pcall(function()
            return s[method]
        end)
        if not ok or not callable(value) then
            return false
        end
    end
    return true
end

--- Append an event this beat is writing, through the dev-mode contract.
local function record(session, ev)
    return session:append(assert_event_dev(ev))
end

--- What is wrong with an llm's answer, or nil when nothing is.
---
--- `device.llm` promises one of two things: an `llm_result` (`knl.shapes`),
--- or `nil` and an error. The result has exactly two statuses, a `usage` the
--- adapter has already normalized into three counts, and — on a refusal —
--- the `refusal.kind` that says what refused. An answer that keeps none of
--- that is a broken adapter, and beat ends the beat the way any other failed
--- call ends rather than filling the gaps in: a defaulted usage would put a
--- count nobody reported into the history, and a refusal reported as
--- "refused" would be beat naming a reason it was never given.
---
--- @param resp any  whatever `device.llm` answered
--- @return string|nil  the violation, or nil when the answer is well formed
local function llm_contract_violation(resp)
    if type(resp) ~= "table" then
        if resp == nil then
            return "llm answered neither a result nor an error"
        end
        return "llm returned " .. type(resp) .. " (the contract is a table, or nil and an error)"
    end
    if resp.status ~= "ok" and resp.status ~= "refused" then
        return "llm answered status " .. tostring(resp.status) .. ' (llm_result is "ok" or "refused")'
    end
    if type(resp.usage) ~= "table" then
        return "llm answered a " .. type(resp.usage) .. " usage (llm_result promises the three counts)"
    end
    if resp.status == "refused" and type(resp.refusal) ~= "table" then
        return "llm refused without a refusal (llm_result promises refusal.kind on a refusal)"
    end
    if resp.status == "refused" and type(resp.refusal.kind) ~= "string" then
        return "llm refused without a refusal.kind (llm_result promises the class that refused)"
    end
    return nil
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
--- The handler and the policy are the caller's code, like fold and the llm,
--- but their raises are not traced: a raising handler or policy is closed
--- as DATA — the `result` of a tool_result, a durable record a later fold
--- reads back and sends to the model — and a record that carried a stack in
--- dev and not in prod would be two different histories. The traceback is
--- attached where a failure is *reported* (an Outcome the caller reads and
--- drops), never where one is *recorded*.
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
        -- Read, not repaired. A `tool_use` block names its call id and its
        -- tool (`knl.shapes.tool_use_block`); a block that named neither is
        -- the llm breaking that contract, and standing an empty string in
        -- for the missing name would turn it into a tool_result about a
        -- tool called "" — a fabricated fact in a durable record.
        local call_id = block.id
        local name = block.name
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

        -- The empty result, defined: a tool_result carries a `result`, and a
        -- handler that answered nothing answered the empty string. This is
        -- the rule, not a stand-in for a missing value — "the tool ran and
        -- produced no output" is a real outcome, and it is what a later fold
        -- sends back to the model.
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
--- Every syscall it makes is pcall'd and every failure comes back as an
--- Outcome — `err("state")`, carrying the kernel's own reading of the raise
--- (`detail.kind` / `detail.retryable`, `knl.shapes.error`). beat does NOT
--- act on `retryable`: a beat that quietly repeated a `busy` reserve would
--- be a loop nobody wrote and nobody bounded, and it would make a second
--- attempt at a call the caller may no longer want. Asking again is the
--- caller's loop's decision, and this is the value it decides from.
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
        -- The kernel's own reading of its own failure: `events` holds the
        -- raise on this path, and what a caller needs off it (which class,
        -- whether asking again is worth anything) is in the table, not in
        -- a sentence somebody would have to parse.
        return emit(Outcome.err("state", read_error(events)))
    end
    -- The fold is the caller's code, so its failures are traced (dev mode).
    local folded_ok, request = xpcall(device.fold, traced, events, device)
    if not folded_ok then
        return emit(Outcome.err("conf", raised_detail("fold failed: " .. raised_text(request), request)))
    end

    -- [2] filter chain (fn(req) -> req) -----------------------------------
    for i, filter in ipairs(device.filters) do
        local filtered_ok, filtered = xpcall(filter, traced, request)
        if not filtered_ok then
            return emit(Outcome.err("filter", raised_detail(raised_text(filtered), filtered)))
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
    local est_ok, amount = xpcall(device.cost, traced, request)
    if not est_ok then
        return emit(Outcome.err("conf", raised_detail("cost failed: " .. raised_text(amount), amount)))
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
        -- `granted` holds the raise here; `busy` is the class a loop may act on.
        return emit(Outcome.err("state", read_error(granted)))
    end
    if not granted then
        return emit(Outcome.stopped("budget", tag))
    end

    -- [4] record the request write-ahead (open kind "llm_request") --------
    -- The request as actually sent is a fact in the history before the call,
    -- so a call that then fails leaves the llm_request event behind.  An append
    -- can fail (closed session, an unavailable store, validation) — beat's
    -- contract is an Outcome, so a state failure is Error("state"), never
    -- a raw raise.
    local rec_ok, rec_err = pcall(record, session, {
        kind = "llm_request",
        beat = beat_id,
        request = request,
    })
    if not rec_ok then
        return emit(Outcome.err("state", read_error(rec_err)))
    end

    -- [5] beat calls the llm directly ------------------------------------
    -- resp = an `llm_result`: { status = "ok"|"refused", content, usage,
    -- stop_reason?, refusal? }.  The status is the adapter's judgement; beat
    -- reads it, it does not invent one (status is llm-supplied).  The call is
    -- caught: an adapter that raises (instead of returning nil, err) is still
    -- a call failure, not an escape from the Outcome contract — and it is the
    -- caller's own code, so the raise is traced (dev mode).
    local call_ok, resp, berr = xpcall(device.llm, traced, request)
    local raised
    if not call_ok then
        raised = resp
        berr = "llm raised: " .. raised_text(raised)
        resp = nil
    end

    -- [6] the answer, held to its contract --------------------------------
    -- One ending for every way the call did not come off: a transport or
    -- provider failure (`nil, err` — its own message is the reason), a raise,
    -- or an answer that is not an `llm_result`. Note it and stop.  The
    -- failure note is best-effort (the state may be what failed); the call
    -- error stays the primary detail either way.
    --
    -- The contract is judged rather than worked around. There is no third
    -- status to read (`llm_result` has two, and a failure is `nil, err`), no
    -- usage to default and no refusal reason to invent: an adapter that broke
    -- its promise has produced no response, and this is where that is said.
    local reason
    if type(resp) ~= "table" then
        reason = berr or llm_contract_violation(resp)
    else
        reason = llm_contract_violation(resp)
    end
    if reason ~= nil then
        local noted_ok, note_err = pcall(record, session, {
            kind = "llm_call_failed",
            beat = beat_id,
            error = tostring(reason),
        })
        if not noted_ok then
            -- Two failures, one Outcome. The state is the one reported:
            -- the call failing is a fact this beat could not write down,
            -- and a caller that cannot write cannot go on either way. The
            -- kernel's reading of the append is the detail (so a loop can
            -- still tell contention from a dead store), and the call's own
            -- reason — the note that did not land — rides along as `cause`
            -- (the suppressed failure, not the winner).
            local detail = read_error(note_err)
            detail.cause = tostring(reason)
            return emit(Outcome.err("state", detail))
        end
        return emit(Outcome.err("call", raised_detail(tostring(reason), raised)))
    end
    -- ok / refused: the model answered. Appending the llm_response is what
    -- records it, under this beat's id, and the budget does not move (the
    -- quota was settled at [3]).  The counts go in as they came: the adapter
    -- normalized them to three numbers on its way out, so there is nothing
    -- here to default and nothing to invent.
    local resp_ok, resp_err = pcall(record, session, {
        kind = "llm_response",
        beat = beat_id,
        content = resp.content,
        usage = resp.usage,
        stop_reason = resp.stop_reason,
    })
    if not resp_ok then
        return emit(Outcome.err("state", read_error(resp_err)))
    end
    resp.beat = beat_id
    -- A refusal is a recorded response the model would not build on — and
    -- the beat that produced it reserved its amount like any other. What
    -- refused is the adapter's classification (`refusal.kind`: the model
    -- itself, or a provider's filter), which is the one place that judgement
    -- is made; `stop_reason` is the provider's own word for the same moment
    -- and beat does not translate one into the other.
    if resp.status == "refused" then
        return emit(Outcome.refused(resp.refusal.kind, resp))
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
        -- Only an append raises out of execute_tools (a handler's or a
        -- policy's raise is closed as data inside it), so this is the
        -- kernel's failure and is read like any other.
        return emit(Outcome.err("state", read_error(summary)))
    end
    if policy_problem then
        return emit(Outcome.err("conf", policy_problem))
    end
    resp.tools = summary

    return emit(Outcome.ok(resp))
end

-- An internal exposed for the spec, which drives it directly.
M._execute_tools = execute_tools

-- ============================================================
-- views — the predefined reads (view-design.md §2)
-- ============================================================
--
-- A view is a named function that runs one SELECT. That is the whole of the
-- mechanism: no builder, no query object, no registration hook. The kernel
-- publishes the table (`knl.shapes.schema`) and a caller writes SQL against
-- it, and these three are the ones the kernel ships because they read what
-- the kernel itself wrote — the beat grouping it stamps, the tool pairs it
-- closes, the ledger it keeps. A consumer's own view is a function of
-- exactly this form in exactly this way (`local function tool_error_rate(s)
-- return s:query([[...]]) end`); nothing here is privileged.
--
-- They do not duplicate the built-in views. `events` / `usage` / `tail` stay
-- the kernel's own (d3-input rev 2), and `usage` in particular is not
-- rewritten as SQL here.
--
-- Two rules hold for every statement below and for a consumer's own:
--
--   * the streams being read are named by `$sessions`, never spliced in.
--     The kernel expands the token into one bound placeholder per id, so an
--     id is a value like any other and `opts.sessions` reads a set of
--     sessions with the same SQL that reads one;
--   * no value is concatenated into the text. What is written into these
--     statements is column names, the kernel's own kind vocabulary, and
--     nothing that came from a caller.
--
-- `beat` is not a column. It is a field of the stored event, which lives
-- whole in the `payload` column, so every view reaches it with
-- `json_extract(payload, '$.beat')` [実測: sqlite_store.rs `insert_row`
-- writes the entire event object into the payload column; event.rs
-- `FIELD_BEAT` is a top-level field of that object].

--- One row per beat: where it starts, where it ends, and what it wrote.
---
--- `kinds` is the beat's events in `seq` order, comma-joined. The order
--- comes from the ordered subquery rather than from `group_concat(kind ORDER
--- BY seq)`: the aggregate's own ORDER BY needs SQLite 3.44, and SQLite may
--- not flatten a subquery with an ORDER BY into an aggregating outer query,
--- so the rows reach the aggregate in the order the subquery put them.
---
--- Events with no `beat` — the session's own boundaries, the ledger, a
--- caller's seed message — are not part of any beat and are left out.
local BEATS_SQL = [[
SELECT beat,
       MIN(seq)           AS seq_from,
       MAX(seq)           AS seq_to,
       group_concat(kind) AS kinds
  FROM (SELECT json_extract(payload, '$.beat') AS beat,
               stream,
               seq,
               kind
          FROM events
         WHERE stream IN $sessions
           AND json_extract(payload, '$.beat') IS NOT NULL
         ORDER BY stream, seq)
 GROUP BY beat
 ORDER BY seq_from, beat
]]

--- The tool pairs: a `tool_call` and the `tool_result` that answered it,
--- joined on the call id within one stream.
---
--- A call id is unique to the stream that minted it, so the join carries
--- `r.stream = c.stream` — without it a set of sessions read together could
--- pair one session's call with another's result.
---
--- A call with no result is not a pair and does not appear. That is the
--- point of the view: what it lists is the calls that were answered, and a
--- call left open by a run that died mid-tool is visible as its absence
--- (`beats` still shows the `tool_call` in its `kinds`).
local TOOL_PAIRS_SQL = [[
SELECT json_extract(c.payload, '$.beat')    AS beat,
       json_extract(c.payload, '$.call_id') AS call_id,
       json_extract(c.payload, '$.name')    AS name,
       json_extract(r.payload, '$.ok')      AS ok
  FROM events AS c
  JOIN events AS r
    ON r.stream = c.stream
   AND r.kind = 'tool_result'
   AND json_extract(r.payload, '$.call_id') = json_extract(c.payload, '$.call_id')
 WHERE c.stream IN $sessions
   AND c.kind = 'tool_call'
 ORDER BY c.stream, c.seq
]]

--- The budget ledger: every `budget_*` event in order, with the amount and
--- the grant's tag read out of the payload.
---
--- The four kinds are named rather than matched with a `LIKE 'budget_%'`:
--- they are the closed vocabulary the balance is a fold of (event.rs), and
--- `kind` is an indexed column, so naming them keeps the read the size of
--- the ledger rather than the size of the stream.
local LEDGER_SQL = [[
SELECT seq,
       kind,
       json_extract(payload, '$.amount') AS amount,
       json_extract(payload, '$.tag')    AS tag
  FROM events
 WHERE stream IN $sessions
   AND kind IN ('budget_granted', 'budget_reserved', 'budget_refused', 'budget_spent')
 ORDER BY stream, seq
]]

--- Run one view's statement over `session`.
---
--- The options are the caller's, passed through untouched: `sessions` is
--- what makes a view span a set of streams, and `timeout_ms` / `limit` are
--- the same knobs any other read has. No view takes parameters of its own —
--- the only values any of them binds are the stream ids the kernel resolves
--- from `$sessions`.
---
--- `truncated` is handed back beside the rows rather than dropped: a view
--- that had more rows than the limit allowed has said so, and swallowing
--- that would leave a caller to guess from a suspiciously round count.
local function read_view(session, sql, opts)
    return session:query(sql, nil, opts)
end

--- Whether a stored `ok` is true.
---
--- SQLite has no boolean: `json_extract` answers 1 / 0 for a JSON true /
--- false, and that is what crosses the bridge. The view declares an `ok`, so
--- the reading back into a boolean happens here — once, in the layer that
--- promised it — rather than in every caller.
local function truthy(value)
    return value == true or value == 1
end

M.views = {}

--- One row per beat: `{ beat, seq_from, seq_to, kinds }`.
---
--- @param session userdata|table  a knl session
--- @param opts table|nil  query opts (`sessions` to span a set of streams)
--- @return table rows
--- @return boolean truncated
function M.views.beats(session, opts)
    return read_view(session, BEATS_SQL, opts)
end

--- One row per answered tool call: `{ beat, call_id, name, ok }`.
---
--- The rows are rebuilt rather than edited in place, so reading a view never
--- writes to a table the caller can still be holding.
---
--- @param session userdata|table  a knl session
--- @param opts table|nil  query opts (`sessions` to span a set of streams)
--- @return table rows
--- @return boolean truncated
function M.views.tool_pairs(session, opts)
    local rows, truncated = read_view(session, TOOL_PAIRS_SQL, opts)
    local out = {}
    for i, row in ipairs(rows) do
        out[i] = {
            beat = row.beat,
            call_id = row.call_id,
            name = row.name,
            ok = truthy(row.ok),
        }
    end
    return out, truncated
end

--- The budget ledger: `{ seq, kind, amount, tag }` in seq order.
---
--- @param session userdata|table  a knl session
--- @param opts table|nil  query opts (`sessions` to span a set of streams)
--- @return table rows
--- @return boolean truncated
function M.views.ledger(session, opts)
    return read_view(session, LEDGER_SQL, opts)
end

-- ============================================================
-- The registry, executed (session-device-design.md §9-m)
-- ============================================================
--
-- `M.shapes.api` names the shape of every argument of every export. A
-- registry nobody runs is prose with a table around it — the entry drifts
-- from the function and nothing goes red — so in dev mode each declared
-- export is replaced, once, here at load, by a wrapper that holds the call
-- to its entry before letting it through. Prod is untouched: the exports
-- are the functions themselves and a call pays nothing, which is why this
-- is a gate and not the argument checking a function needs to be correct.
-- The checks a call must not get through WITHOUT (a device's config, a
-- filter's return, cost's bound) stay where they are, loud in both modes.
--
-- What the gate judges is the shape of the arguments that were PASSED. An
-- argument that is absent — not supplied, or supplied as nil — is left to
-- the function: which arguments are required, and what a missing one means,
-- is its own business, and `knl.beat(s, nil)` must go on answering
-- `Outcome.err("conf")` rather than raising. The registry answers for what
-- it was given.

--- Wrap `fn` so every argument it is passed is held to `declared[i].shape`
--- (dev mode only — `assert_dev` is a no-op otherwise, and this wrapper is
--- not even installed in prod).
---
--- The hint names the registry entry and the position, so a violation reads
--- as a broken call to a declared API rather than as an anonymous shape
--- failure somewhere inside the module.
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
            M[name] = arg_checked("knl." .. name, export, entry.args)
        elseif type(export) == "table" and type(entry.members) == "table" then
            -- A namespace (`Outcome`): its members are exports too. A
            -- member of something this module does not export as a table
            -- (`device:with`) has no function here to replace, and is
            -- skipped rather than hunted down.
            for member, member_entry in pairs(entry.members) do
                if type(export[member]) == "function" then
                    export[member] = arg_checked("knl." .. name .. "." .. member, export[member], member_entry.args)
                end
            end
        end
    end
end

return M
