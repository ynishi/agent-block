--- knl.shapes — the contracts of `knl`, as data.
---
--- What this is
---   Every public interface of the kernel module declared as an lshape, the
---   closed lists beside the shapes that close on them, and the registry
---   (`api`) that names the shape of every export. `knl/init.tl` requires
---   this file and publishes what it returns as `knl.shapes`, unchanged: a
---   caller reads `require("knl").shapes` and never names this module.
---
---   It is a file of its own because it is data. It builds values and
---   holds no behaviour, and it reads nothing of the kernel's — no record, no
---   `Outcome`, no bridge — so it requires `lshape` and not `knl`, and the two
---   files do not require each other in a circle. What holds a value to one
---   of these shapes stays with the code it guards, in `knl/init.tl`: the
---   append and Outcome gates, the device's loud checks, and the dev-mode gate
---   that runs `api` at load.
---
---   Why the contracts are published as data and why the registry is
---   executed is `knl`'s header ("knl.shapes is a registry that is
---   EXECUTED"); the reasoning for each shape sits beside it below.

local lshape = require("lshape")
local T = lshape.t

--- The contracts, as data: lshape shapes a pack asserts at its own
--- boundary in dev mode, and the closed lists beside the shapes that
--- close on them. The record is also this module's value — its fields
--- are assigned below and `require("knl.shapes")` answers it — so
--- `knl/init.tl` names the type with a `local type` import of this file.
local record Shapes
    outcome: lshape.Schema
    error: lshape.Schema
    query_opts: lshape.Schema
    schema: any
    error_kinds: {string}
    call_error: lshape.Schema
    call_error_kinds: {string}
    call_error_retryable: {string:boolean}
    request: lshape.Schema
    window_report: lshape.Schema
    export_opts: lshape.Schema
    event_base: lshape.Schema
    event_meta: lshape.Schema
    events: {string:lshape.Schema}
    budget_allocation: lshape.Schema
    device_config: lshape.Schema
    tool_spec: lshape.Schema
    tool_entry: lshape.Schema
    tool_policy_decision: lshape.Schema
    cost_result: lshape.Schema
    llm_result: lshape.Schema
    llm_usage: lshape.Schema
    tool_use_block: lshape.Schema
    session_handle: lshape.Schema
    callable: lshape.Schema
    open_opts: lshape.Schema
    resume_opts: lshape.Schema
    budget_grant: lshape.Schema
    --- The registry: one entry per export, naming what goes in and out.
    api: {string:any}
    --- The same, for the predefined views: one entry per view.
    views: {string:any}
    --- The bridge's own two halves — one entry per syscall, naming the
    --- generated type its arguments and its return are declared with — and
    --- that generated module itself.
    session: {string:any}
    module: {string:any}
    rust: any
end

-- ============================================================
-- shapes — the public contracts of `knl` (handwritten lshape)
-- ============================================================
--
-- Every public IF is defined here and published through `knl.shapes`
-- so a caller reads the contract as data
-- rather than out of prose, and `knl.shapes.api` names the shape of
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
-- prim, so the shape admits the three types and `device_problem` (in
-- `knl/init.tl`) makes the exact judgement.
--
-- The event vocabulary this layer writes (msg_user / llm_request /
-- llm_response / llm_call_failed / tool_call / tool_result) is here too,
-- as `knl.shapes.events` — but only the `data` half of each, because that
-- is the half whose owner is the writer (the *stored event* section of the
-- header). There
-- is no second copy of anything: the kernel validates the envelope and the
-- `data` of ITS own kinds (`session_*`, `budget_*`) and stopped judging
-- these, so the shapes below are the only declaration of them, and the
-- appends in `knl/init.tl` (`record_event`) are where they are held.

