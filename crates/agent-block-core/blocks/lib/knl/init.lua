--- knl — the Lua kernel: session + device, the beat, Outcome, shapes, views.
---
--- What this is
---   The driving half of the kernel/shell split. Rust is the pure syscall
---   layer (session / append / events / view / query / reserve / spend /
---   close) and states the kernel's own invariants in its module doc; this
---   module runs a BEAT — one model call plus the tools that call asks for —
---   over that layer and hands back an `Outcome`. There is no loop here: a
---   caller composes beats on the spot, which is why the primitive is one
---   beat and not a run.
---
--- Two arguments, two owners
---   `knl.open{ owner?, budget?, store? }` hands back the kernel's session
---   userdata verbatim — the durable half: the fact-log and the quota, owned
---   by the kernel, advanced only by appending. `knl.device{ llm?, tools?,
---   tool_policy?, fold?, filters?, system?, cost? }` builds the policy half
---   — a stateless value whose defaults are resolved once at construction
---   and then frozen. `knl.beat(session, device)` takes both. They are not
---   bundled into one handle because they differ in owner (kernel / caller),
---   lifetime (durable / per-process) and mutability (append-only / frozen),
---   and the config is CONSUMED at construction rather than carried: a
---   device holds resolved fields, never the table it was configured with.
---   Per-beat policy variation is not an override argument: derive another
---   device with `d:with{ llm = strong }` and beat with that one.
---
--- Lifecycle belongs to the session
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
---   failed on its way out (the suppressed-exception rule, and the loser is
---   logged as a record rather than dropped).
---
--- Beats are declared, not numbered
---   The kernel does not count beats. `knl.beat` mints one id per beat with
---   `knl.new_beat_id()` (time-ordered, session-free) and stamps it on every
---   event that beat writes — llm_request, llm_response, the tool pair, and
---   a failed call's note. The kernel stores a `beat` it is given and asks
---   only that it be a string; grouping and ordering read it back, nothing
---   more. `resp.beat` carries the same id out to the caller.
---
--- The steps of a beat
---   [0]   the gate: a knl session first, a knl device second, an `llm` on
---         the device — any of the three missing is `Outcome.err("conf")`
---   [0.5] the beat names itself (`knl.new_beat_id`)
---   [1]   request = `device.fold(session:events(), device)`
---   [2]   the filter chain, each `fn(request) -> request`
---   [3]   `session:reserve(device.cost(request))` — a refusal stops here,
---         with nothing recorded and no call made
---   [4]   append `llm_request`
---   [5]   `device.llm(request)`
---   [6]   append `llm_response` (or `llm_call_failed`), then settle
---   [7]   the tools that response asked for: `tool_call` / `tool_result`
---
--- Outcome — the four statuses
---   A beat answers with a value, never a raise, and the value is one of
---   four tagged tables. `Outcome.match(o, arms)` is exhaustive: all four
---   arms are required, and an unknown status is a loud error — the
---   dynamic-language stand-in for a compiler's exhaustiveness check.
---
---     ok       the beat ran. `out` is the call's answer (content / usage /
---              stop_reason) plus the `beat` id and the tool summary
---     refused  the model answered and declined to make progress. `reason`
---              is the adapter's provider-neutral classification of the
---              refusal ("model" / "content_filter") and `detail` is the
---              whole answer, so a caller can inspect what came back
---     error    the mechanism failed: the beat did not come off
---     stopped  the beat stopped ON PURPOSE before calling — the quota
---              would not cover it. Nothing broke and the model was never
---              asked; this is the branch a caller's loop exits on, and
---              `tag` names the grant that stopped it
---
---   `Outcome.err(kind, detail)` carries TWO vocabularies and they answer
---   different questions. `kind` names the STAGE that failed — "conf" (the
---   device or a caller function it holds), "filter", "call" (the llm), or
---   "state" (a record that could not be laid down). What the failure WAS,
---   and whether asking again could work, is in `detail`: a "state" failure
---   carries the kernel's own reading, `detail.kind` one of
---   `knl.shapes.error_kinds` and `detail.retryable`. A loop deciding on a
---   retry reads `detail.retryable` and never the stage: "state" says
---   where, not whether.
---
--- What a stored event looks like
---   One envelope, one place for structure. An event is `{ kind, beat?,
---   meta?, data? }` and nothing else at the top level — a stray key is
---   refused, not stored — and the kernel stamps `seq` / `epoch_ms` /
---   `_schema_version` on it and keeps the envelope as columns (stream,
---   seq, epoch_ms, kind, schema_version, beat, meta, data).
---
---   The envelope is the half that does not move. `beat` is the correlation
---   key this layer stamps, `meta` is a SHALLOW map of labels (string /
---   number / boolean values; a nested one is refused), and everything a
---   kind is actually ABOUT is structured JSON under `data`, whose shape
---   belongs to whoever writes that kind. The kernel validates the envelope
---   and the `data` of its own kinds (`session_*`, `budget_*`) and nothing
---   else; the shapes for the kinds a beat writes are declared here, in
---   `knl.shapes.events`, and asserted in dev mode at the append sites.
---
---   The point of the split is where a change can hurt. A view that reads
---   the envelope or `meta` is untouched by a schema change; a view that
---   reads a path inside `data` changes together with the shape of the kind
---   it reads — `knl.views.usage` with `llm_response`, `tool_pairs` with the
---   tool pair, `ledger` with `budget_*`. A caller's seed is a kind like any
---   other and goes in the same envelope:
---
---       s:append{ kind = "msg_user", data = { content = "hi" } }
---       s:append{ kind = "msg_user", meta = { label = "seed" },
---                 data = { content = "hi" } }
---
--- Reading the log back: two tiers, and only two
---   BUILT-IN VIEW — `session:view(name, opts?)`, plus `session:events(from)`
---   beside it. These are the kernel's own reads, they are fixed, and they
---   do not grow: the record from a position on, and `tail` (the last `n`
---   events, verbatim). A fold whose consumer is not fixed in kernel terms
---   never becomes a name here.
---
---   QUERY VIEW — a named Lua function that runs ONE `SELECT`. The log is a
---   SQLite table whose columns the kernel publishes (`knl.shapes.schema`),
---   and `session:query(sql, params?, opts?)` binds values, refuses anything
---   that is not one SELECT / WITH, and resolves `$stream` (this session)
---   and `$sessions` (`opts.sessions`, the set to read across). That is the
---   whole mechanism: no builder, no query object, no registration hook.
---   `knl.views.beats` / `tool_pairs` / `ledger` / `usage` / `tree` are the
---   five this module ships, and a consumer's own view is a function of
---   exactly the same form — nothing about the five is privileged.
---
---   Token usage is a query view and not a built-in one, deliberately.
---   Every `llm_response` carries the counts its adapter normalized out of
---   the provider's answer, so `knl.views.usage` is an accounting of facts
---   already in the log, in the shell's vocabulary rather than the kernel's
---   — and it is a reading apart from the budget, which is a quota the owner
---   granted and never a tally of tokens (see below).
---
--- The budget
---   The budget is a quota the owner granted the session, not a tally of
---   what it used. beat asks for permission BEFORE it calls —
---   `session:reserve(n)`, after the request is known and before anything is
---   recorded — and a refusal is a planned stop, not a failure and not a
---   model decision: `Outcome.stopped("budget", tag)`, with no `llm_request`
---   event and no call. How much a beat asks for is the device's policy:
---   `device.cost(request)`, one unit per beat by default. What a unit
---   *means* is whatever the owner tagged the grant with — the kernel reads
---   the number and nothing else, and token usage (`knl.views.usage`) is a
---   separate reading that beat never folds back into the budget.
---
--- Sessions opened from sessions
---   `knl.open{ parent = s, budget = { from_parent = n } }` opens a session
---   out of `s`'s balance: `s`'s ledger gains a reservation naming the child,
---   the child's log opens with `parent` recorded on it and `n` of its own,
---   and the two are ONE write on the parent's database. A balance that will
---   not cover it records a refusal on the parent and raises `refused` —
---   nothing is opened, and there is no half-opened session to hand back.
---   Nothing comes back when the child closes: an allocation is a spend, the
---   same way a reservation is.
---
---   The kernel keeps no tree. It records two facts — the parent on the
---   child's opening, the child on the parent's ledger entry — and, when a
---   session closes with children that had not ended, their ids on the
---   boundary (`session_closed.data.open_children`, recorded in the same
---   write; a close is never refused for them). Everything else is a
---   supervisor's: `knl.views.tree` is one recursive SELECT over those
---   fields, and a policy over a subtree is a pack above this module rather
---   than a rule inside it.
---
--- What a device promises, function by function
---   Every one of these is the caller's code, and beat holds it to a written
---   contract rather than guessing at a return it does not recognise.
---
---     fold(events, device) -> request
---       pure; the default (`knl.fold`) folds the log into the neutral
---       content-block shape. A raise is `err("conf")`.
---     filter(request) -> request
---       each filter replaces the request wholesale. Returning a non-table
---       is `err("filter")` — loudly, in prod as well as dev, because the
---       value goes into the durable record and onto the wire.
---     cost(request) -> integer >= 1
---       what the beat reserves. Checked in prod, not only asserted in dev:
---       a beat that could ask for zero is how a run stops being finite.
---       The default answers 1, so a grant counts beats.
---     llm(request) -> llm_result | nil, err
---       `knl.shapes.llm_result`: `status` is "ok" or "refused" and there is
---       no third value; `content` is an array of blocks, `usage` is three
---       counts, and a "refused" answer names the refusal's `kind`. A
---       transport or provider failure is `nil, err` (or a raise), which
---       beat records as `llm_call_failed` and reports as `err("call")`.
---     tool_policy(tool_use_block, out) -> decision, reason?
---       `nil` (no opinion — run), `"run"` or `"deny"`, and nothing else: a
---       fourth word is a device-contract violation, not a fourth meaning.
---       A policy that RAISES denies — a gate written to veto tools must not
---       fall open on its own bug — and its message becomes the reason.
---
--- When something raises
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
--- knl.shapes is a registry that is EXECUTED
---   Every public interface of this module is declared as an lshape and
---   published through `knl.shapes`, so a caller reads the contract as data
---   rather than out of prose. `knl.shapes.api` goes one further: it names
---   the shape of every argument of every export, and in dev mode each
---   declared export is wrapped once, at load, by a gate that holds the call
---   to its entry. A registry nobody runs is prose with a table around it.
---   Prod installs no wrapper and pays nothing, which is why the checks a
---   call must not get through WITHOUT — a device's config, a filter's
---   return, cost's bound — stay where they are and stay loud in both modes.
---   `knl/spec/api_spec.lua` closes the loop from the other side: an export
---   with no entry, an entry with no export, and a device field
---   `device_config` does not describe are each a failure.
---
---   Two halves of one table, and they have different owners. Everything
---   above is THIS module's — the shapes of what `knl.beat` / `knl.device` /
---   the views take, executed by the dev gate at the foot of this file.
---   `knl.shapes.session` and `knl.shapes.module` are the BRIDGE's, and they
---   are not written here at all: they point at `knl_types`, generated at
---   host start from the argument and return types of `bridge/knl.rs`. That
---   surface is checked in Rust, on every call and in both modes, so the
---   session userdata needs no wrapper here — which is just as well, since
---   wrapping it would mean handing out a proxy table in place of the
---   kernel's own value, and `local s <close>` and `open{ parent = s }` both
---   want the userdata itself.
---
--- Clean by construction: no legacy, no fabrication
---   This module and `knl_adapter` are kept free of consumer legacy on
---   purpose, because a compat shim taken in here becomes the kernel's
---   vocabulary forever. Nothing is invented on a caller's behalf: an llm
---   answer with no `usage` is `err("call")` rather than `usage or {}`, a
---   refusal without a `kind` is the same rather than a default word, a
---   third llm status does not exist, a nameless `tool_use` block is the
---   provider breaking its contract rather than a hole to fill with an empty
---   string, and no request field (`max_tokens` and friends) is injected
---   behind the caller's back. There are no compat aliases: a tool entry
---   declares `input_schema`, and nothing else is read in its place.
---
--- Deliberately not here
---   Fork (branching a history), scope trees and sub-scope allowances,
---   parallel tool execution, streaming, and a structured error type for a
---   tool_result beyond the raised string. Each is deferred until a real
---   loop asks for it, not designed ahead of the need.

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
--- message }` — the *When something raises* section of the header.
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
            input_schema = spec.input_schema,
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
---   msg_user     -> { role = "user", content = <data.content verbatim> }
---   llm_response -> { role = "assistant", content = <data.content verbatim> }
---   tool_result  -> collected, in seq order, into the user message that
---                   follows the assistant message they answer (consecutive
---                   tool_results batch together, which for a well-formed
---                   history is the same as grouping by beat)
---
--- What a kind is about lives under `data` (see the header), so that is
--- where this reads: `data.content`, `data.call_id`, `data.ok`,
--- `data.result`. An event that carried none — a caller's own kind, or one
--- of the kernel's own boundaries — falls through the same skip as the rest
--- rather than indexing nil.
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
        -- The kind's own content, all of it in one place. An event with no
        -- `data` reads as an empty one, so a malformed record is skipped
        -- like an unknown kind instead of raising inside a pure function.
        local data = type(ev.data) == "table" and ev.data or {}
        if kind == "msg_user" then
            close_dangling()
            flush()
            messages[#messages + 1] = { role = "user", content = data.content }
        elseif kind == "llm_response" then
            close_dangling()
            flush()
            messages[#messages + 1] = { role = "assistant", content = data.content }
            if type(data.content) == "table" then
                for _, block in ipairs(data.content) do
                    if block.type == "tool_use" and block.id ~= nil then
                        pending[#pending + 1] = block.id
                    end
                end
            end
        elseif kind == "tool_result" then
            if data.call_id ~= nil then
                answered[data.call_id] = true
            end
            batch[#batch + 1] = {
                type = "tool_result",
                tool_use_id = data.call_id,
                content = result_text(data.result),
                is_error = (data.ok == false) or nil,
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
-- so a caller reads the contract as data
-- rather than out of prose, and `M.shapes.api` names the shape of
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
-- The event vocabulary this layer writes (msg_user / llm_request /
-- llm_response / llm_call_failed / tool_call / tool_result) is here too,
-- as `knl.shapes.events` — but only the `data` half of each, because that
-- is the half whose owner is the writer (the *stored event* section of the
-- header). There
-- is no second copy of anything: the kernel validates the envelope and the
-- `data` of ITS own kinds (`session_*`, `budget_*`) and stopped judging
-- these, so the shapes below are the only declaration of them, and the
-- appends in `record` are where they are held.

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

--- An event's `meta`: labels, and only labels.
---
--- Shallow by rule — string / number / boolean values and nothing else —
--- because `meta` is the half of the envelope a view can read without ever
--- being broken by a schema change, and a nested value would make it a
--- second `data` with none of that promise. The kernel refuses a nested one
--- at the syscall; this is the same rule as data, so dev mode says it at
--- the line that wrote it rather than at the boundary.
local EVENT_META = T.map_of(T.string, T.any_of({ T.string, T.number, T.boolean }))

--- The envelope, in the terms this layer owns: a `kind` string, the `beat`
--- id stamped on the events a beat wrote (an opaque string and nothing
--- more), the shallow `meta` labels, and the `data` a kind carries.
---
--- Open, and deliberately: the kernel stamps `seq` / `epoch_ms` /
--- `_schema_version` on a stored event, so the value that comes back out of
--- `events()` carries more keys than the one that went in. The closure —
--- "no other top-level key" — is the kernel's, enforced at the syscall
--- where the stamps are known; what this shape holds is the four fields a
--- caller writes.
---
--- What is inside `data` is per-kind and is `EVENT_DATA` below.
local EVENT_BASE = T.shape({
    kind = T.string,
    beat = T.string:is_optional(),
    meta = EVENT_META:is_optional(),
    data = T.table:is_optional(),
})

--- The `data` of every kind this layer writes — its shape, owned here
--- because the writer owns it.
---
--- Closed, every one of them: `data` is what a SQL view reads a path out
--- of, so a key that arrived by accident is a column somebody will
--- eventually select and a key that was renamed is a view that quietly
--- answers NULL. A closed shape is what turns both into a dev-mode
--- failure at the append that wrote it.
---
--- Two fields are deliberately wider than the contract they came from.
--- `llm_response.usage` is `table`, not the strict three counts
--- (`llm_usage`): the counts are the ADAPTER's promise and beat judges
--- them at the call ([6] — an answer with no usage is `err("call")` and no
--- response is recorded), so re-judging them at the append would report a
--- broken adapter as a store failure. `msg_user.content` and
--- `tool_result.result` are `any` because they are provider / tool
--- vocabulary: a string, blocks, or whatever a handler answered.
---
--- `msg_user` is not a kind a beat writes — it is the seed a caller writes
--- (see the header) — and it is declared here with the rest so the form is
--- published rather than remembered.
---
--- Two kinds here are the KERNEL's, not this layer's: `session_opened` and
--- `session_closed`. Nothing in Lua may write them (the kernel refuses a
--- hand-written boundary) and their shape is checked on the other side of
--- the syscall — they are declared here because a supervisor READS them.
--- `parent` and `open_children` are how a session tree is recorded, and a
--- view over them (`knl.views.tree`) is tied to those two paths the same way
--- `usage` is tied to `llm_response`: the declaration is what says which
--- reads move when the shape does.
local EVENT_DATA = {
    session_opened = T.shape({
        scope_id = T.string,
        owner = T.string,
        -- Absent on a root, which is what makes a root a root: there is no
        -- "parent = nil" to tell apart from an unrecorded one.
        parent = T.string:is_optional(),
    }, { open = false }),
    session_closed = T.shape({
        reason = T.string,
        detail = T.string:is_optional(),
        -- The children that had not ended when this one did, as the close
        -- found them in the same write. A record and never a refusal:
        -- the log turns no write away.
        open_children = T.array_of(T.string):is_optional(),
    }, { open = false }),
    msg_user = T.shape({
        content = T.any,
    }, { open = false }),
    llm_request = T.shape({
        request = REQUEST,
    }, { open = false }),
    llm_response = T.shape({
        content = T.array_of(T.table),
        usage = T.table,
        stop_reason = T.string:is_optional(),
    }, { open = false }),
    llm_call_failed = T.shape({
        error = T.string,
    }, { open = false }),
    tool_call = T.shape({
        call_id = T.string,
        name = T.string,
        args = T.any,
    }, { open = false }),
    tool_result = T.shape({
        call_id = T.string,
        ok = T.boolean,
        result = T.any,
    }, { open = false }),
}

--- One entry of a device's `tools` map: what the model may call
--- (description / input_schema, both optional and both provider
--- vocabulary) plus how to call it. Only `input_schema` is read; there is
--- no alias.
local TOOL_ENTRY = T.shape({
    description = T.string:is_optional(),
    input_schema = T.any:is_optional(),
    handler = FUNCTION,
})

--- What a `tool_policy` may decide: `nil`
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

--- What a parent hands a child out of its own balance: `from_parent` units,
--- counted in `tag` (the parent's unit when it is left out).
---
--- Not a grant, and the difference is where the units come from. A grant is
--- an owner allowing — a balance appearing, which only an owner may do — and
--- an allocation is a move: the parent's balance falls by exactly what the
--- child's rises by, in one write. So there is no `desc` either; what the
--- log records about why is the parent it names.
local BUDGET_ALLOCATION = T.shape({
    from_parent = T.number:describe("a whole number of units, out of the parent's balance"),
    tag = T.string:is_optional(),
}, { open = false })

--- `knl.open` opts: state only. Policy has its own constructor.
---
--- `parent` is a session this one is opened *from*: the child lands on the
--- parent's database, its opening names the parent, and its quota is moved
--- out of the parent's balance — one write, both ledgers. It goes with
--- `budget = { from_parent = n }` and with nothing else: an owner's grant on
--- a child would be a quota nobody paid for, and `from_parent` with no parent
--- has nowhere to take it from. The kernel refuses each with the other named.
local OPEN_OPTS = T.shape({
    owner = T.string:is_optional(),
    budget = T.any_of({ BUDGET_GRANT, BUDGET_ALLOCATION }):is_optional(),
    store = T.any:is_optional(),
    parent = SESSION_HANDLE:is_optional(),
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
--- side of the Rust `KnlError::KINDS`.
---
--- One constant, used by the shape below and by the check that holds this
--- declaration against the bridge's own (`knl.api().errors`, compared in
--- `tests/fixtures/knl_beat_test.lua` inv10, the test that has a bridge).
--- Retyping the list beside the shape would make a second place to keep in
--- step, which is the drift the api registry exists to rule out.
---
--- `timeout` is the read side's own class: a
--- `session:query` that ran past its deadline was interrupted, and it is not
--- retryable — the same statement over the same data would run just as long.
--- `busy` remains the one class the kernel calls worth asking about again.
---
--- `refused` is the odd one out and deliberately so: every other class is a
--- fault, and that one is a decision. A child asked for more than its
--- parent's balance covered, the refusal is in the log, and nothing is
--- wrong — so it is not retryable either, because the same balance answers
--- the same way until an owner grants more.
local ERROR_KINDS = { "busy", "storage", "corruption", "closed", "validation", "unsupported", "timeout", "refused" }

--- A raised kernel failure, read back as data (`knl.error(e)`).
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

--- What a caller asks for beyond the SQL itself.
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
--- as the contract a caller writes SQL against.
---
--- This is plain data rather than an lshape schema on purpose — it describes
--- a SQL table, not a Lua value, and what it is FOR is to be compared:
--- `knl.api().schema` answers the same declaration from the kernel's side
--- and `tests/fixtures/knl_beat_test.lua` (inv11) holds the two against each
--- other, exactly as inv10 does for the syscall registries. Adding a column
--- is compatible; renaming or dropping one is a breaking change on the same
--- footing as changing a stored event's shape.
---
--- The envelope IS the column list (see the header). `stream` / `seq` /
--- `epoch_ms` / `kind` / `schema_version` are the kernel's stamps, `beat` is
--- the correlation key a view groups by, `meta` holds the shallow labels and
--- `data` holds the one structured JSON value a kind is about.
---
--- So a view reaches a beat with the `beat` column rather than a JSON path,
--- and the only `json_extract` any of them needs is into `data` — which is
--- exactly the reading that has to change when a kind's shape does. There is
--- no `payload` column any more: the whole-object form it held is what this
--- round split into the envelope and the one structured field.
local EVENTS_SCHEMA = {
    table = "events",
    columns = {
        { name = "stream", type = "TEXT", pk = true },
        { name = "seq", type = "INTEGER", pk = true },
        { name = "epoch_ms", type = "INTEGER", pk = false },
        { name = "kind", type = "TEXT", pk = false },
        { name = "schema_version", type = "INTEGER", pk = false },
        { name = "beat", type = "TEXT", pk = false },
        { name = "meta", type = "TEXT", pk = false },
        { name = "data", type = "TEXT", pk = false },
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
    event_meta = EVENT_META,
    -- The `data` shapes of the kinds this layer writes, by kind, plus the
    -- two kernel boundary kinds a supervisor reads (`session_opened` /
    -- `session_closed`). The envelope is `event_base`; this is the half
    -- whose owner is the writer.
    events = EVENT_DATA,
    budget_allocation = BUDGET_ALLOCATION,
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

--- The API registry: one entry per public
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

--- The bridge's argument and return shapes, from Rust.
---
--- `knl_types` is generated at host start from the argument and return types
--- of the syscall layer (`bridge/knl.rs`, `mod types`) and injected as an
--- embedded module, so the two registries below POINT AT the Rust type
--- instead of restating it. What they replaced was two declarations of one
--- interface — the bridge's signatures, and a hand-written lshape table
--- beside them — held together by a test that compared names and would not
--- have noticed a field renamed on either side.
---
--- A VM without the host has no such module: the pure lspec runner loads this
--- file off the filesystem and never registers the bridge, and there is no
--- session to call there either. The registry then names the type it would
--- have resolved to rather than inventing a shape for it — a description is a
--- declaration (`knl/spec/api_spec.lua`), and a fallback that was a shape
--- would be the second declaration all over again. The VM that does have the
--- bridge is the one that holds the two halves against each other
--- (`tests/fixtures/knl_beat_test.lua`, inv10).
local types_ok, RUST = pcall(require, "knl_types")
if not types_ok then
    RUST = setmetatable({}, {
        __index = function(_, name)
            return "knl_types." .. name .. " (generated by the host; no bridge in this VM)"
        end,
    })
end

M.shapes.rust = RUST

M.shapes.session = {
    id = { args = "none", returns = RUST.SessionId },
    scope_id = { args = "none", returns = RUST.ScopeId },
    owner = { args = "none", returns = RUST.Owner },
    append = { args = { RUST.AppendEvent }, returns = RUST.Seq },
    events = { args = { RUST.Seq }, returns = RUST.EventRows },
    len = { args = "none", returns = RUST.Count },
    -- The one built-in fold beside `events`: the last `n` records, verbatim.
    -- Token usage used to be the second name here and is not one any more —
    -- it is `knl.views.usage`, one SELECT like every other view.
    view = { args = { RUST.ViewName, RUST.ViewOpts }, returns = "table (the named fold)" },
    -- The SQL read. The statement is the caller's and
    -- the kernel touches two things in it: it refuses anything that is not
    -- one SELECT / WITH, and it expands `$sessions` into one bound
    -- placeholder per id. Values are bound, never interpolated — `$stream`
    -- is the session's own id, `?` / `:name` are the caller's own.
    query = {
        args = { RUST.Sql, RUST.QueryParams, RUST.QueryOpts },
        returns = RUST.QueryResult,
    },
    -- A refusal hands back the grant's tag beside the `false`, which a single
    -- shape cannot say: lshape's tuple is fixed-length and has no optional
    -- position, so the second return stays prose.
    reserve = { args = { RUST.Amount }, returns = "true | false, tag — decided inside the store" },
    -- The write is the whole result: spend records the deduction and
    -- answers nothing. What is left is a separate reading (`remaining`), so
    -- a caller cannot mistake one call's return for a balance it can hold.
    spend = { args = { RUST.Amount }, returns = "nothing" },
    remaining = { args = "none", returns = RUST.Remaining },
    exhausted = { args = "none", returns = RUST.Exhausted },
    close = {
        args = { RUST.CloseReason, RUST.CloseDetail },
        returns = "nothing; records session_closed{reason, detail?} once per stream",
    },
    __close = { args = { "session", RUST.Raised }, returns = "nothing; scope_exit or error(+detail)" },
}

M.shapes.module = {
    open = { args = { RUST.OpenOpts }, returns = "session" },
    resume = { args = { RUST.ResumeOpts }, returns = "session" },
    new_beat_id = { args = "none", returns = RUST.BeatId },
    error = { args = { RUST.Raised }, returns = RUST.ErrorTable },
    api = { args = "none", returns = RUST.ApiReport },
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

--- The predefined query views, declared.
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
    usage = {
        args = view_args(),
        returns = "{ { stream, calls, input_tokens, output_tokens, thinking_tokens }, ... }, truncated"
            .. " — one row per stream that answered, the counts the providers reported",
    },
    tree = {
        args = view_args(),
        returns = "{ { session, parent, opened_epoch_ms, closed_epoch_ms?, open_children? }, ... }, truncated"
            .. " — the subtree rooted at this session, discovered from the log;"
            .. " `open_children` is the JSON array the close recorded",
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

--- Dev-mode gate for an event a beat is about to write. No-op in prod.
---
--- Two halves, and they have different owners. The ENVELOPE rules are the
--- ones the kernel also enforces and this layer mirrors so they fail at the
--- line that wrote them rather than at the syscall: a `kind`, a `beat` that
--- is a string when present, and a `meta` that is shallow. The `data` is
--- this layer's own — `knl.shapes.events` holds the shape of each kind it
--- writes, and the kernel stopped judging them — so an unknown kind is
--- simply not checked here, which is what leaves the vocabulary open.
---
--- It guards only what *beat* writes. A caller's own `session:append` goes
--- straight to the kernel: the session handle is the kernel's, not
--- something this module wraps.
local function assert_event_dev(ev)
    shape.assert_dev(ev, EVENT_BASE, "knl_event")
    if type(ev) == "table" then
        local declared = EVENT_DATA[ev.kind]
        if declared ~= nil then
            shape.assert_dev(ev.data, declared, "knl_event data (" .. tostring(ev.kind) .. ")")
        end
    end
    return ev
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
        -- construction cannot reach the device.
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

local OPEN_STATE_KEYS = { owner = true, budget = true, store = true, parent = true }
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
--- With `parent` it opens a CHILD: on the parent's database, out of the
--- parent's balance (`budget = { from_parent = n, tag? }`), and in one write
--- — the child's opening and grant, and the parent's reservation naming it.
--- A balance that will not cover it records a refusal on the parent and
--- raises `refused`; nothing is opened. Nothing comes back when the child
--- closes, because an allocation is a spend.
---
--- @param opts table  { owner?, budget? = { amount, tag?, desc? } | { from_parent, tag? }, store?, parent? }
--- @return userdata session
function M.open(opts)
    opts = opts or {}
    state_only(opts, OPEN_STATE_KEYS, "knl.open")
    shape.assert_dev(opts, OPEN_OPTS, "knl.open opts")
    return bridge("open")({
        owner = opts.owner,
        budget = opts.budget,
        store = opts.store,
        parent = opts.parent,
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

--- The canonical bracket: open (or resume),
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
--- it is bookkeeping for, so a close that itself fails on that path
--- is warned about rather than raised. On the clean path a failing close
--- raises instead, since a bracket
--- that reports success with no boundary recorded is the one outcome this
--- exists to rule out.
---
--- When both fail, the body error wins and the close failure is the
--- suppressed one (try-with-resources' suppressed exception). It is
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
    -- the one that propagates (the suppressed exception of
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
--- the events of one beat. A module
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

--- Ask the policy about one tool_use block.
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
            data = {
                call_id = call_id,
                name = name,
                args = args,
            },
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
            data = {
                call_id = call_id,
                ok = ok,
                result = result,
            },
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
    -- writes. The kernel neither numbers nor requires it.
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
    -- counts: the budget and `knl.views.usage` are separate readings (the
    -- *budget* section of the header).
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
        data = { request = request },
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
            data = { error = tostring(reason) },
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
        data = {
            content = resp.content,
            usage = resp.usage,
            stop_reason = resp.stop_reason,
        },
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
-- views — the query views this module ships
-- ============================================================
--
-- A view is a named function that runs one SELECT. That is the whole of the
-- mechanism: no builder, no query object, no registration hook. The kernel
-- publishes the table (`knl.shapes.schema`) and a caller writes SQL against
-- it, and these four are the ones the kernel ships because they read what
-- the kernel itself wrote — the beat grouping it stamps, the tool pairs it
-- closes, the ledger it keeps, the token counts the providers reported. A
-- consumer's own view is a function of exactly this form in exactly this way
-- (`local function tool_error_rate(s) return s:query([[...]]) end`); nothing
-- here is privileged.
--
-- They do not duplicate the built-in reads, which are `events(from)` and
-- `tail(n)` and nothing else. Token usage is NOT one of them: it is an
-- aggregate over the log like any other question a caller asks of it, so it
-- is `knl.views.usage` — one SELECT, in this file, on the same footing as a
-- view a consumer writes.
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
-- Which half of the stored event a statement reads decides what can break
-- it. `beat` is a COLUMN — the envelope's
-- correlation key — so `beats` groups on it and is untouched by any change
-- to what a kind carries. The rest reach into `data`, and each of those
-- paths is tied to one kind's shape: `tool_pairs` to the tool pair,
-- `ledger` to `budget_*`, `usage` to `llm_response`. A kind's shape and the
-- view that reads it change together, which is the whole reason the
-- structured half lives in one column instead of being spread over the row.

--- One row per beat: where it starts, where it ends, and what it wrote.
---
--- `kinds` is the beat's events in `seq` order, comma-joined. The order
--- comes from the ordered subquery rather than from `group_concat(kind ORDER
--- BY seq)`: the aggregate's own ORDER BY needs SQLite 3.44, and SQLite may
--- not flatten a subquery with an ORDER BY into an aggregating outer query,
--- so the rows reach the aggregate in the order the subquery put them.
---
--- Events with no `beat` — the session's own boundaries, the ledger, a
--- caller's seed message — are not part of any beat and are left out. The
--- grouping key is the `beat` COLUMN, so this view reads nothing out of any
--- kind's `data` and no change to one can reach it.
local BEATS_SQL = [[
SELECT beat,
       MIN(seq)           AS seq_from,
       MAX(seq)           AS seq_to,
       group_concat(kind) AS kinds
  FROM (SELECT beat,
               stream,
               seq,
               kind
          FROM events
         WHERE stream IN $sessions
           AND beat IS NOT NULL
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
--- The `beat` comes off the column and the rest out of `data`: this view is
--- tied to the shape of `tool_call` / `tool_result` (`knl.shapes.events`)
--- and moves with it.
local TOOL_PAIRS_SQL = [[
SELECT c.beat                            AS beat,
       json_extract(c.data, '$.call_id') AS call_id,
       json_extract(c.data, '$.name')    AS name,
       json_extract(r.data, '$.ok')      AS ok
  FROM events AS c
  JOIN events AS r
    ON r.stream = c.stream
   AND r.kind = 'tool_result'
   AND json_extract(r.data, '$.call_id') = json_extract(c.data, '$.call_id')
 WHERE c.stream IN $sessions
   AND c.kind = 'tool_call'
 ORDER BY c.stream, c.seq
]]

--- The budget ledger: every `budget_*` event in order, with the amount and
--- the grant's tag read out of `data`. Those are kernel kinds, so their
--- `data` shape is the kernel's — this is the one view whose paths belong to
--- the other side of the syscall.
---
--- The four kinds are named rather than matched with a `LIKE 'budget_%'`:
--- they are the closed vocabulary the balance is a fold of (event.rs), and
--- `kind` is an indexed column, so naming them keeps the read the size of
--- the ledger rather than the size of the stream.
local LEDGER_SQL = [[
SELECT seq,
       kind,
       json_extract(data, '$.amount') AS amount,
       json_extract(data, '$.tag')    AS tag
  FROM events
 WHERE stream IN $sessions
   AND kind IN ('budget_granted', 'budget_reserved', 'budget_refused', 'budget_spent')
 ORDER BY stream, seq
]]

--- The token accounting: one row per stream, over the `llm_response` events
--- that stream recorded.
---
--- `calls` is how many answers landed and the three counters are what the
--- providers reported for them — facts already in the log, since an adapter
--- normalizes a provider's usage into the three numbers before the response
--- is ever appended. Nothing here is a budget: the grant is a quota the owner
--- gave (`ledger` is its reading), and no arithmetic connects the two.
---
--- A counter a stored response does not carry contributes 0: `json_extract`
--- answers NULL for a missing key, `SUM` skips it, and the `COALESCE` turns
--- an all-NULL sum back into a number so a caller never reads a nil count.
---
--- The grouping is by `stream`, which is what makes the row set answer for a
--- read across a set of sessions rather than blending them. A stream that
--- recorded no response has no row at all — that absence IS its zero, and
--- filling it in would mean naming the streams in the statement, which is
--- exactly what `$sessions` exists not to do.
local USAGE_SQL = [[
SELECT stream,
       COUNT(*)                                                         AS calls,
       COALESCE(SUM(json_extract(data, '$.usage.input_tokens')), 0)     AS input_tokens,
       COALESCE(SUM(json_extract(data, '$.usage.output_tokens')), 0)    AS output_tokens,
       COALESCE(SUM(json_extract(data, '$.usage.thinking_tokens')), 0)  AS thinking_tokens
  FROM events
 WHERE stream IN $sessions
   AND kind = 'llm_response'
 GROUP BY stream
 ORDER BY stream
]]

--- The session tree, rooted at the session the view is called on.
---
--- The one view that does not take its streams from `$sessions`, and the
--- reason is what a tree is: the set is not something a caller names, it is
--- what the log says was opened from what. So the root is `$stream` — this
--- session — and the walk follows `session_opened.data.parent` down from it
--- with a recursive CTE, which is a `WITH` statement and therefore a read
--- like any other (the query layer needed no change for this: a statement
--- sees the `events` table, and `$stream` / `$sessions` are values bound
--- into it rather than a fence around what it may look at).
---
--- `UNION` and not `UNION ALL`: a stream is in the subtree once, and the
--- duplicate-eliminating form is also what stops a `parent` cycle — which
--- the kernel does not prevent and a log written by hand could contain —
--- from running forever.
---
--- The three per-session readings are correlated subqueries rather than
--- joins, so a stream that recorded two endings (two handles that both
--- closed — the log keeps both) is still one row: the first ending is the
--- one reported, and `MIN(epoch_ms)` says the same for the opening.
---
--- `open_children` comes back as the JSON array text the close recorded,
--- because that is what the column holds: `json_extract` of an array is its
--- JSON, and re-encoding it into a Lua list here would be this view
--- inventing a shape the log does not have.
local TREE_SQL = [[
WITH RECURSIVE tree(session, parent) AS (
    SELECT root.stream, json_extract(root.data, '$.parent')
      FROM events AS root
     WHERE root.kind = 'session_opened'
       AND root.stream = $stream
    UNION
    SELECT child.stream, json_extract(child.data, '$.parent')
      FROM events AS child, tree
     WHERE child.kind = 'session_opened'
       AND json_extract(child.data, '$.parent') = tree.session
)
SELECT t.session AS session,
       t.parent  AS parent,
       (SELECT MIN(o.epoch_ms) FROM events AS o
         WHERE o.stream = t.session AND o.kind = 'session_opened') AS opened_epoch_ms,
       (SELECT MIN(c.epoch_ms) FROM events AS c
         WHERE c.stream = t.session AND c.kind = 'session_closed') AS closed_epoch_ms,
       (SELECT json_extract(c.data, '$.open_children') FROM events AS c
         WHERE c.stream = t.session AND c.kind = 'session_closed'
         ORDER BY c.seq LIMIT 1) AS open_children
  FROM tree AS t
 ORDER BY opened_epoch_ms, session
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

--- The token accounting: `{ stream, calls, input_tokens, output_tokens,
--- thinking_tokens }`, one row per stream that answered.
---
--- @param session userdata|table  a knl session
--- @param opts table|nil  query opts (`sessions` to span a set of streams)
--- @return table rows
--- @return boolean truncated
function M.views.usage(session, opts)
    return read_view(session, USAGE_SQL, opts)
end

--- The subtree rooted at `session`: `{ session, parent, opened_epoch_ms,
--- closed_epoch_ms, open_children }`, in the order the sessions opened.
---
--- The rows are the sessions the log says were opened from this one, however
--- deep. `parent` is nil on a session whose opening recorded none — the root
--- of the whole tree, which this session is not necessarily. `closed_epoch_ms`
--- is nil while a session is still running, and `open_children` is the JSON
--- array a close recorded when it ended with children that had not (see
--- `TREE_SQL`).
---
--- `opts.sessions` is not read: which streams are in a tree is what this view
--- answers, not something a caller names. `timeout_ms` and `limit` are the
--- same knobs as anywhere else, and a subtree bigger than the limit reports
--- `truncated` like any other read.
---
--- @param session userdata|table  a knl session — the root of the walk
--- @param opts table|nil  query opts (`timeout_ms` / `limit`)
--- @return table rows
--- @return boolean truncated
function M.views.tree(session, opts)
    return read_view(session, TREE_SQL, opts)
end

-- ============================================================
-- The registry, executed
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
--
-- The session userdata is deliberately NOT wrapped, in either mode. Its
-- arguments are checked in Rust, on every call, by the same types
-- `knl.shapes.session` declares (`bridge/knl.rs`, `from_lua`), so a direct
-- `s:append(...)` is held to its entry without a gate here — which is the
-- hole this used to have, since a caller reaches the session straight off
-- `knl.open` and never through an export of this module. A wrapper would
-- also have to hand back a proxy in place of the kernel's value, and both
-- `local s <close> = knl.open{...}` and `knl.open{ parent = s }` want the
-- userdata itself.

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