--- A Lua function, as a shape. lshape's `prim` handler is `type(v) ==
--- schema.prim` for any type name, but `lshape.t` exposes only the five it
--- names, so these two are built from the same plain-data schema form
--- (Schema-as-Data: the state is the table; the metatable carries only the
--- `:is_optional()` / `:describe()` sugar).
-- The plain-data form is what lshape's own `t` builds and hands out, so the
-- table is named as the Schema it is once the metatable is on it.
local FUNCTION = setmetatable({ kind = "prim", prim = "function" } as lshape.Schema, lshape.t._internal.schema_mt as metatable<lshape.Schema>)
local USERDATA = setmetatable({ kind = "prim", prim = "userdata" } as lshape.Schema, lshape.t._internal.schema_mt as metatable<lshape.Schema>)

--- Something a beat can call: a function, or a table / userdata carrying
--- `__call` — a Port shim may hand back either. The exact test is
--- `callable` in `knl/init.tl`, run loudly at construction; this is its
--- data shape.
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

--- What a fold left out of the request it just built.
---
--- A fold that windows the history takes a decision the request itself cannot
--- express — it simply begins later, and a reader of the log cannot tell a
--- conversation that started there from one that was cut. So the fold may
--- answer this beside the request and `knl.beat` records it on the
--- `llm_request` event; nothing in the kernel reads it back. It is a fact for
--- the log, in the terms the decision was taken in: BEATS dropped, not events
--- or bytes.
---
---   dropped    the beat ids that went, oldest first; empty when none did
---   kept       how many beats the request carries
---   seed_kept  whether the events ahead of the first kept beat (the caller's
---              seed) are in the request — always so when nothing was
---              dropped, and the fold's own `keep_seed` when something was
---   before     what the whole log cost, in the unit the fold counted in
---   after      what was sent
---   limit      the room the count was held against
---
--- The last three are optional together: a fold that windows by a count of
--- beats never counts tokens at all, and a number invented there would be one
--- nobody measured. `policy.window` is the fold that fills them (sized by a
--- `room`); the plain `tail` form leaves all three out.
---
--- Closed, like every other `data` shape: this is a path `knl.views.beats`
--- selects, and a key that arrived by accident is a column somebody will
--- eventually read.
local WINDOW_REPORT = T.shape({
    dropped = T.array_of(T.string):describe("the beat ids the fold left out, oldest first"),
    kept = T.number:describe("how many beats the request carries"),
    seed_kept = T.boolean:describe("the events ahead of the window are in the request"),
    before = T.number:describe("what the whole log counted"):is_optional(),
    after = T.number:describe("what was sent"):is_optional(),
    limit = T.number:describe("the room the profile left for a request"):is_optional(),
    reserve = T.number
    :describe("tokens held back for the reply beyond profile.max_output, and so not in `limit`")
    :is_optional(),
}, { open = false })

--- An event's `meta`: labels, and only labels.
---
--- Shallow by rule — string / number / boolean values and nothing else —
--- because `meta` is the half of the envelope a view can read without ever
--- being broken by a schema change, and a nested value would make it a
--- second `data` with none of that promise. The kernel refuses a nested one
--- at the syscall; this is the same rule as data, so dev mode says it at
--- the line that wrote it rather than at the boundary.
---
--- `meta.beat` is a label like any other, and the one this layer stamps:
--- the id of the beat that wrote the event, an opaque string. It is not
--- named in the shape — a map of labels is what it is, and `beat` is a
--- string like the rest. The views group on it; that does not make it a
--- field.
local EVENT_META = T.map_of(T.string, T.any_of({ T.string, T.number, T.boolean }))

--- The envelope, in the terms this layer owns: a `kind` string, the shallow
--- `meta` labels (`meta.beat` among them), and the `data` a kind carries.
---
--- Open, and deliberately: the kernel stamps `seq` / `epoch_ms` /
--- `_schema_version` on a stored event, so the value that comes back out of
--- `events()` carries more keys than the one that went in. The closure —
--- "no other top-level key" — is the kernel's, enforced at the syscall
--- where the stamps are known; what this shape holds is the three fields a
--- caller writes.
---
--- What is inside `data` is per-kind and is `EVENT_DATA` below.
local EVENT_BASE = T.shape({
    kind = T.string,
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
--- The classification a model call that did not come off carries.
---
--- WHY IT IS A VOCABULARY AND NOT A STATUS. A beat reports a failed call as
--- `Outcome.err("call", detail)`, and `kind` there names the STAGE — it says
--- the llm is where the beat broke and nothing about what broke. So a loop
--- had nothing to decide on: `policy.retry` reads `detail.kind` and
--- `detail.retryable`, and a call failure carried neither, which meant no
--- retry policy could ever fire on the one failure that is most often worth
--- asking again about (a rate limit, an overloaded provider, a connection
--- that dropped).
---
--- These seven words are that missing half, and they are provider-neutral by
--- construction: an HTTP status is one provider's word for several things,
--- and it is read in exactly one place — the adapter's mapping table
--- (`knl_adapter`) — which answers with one of these. Nothing downstream
--- reads a status or a status class; `status` rides along on the detail as a
--- fact for a person reading a log, never as something to branch on.
---
---   transport        the call never got an answer: connect, read, timeout
---   rate_limited     the provider said "too fast" (429)
---   overloaded       the provider said "not now" (529 / 503)
---   server           the provider broke (5xx)
---   auth             the credentials were refused (401 / 403)
---   invalid_request  the request itself was rejected (400 / 404 / 413 / 422)
---   unknown          anything the mapping does not name — including an
---                    adapter that broke its own contract, and a raise
---
--- `retryable` is true for exactly the first four. It is a separate field
--- rather than a lookup because it is the judgement a loop acts on, and the
--- table below is what the two are kept in step by.
local CALL_ERROR_KINDS: {string} = { "transport", "rate_limited", "overloaded", "server", "auth", "invalid_request", "unknown" }

--- Which of the kinds above says asking again could work.
---
--- Published beside the list so a caller (or a port) reads one answer rather
--- than deciding for itself: a provider being busy is worth another call, a
--- request the provider refused to read is not.
local CALL_ERROR_RETRYABLE: {string:boolean} = {
    transport = true,
    rate_limited = true,
    overloaded = true,
    server = true,
    auth = false,
    invalid_request = false,
    unknown = false,
}

--- What a failed call's `detail` carries — `Outcome.err("call", <this>)`.
---
--- Open, like `knl.shapes.error`: the dev-mode traceback of a caller's own
--- raising llm rides here, and a port may attach what its own reader needs.
--- What is closed is the vocabulary of `kind`.
---
--- `retry_after` is seconds, and it is present only when the provider named
--- one (a `retry-after` header); `status` is the HTTP status the mapping read,
--- carried for a reader and never for a decision.
local CALL_ERROR = T.shape({
    kind = T.one_of(CALL_ERROR_KINDS),
    retryable = T.boolean,
    retry_after = T.number:describe("seconds, as the provider named them"):is_optional(),
    message = T.string,
    status = T.number:describe("the HTTP status the mapping read; a fact, not a branch"):is_optional(),
})

--- Two kinds here are the KERNEL's, not this layer's: `session_opened` and
--- `session_closed`. Nothing in Lua may write them (the kernel refuses a
--- hand-written boundary) and their shape is checked on the other side of
--- the syscall — they are declared here because a supervisor READS them.
--- `parent` and `open_children` are how a session tree is recorded, and a
--- view over them (`knl.views.tree`) is tied to those two paths the same way
--- `usage` is tied to `llm_response`: the declaration is what says which
--- reads move when the shape does.
local EVENT_DATA: {string:lshape.Schema} = {
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
    -- The request as it was sent, and — when the device's fold answered one
    -- — what that fold left out to arrive at it (`WINDOW_REPORT`). The
    -- report is optional because most folds do not window: the default one
    -- sends the whole history and has nothing to report.
    llm_request = T.shape({
        request = REQUEST,
        window = WINDOW_REPORT:is_optional(),
    }, { open = false }),
    llm_response = T.shape({
        content = T.array_of(T.table),
        usage = T.table,
        stop_reason = T.string:is_optional(),
    }, { open = false }),
    -- The note a call that did not come off leaves behind. `error` is the
    -- sentence (a person reads it, and `policy.carry` puts it in front of the
    -- next request); the rest is the same classification the Outcome carries,
    -- recorded so the log answers "what kind of failure was that" without the
    -- caller having kept the Outcome. Closed, like the rest.
    llm_call_failed = T.shape({
        error = T.string,
        kind = T.one_of(CALL_ERROR_KINDS),
        retryable = T.boolean,
        retry_after = T.number:is_optional(),
        status = T.number:is_optional(),
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

--- One tool, flat: what it is called, what the model is told about it, the
--- input schema it is declared with, and the function that runs it. This is
--- the shape `std.fs.tool_specs` answers with and the one
--- `knl_adapter.tools` takes, and it is the kernel's because the device it
--- ends up in is.
---
--- The two halves of a tool's life are PROJECTIONS of it, taken from these
--- fields rather than written again: a device's `tools` map is keyed BY the
--- name, so its entries are this minus `name` (`TOOL_ENTRY` below), and what
--- a Port declares to a provider is this minus the handler
--- (`knl_adapter.shapes.tool_decl`). Three declarations of one triple is how
--- the three drift apart; one, projected twice, is how they cannot.
---
--- Open, like the other shapes at this boundary (`tool_use_block`):
--- `description` and `input_schema` are the provider's vocabulary, and a
--- source that carries its own labels alongside is not this layer's to
--- refuse. The one confusable key IS refused, loudly and by name — a spec
--- that spells the schema field `schema` is a construction error in
--- `knl_adapter.ToolPort.lua`, not a second accepted spelling.
local TOOL_SPEC = T.shape({
    name = T.string,
    description = T.string:is_optional(),
    input_schema = T.any:is_optional(),
    handler = FUNCTION,
})

--- One entry of a device's `tools` map: `TOOL_SPEC` minus `name`, because
--- the map's key is the name. The three fields are the spec's own tables, so
--- what an entry promises and what a spec promises cannot say different
--- things about the same field. Only `input_schema` is read; there is no
--- alias.
-- A `shape`'s own field map, read as data: `lshape.Schema` is one record for
-- every kind of schema and its `fields` is not part of that surface, so the
-- map is named here once and the three entries come off it.
local TOOL_SPEC_FIELDS = (TOOL_SPEC as {string:any}).fields as {string:lshape.Schema}

local TOOL_ENTRY = T.shape({
    description = TOOL_SPEC_FIELDS.description,
    input_schema = TOOL_SPEC_FIELDS.input_schema,
    handler = TOOL_SPEC_FIELDS.handler,
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
--- `meta` is the labels the opening is written with — the envelope's own
--- key, the same vocabulary an `append` carries (shallow scalars). It is how
--- a caller running many sessions in one log names them: a supervisor selects
--- on `json_extract(meta, '$.<key>')` of `session_opened` without knowing what
--- the kind records, and without giving each session a database of its own.
local OPEN_OPTS = T.shape({
    owner = T.string:is_optional(),
    budget = T.any_of({ BUDGET_GRANT, BUDGET_ALLOCATION }):is_optional(),
    store = T.any:is_optional(),
    meta = T.any:is_optional(),
    parent = SESSION_HANDLE:is_optional(),
})

--- `knl.resume` opts: the session to reopen, plus the grant this process runs
--- under and, when the stream is not in the host's own database, the store it
--- is in. Optional for the same reason it is on open: no store named is the
--- file the host owns, which is where a session opened without one went.
local RESUME_OPTS = T.shape({
    store = T.any:is_optional(),
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
--- reports as `err("call")` — with `err` a `call_error` above when the port
--- classified it.
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
---
--- `unsupported` is the one class NOTHING REACHABLE FROM HERE RAISES. It is
--- the store SPI's own: a backend that keeps no queryable table, or keeps one
--- stream and so cannot write two in a transaction, answers with it — and the
--- only backend the host has does both. The two cases a caller might expect it
--- for come back as `validation` instead, with a message that says which
--- argument was wrong: a child asked for on a `"mem"` parent, and an unknown
--- `view` name. It is declared here because the kernel publishes it
--- (`knl.api().errors`, held against this list in `knl_beat_test.lua` inv10)
--- and because a store may still return it; it is not a branch a shell needs
--- to write today.
local ERROR_KINDS: {string} = { "busy", "storage", "corruption", "closed", "validation", "unsupported", "timeout", "refused" }

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

--- What `knl.export` is asked for.
---
--- `as` names the reading — the stored events as they came back, or the
--- messages they fold to — and defaults to `"events"`, the plainest of the
--- two. `sessions` is the same word `query_opts` uses and is held to a
--- narrower promise here: this round exports the session's own stream, so a
--- list naming anything else is refused rather than half-honoured (see
--- `knl.export`).
---
--- Closed, like `query_opts`: an option the reader does not know must not
--- quietly do nothing.
local EXPORT_OPTS = T.shape({
    as = T.one_of({ "events", "messages" }):describe('the reading; default "events"'):is_optional(),
    sessions = T.array_of(T.string)
    :describe("the streams to export; only this session's own id, this round")
    :is_optional(),
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
--- `epoch_ms` / `kind` / `schema_version` are the kernel's stamps, `meta`
--- holds the shallow labels — the beat id among them — and `data` holds the
--- one structured JSON value a kind is about.
---
--- `position` is the key, and it is the log's global order rather than one
--- stream's: every event of the database in the order it committed, dense
--- and gap-free as read. `(stream, seq)` is unique beside it, so a read
--- within one session still orders by `seq`; a read ACROSS sessions orders
--- by `position`, which is the one thing `seq` cannot answer.
---
--- There is no `beat` column. The id lives in `meta` and the views reach it
--- with `json_extract(meta, '$.beat')`, which the log carries an index for.
--- There is no `payload` column either: the whole-object form it held is
--- what an earlier round split into the envelope and the one structured
--- field.
local EVENTS_SCHEMA: {string:any} = {
    table = "events",
    columns = {
        { name = "position", type = "INTEGER", pk = true },
        { name = "stream", type = "TEXT", pk = false },
        { name = "seq", type = "INTEGER", pk = false },
        { name = "epoch_ms", type = "INTEGER", pk = false },
        { name = "kind", type = "TEXT", pk = false },
        { name = "schema_version", type = "INTEGER", pk = false },
        { name = "meta", type = "TEXT", pk = false },
        { name = "data", type = "TEXT", pk = false },
    },
}

--- The contracts `knl` holds itself to, as data.
Shapes.outcome = OUTCOME
Shapes.error = ERROR
Shapes.query_opts = QUERY_OPTS
Shapes.schema = EVENTS_SCHEMA
-- The vocabulary as a list, next to the shape that closes on it: a
-- caller enumerating the classes reads the same constant the shape does.
Shapes.error_kinds = ERROR_KINDS
-- The same arrangement for a failed model call: the classification a
-- beat's `err("call")` carries, and the closed list its `kind` comes
-- from. An adapter maps its provider onto these words and a loop decides
-- on them; nobody downstream reads a status.
Shapes.call_error = CALL_ERROR
Shapes.call_error_kinds = CALL_ERROR_KINDS
Shapes.call_error_retryable = CALL_ERROR_RETRYABLE
Shapes.request = REQUEST
-- What a fold answers BESIDE the request when it left something out.
-- Published here rather than in the pack that produces it (`policy.window`)
-- for the reason every other event shape is: the kernel is what records
-- it, on the `llm_request`, so the kernel owns the declaration.
Shapes.window_report = WINDOW_REPORT
Shapes.export_opts = EXPORT_OPTS
Shapes.event_base = EVENT_BASE
Shapes.event_meta = EVENT_META
-- The `data` shapes of the kinds this layer writes, by kind, plus the
-- two kernel boundary kinds a supervisor reads (`session_opened` /
-- `session_closed`). The envelope is `event_base`; this is the half
-- whose owner is the writer.
Shapes.events = EVENT_DATA
Shapes.budget_allocation = BUDGET_ALLOCATION
Shapes.device_config = DEVICE_CONFIG
-- A tool, and the two projections of it this layer hands around: the
-- flat spec a caller writes (`knl_adapter.tools` takes an array of
-- them), and the entry a device's map holds under the name.
Shapes.tool_spec = TOOL_SPEC
Shapes.tool_entry = TOOL_ENTRY
Shapes.tool_policy_decision = TOOL_POLICY_DECISION
Shapes.cost_result = COST_RESULT
Shapes.llm_result = LLM_RESULT
Shapes.llm_usage = USAGE
Shapes.tool_use_block = TOOL_USE_BLOCK
-- The two types a session handle can have, for a shell pack declaring a
-- function that takes one. Every pack above `knl` wrote this same
-- `any_of` for itself; the judgement that goes with it is `is_session`,
-- and having the two come from one place is what keeps them agreeing.
Shapes.session_handle = SESSION_HANDLE
-- Callable, for the same reason: a pack that takes a caller's function
-- and accepts a `__call` table (a Port shim hands back either) was
-- restating this too.
Shapes.callable = CALLABLE
Shapes.open_opts = OPEN_OPTS
Shapes.resume_opts = RESUME_OPTS
Shapes.budget_grant = BUDGET_GRANT

--- The API registry: one entry per public
--- export, naming the shape of what goes in and what comes out. It exists
--- so the completeness of the contract is *checked* rather than remembered
--- — `knl/spec/api_spec.lua` walks `knl` and fails on an export with
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
--- the registry be RUN — see the dev-mode gate at the foot of `knl/init.tl`,
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
--- `knl.shapes.session` / `knl.shapes.module` below describe that surface from
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
-- `require` is named as the plain function it is here, so the module comes
-- back as a VALUE to read names off — the `local type` import at the top of
-- `knl/init.tl` is the other half, the types, and neither is the other.
local types_ok, RUST = pcall((require as function(string): {string:any}), "knl_types")
if not types_ok then
    RUST = setmetatable({} as {string:any}, {
        __index = function(_: any, name: string): string
            return "knl_types." .. name .. " (generated by the host; no bridge in this VM)"
        end,
    } as metatable<{string:any}>)
end

Shapes.rust = RUST

Shapes.session = {
    id = { args = "none", returns = RUST.SessionId },
    scope_id = { args = "none", returns = RUST.ScopeId },
    owner = { args = "none", returns = RUST.Owner },
    append = { args = { RUST.AppendEvent }, returns = RUST.Seq },
    -- The record from a position on, and whether the read stopped at the
    -- kernel's row cap. The pair is `query`'s, for the same reason: a page and
    -- a whole answer are different facts, and a fold over the first is a
    -- request missing its newest turns.
    events = { args = { RUST.Seq }, returns = RUST.EventsResult },
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

Shapes.module = {
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
local function arg_of(schema: lshape.Schema, desc: string): {string:any}
    return { shape = schema, desc = desc }
end

--- The arguments every predefined view takes: the session whose store is
--- read, and the query options (the set of streams above all) passed
--- straight through to `session:query`.
local function view_args(): {{string:any}}
    return { arg_of(SESSION_HANDLE, "session"), arg_of(QUERY_OPTS, "opts?") }
end

--- The predefined query views, declared.
---
--- One table, published twice: as `knl.shapes.views` (the registry a caller
--- reads) and as the `members` of the `views` entry below (what the dev-mode
--- gate walks and what `api_spec` holds `knl.views` against). Two names for
--- one table rather than two tables, so a view cannot be declared in one
--- place and missed in the other.
local VIEWS: {string:any} = {
    beats = {
        args = view_args(),
        returns = "{ { beat, seq_from, seq_to, kinds, dropped, before, after }, ... }, truncated"
            .. " — one row per beat, in first-seq order; the last three come off that beat's own"
            .. " `llm_request` (`data.window`, what its fold left out: how many beats it dropped and"
            .. " what the request cost before and after) and are nil when the request carried no report",
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
    sessions = {
        args = view_args(),
        returns = "{ { session, meta, parent, opened_epoch_ms, closed_epoch_ms? }, ... }, truncated"
            .. " — one row per session in the log; `meta` is the JSON text of"
            .. " the labels it was opened with",
    },
    tree = {
        args = view_args(),
        returns = "{ { session, parent, opened_epoch_ms, closed_epoch_ms?, open_children? }, ... }, truncated"
            .. " — the subtree rooted at this session, discovered from the log;"
            .. " `open_children` is the JSON array the close recorded",
    },
}

Shapes.views = VIEWS

Shapes.api = {
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
    export = {
        args = { arg_of(SESSION_HANDLE, "session"), arg_of(EXPORT_OPTS, "opts?") },
        returns = 'table — the whole log, paged to the end: the stored events in seq order (as = "events"),'
            .. ' or one entry per message-bearing event (as = "messages": { role, content, beat, seq,'
            .. " epoch_ms, kind }, plus usage / stop_reason where the entry came from an llm_response)",
    },
    new_beat_id = {
        args = {},
        returns = "string (time-ordered, session-free)",
    },
    -- The two the bridge owns and this module re-exports. Their contract is
    -- the bridge's, so `returns` points at the generated type the syscall
    -- itself is declared with (`knl.shapes.module.error` / `.api` name the
    -- same two) rather than restating it here.
    --
    -- `args` cannot point there as well, and the reason is the fallback: a VM
    -- with no bridge has no `knl_types`, so `RUST.Raised` is the sentence that
    -- names the type it would have been. `returns` may be a sentence — nothing
    -- executes it — but an `args` entry is RUN by the gate in `knl/init.tl`,
    -- so it carries the widest shape that is still true of a raised value,
    -- and the type it stands for is named beside it.
    error = {
        args = { arg_of(T.any, "raised (knl_types.Raised — whatever a syscall handed over)") },
        returns = RUST.ErrorTable,
    },
    api = {
        args = {},
        returns = RUST.ApiReport,
    },
    is_session = {
        args = { arg_of(T.any, "a value that may be a session handle") },
        returns = "boolean — it answers the whole of `shapes.session`",
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

return Shapes
