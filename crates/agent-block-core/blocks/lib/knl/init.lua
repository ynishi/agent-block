--- knl — the Lua kernel (POC of ctx / beat / fold / Outcome).
---
--- What this is
---   The driving half of the kernel/shell split: Rust is the pure syscall
---   layer (session / append / events / view / spend / close), and this
---   module runs a beat — one model call plus the tools that call asks for —
---   over it, returning an `Outcome`. See `core-loop-design.md` (beat / ctx)
---   on top of `knl-kernel-design.md`; this file is the POC, not the full one.
---
--- The ctx shape (core-loop-design.md §1/§2)
---   `knl.open{...}` returns a ctx: a single immutable handle that carries
---   both the state (the Rust session handle — History / Budget) and the
---   config (the default policy: llm, tools, filters, ...). `knl.beat(ctx)`
---   takes that one argument and reads everything from it. Per-beat policy
---   variation is NOT an override argument — derive a new ctx with
---   `ctx:with{ llm = strong }` and beat that instead. There is no loop in
---   this module: a driver composes beats on the spot, shell-style; the
---   minimal primitive is deliberate so that composition stays writable
---   inline (the old `run` is gone — a default loop belongs to a layer
---   above knl, not inside it).
---
--- Immutability (core-loop-design.md §2)
---   The config is fixed at open/with time and guarded by `__newindex`
---   (mutating a ctx raises). The state only advances by appending to the
---   fact-log through the handle — the ctx value itself never mutates.
---   `with` returns a new ctx sharing the same session; concurrent appends
---   to one session are the Rust kernel's CAS problem, not Lua's.
---
--- Beat numbering (kernel-owned)
---   The ctx is the numbering authority. Appending a `model_response`
---   through `ctx:append` is what numbers it: the Rust kernel assigns the
---   number (like `seq`) and charges the usage in the same step. beat reads
---   the number back with `ctx:turns()` afterwards and stamps it onto the
---   tool_call / tool_result events. (The Rust-side event field is still
---   named `turn`; renaming it to `beat` is a deferred ST — the Lua Outcome
---   already speaks `beat`.)
---
--- What the POC deliberately leaves out
---   Real provider adapters (llm is `fn(req) -> {status, content, usage,
---   stop_reason}`), the full fold vocabulary, realistic tool / filter
---   policies, and fork (branching the history — deferred until the basic
---   shape is wired into real loops).

--- The Rust syscall bridge, captured before `require("knl")` shadows the
--- name (see the header). `nil` in a VM that has no bridge (e.g. the pure
--- lspec runner): `fold` / `Outcome` / `beat` never touch it, and `open`
--- reports its absence rather than indexing nil.
local syscall = knl

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

local M = {}

-- ============================================================
-- Outcome — the result type of a beat
-- ============================================================
--
-- Plain-data status tag tables, never metatable methods: an Outcome crosses
-- the JSON boundary with the kernel, and a metatable does not survive the
-- round trip. Predicates and the match are free functions for the same
-- reason — the value carries data, the module carries behaviour.

local Outcome = {}

--- The status values the kernel knows. Exactly these three, provider-neutral.
local STATUSES = { "ok", "refused", "error" }

--- A beat that ran (a budget stop is one too — it ran and stopped on
--- purpose). `out` is the call's return, or `{ budget_stopped = true }` when
--- the gate stopped before calling.
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

function Outcome.is_ok(o)
    return type(o) == "table" and o.status == "ok"
end

function Outcome.is_refused(o)
    return type(o) == "table" and o.status == "refused"
end

function Outcome.is_error(o)
    return type(o) == "table" and o.status == "error"
end

--- Match an Outcome against `arms = { ok = fn, refused = fn, error = fn }`.
---
--- Exhaustive: every one of the three arms must be present, and the actual
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
--- case crosses the boundary — into the durable `request` event and onto
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
--- `config` and writes nothing. Three kinds map to messages and the rest are
--- skipped (tool_call / run_* / model_call_failed / request / open kinds):
---
---   msg_user       -> { role = "user", content = <verbatim> }
---   model_response -> { role = "assistant", content = <verbatim> }
---   tool_result    -> collected, in seq order, into the user message that
---                     follows the assistant turn they answer (consecutive
---                     tool_results batch together, which for a well-formed
---                     history is the same as grouping by beat)
---
--- `system` and `tools` are composed from the config each beat, not read
--- from the history.
---
--- @param events table  array of stored events (from `ctx:events()`)
--- @param config table
--- @return table request  { system?, messages, tools? }
function M.fold(events, config)
    config = config or {}
    local messages = tag_array({})
    local batch = tag_array({})

    -- The tool_use ids of the most recent assistant message, in block
    -- order, and which of them a recorded tool_result has answered.  A
    -- history can legitimately end (or be interrupted) between the
    -- model_response append and the tool results — the state a crash
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
        elseif kind == "model_response" then
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
        -- everything else (tool_call / run_* / model_call_failed / request /
        -- open kinds) is not part of a request: skip it.
    end
    close_dangling()
    flush()

    local request = { messages = messages }
    if config.system ~= nil then
        request.system = config.system
    end
    if config.tools ~= nil then
        request.tools = wire_tools(config.tools)
    end
    return request
end

-- ============================================================
-- shapes — the kernel's data contracts (handwritten lshape; the
-- schema-bridge SoT judgement is a separate, non-blocking ST)
-- ============================================================
--
-- Only *data* gets a shape. The config is a closure bundle (llm / tools /
-- filters are functions) — not persistable, not shapeable; its checks stay
-- in `config_problem`. What crosses boundaries as data is shaped here and
-- asserted in dev mode (LSHAPE_CHECK=1), a no-op in prod — the same
-- boundary discipline as knl_adapter's `llm_result`.

--- An Outcome, as data. Discriminated on `status` — exactly the kernel's
--- three, each variant carrying only its own fields.
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

--- Every event minimally: a `kind` string. The vocabulary is open (run_* /
--- open kinds and caller kinds exist beyond what this module writes), so
--- the base shape is the only universal contract.
local EVENT_BASE = T.shape({ kind = T.string })

--- Per-kind shapes for the kinds this module itself writes (plus the
--- msg_user seed). Open shapes: the kernel stamps seq / epoch_ms / turn on
--- the way in, and those ride alongside. `turn` is optional on the tool
--- pair (stamped from out.beat by execute_tools).
local EVENT_SHAPES = {
    msg_user = T.shape({
        kind = T.literal("msg_user"),
        content = T.any,
    }),
    request = T.shape({
        kind = T.literal("request"),
        request = REQUEST,
    }),
    model_response = T.shape({
        kind = T.literal("model_response"),
        content = T.any,
        usage = T.table:is_optional(),
        stop_reason = T.string:is_optional(),
    }),
    model_call_failed = T.shape({
        kind = T.literal("model_call_failed"),
        error = T.string,
    }),
    tool_call = T.shape({
        kind = T.literal("tool_call"),
        turn = T.number:is_optional(),
        call_id = T.string,
        name = T.string,
        args = T.table,
    }),
    tool_result = T.shape({
        kind = T.literal("tool_result"),
        turn = T.number:is_optional(),
        call_id = T.string,
        ok = T.boolean,
        result = T.any,
    }),
}

--- The contracts this module holds itself to, as data — mirrors how
--- knl_adapter exposes `llm_result` via `M.shapes`.
M.shapes = {
    outcome = OUTCOME,
    request = REQUEST,
    event = EVENT_BASE,
    events = EVENT_SHAPES,
}

--- Dev-mode gate for an event about to cross into the kernel: the base
--- contract always, the per-kind contract when this module knows the kind.
--- Unknown kinds pass on the base alone (open vocabulary, not a typo trap
--- this layer can judge). No-op in prod.
local function assert_event_dev(ev)
    shape.assert_dev(ev, EVENT_BASE, "knl_event")
    local per_kind = type(ev) == "table" and EVENT_SHAPES[ev.kind] or nil
    if per_kind then
        shape.assert_dev(ev, per_kind, "knl_event:" .. tostring(ev.kind))
    end
    return ev
end

--- Dev-mode gate for an Outcome on its way out of beat. No-op in prod.
local function emit(o)
    return shape.assert_dev(o, OUTCOME, "knl_outcome")
end

-- ============================================================
-- ctx — the immutable handle (state + config)
-- ============================================================

--- The fields `with` may derive. Everything that names the session itself
--- (owner / store / session) is NOT here: that is a different session, not a
--- derivation — open or resume one instead.
local CONFIG_KEYS = {
    llm = true,
    tools = true,
    tool_policy = true,
    fold = true,
    filters = true,
    system = true,
    max_turns = true,
}

--- Minimal config shape check. Returns an error string, or nil when the
--- config is usable. `llm` is not required here — a ctx may be opened for
--- reading/appending only; beat's gate demands it.
local function config_problem(config)
    if type(config) ~= "table" then
        return "config must be a table"
    end
    if config.filters ~= nil and type(config.filters) ~= "table" then
        return "filters must be an array of functions"
    end
    if config.tools ~= nil and type(config.tools) ~= "table" then
        return "tools must be a table"
    end
    if config.tool_policy ~= nil and type(config.tool_policy) ~= "function" then
        return "tool_policy must be a function"
    end
    if config.fold ~= nil and type(config.fold) ~= "function" then
        return "fold must be a function"
    end
    return nil
end

local ctx_with -- forward declaration (referenced by make_ctx's __index)

--- Wrap a kernel session handle and a frozen config into a ctx.
---
--- Reads resolve in order: `with` / internals, then config fields (the
--- memory-map read: `ctx.llm`, `ctx.max_turns`, ... are direct), then the
--- handle's own surface (state methods — `ctx:append`, `ctx:events`,
--- `ctx:turns`, `ctx:view`, ... delegate to the kernel handle). Writes
--- raise: the ctx is immutable, derive with `ctx:with{...}`.
local function make_ctx(handle, config)
    local mt = {
        __index = function(_, k)
            if k == "with" then
                return ctx_with
            end
            -- Internals: the spec and beat read these; not part of the
            -- public surface.
            if k == "_handle" then
                return handle
            end
            if k == "_config" then
                return config
            end
            local v = config[k]
            if v ~= nil then
                return v
            end
            -- The append boundary carries the dev-mode event contract:
            -- every event that crosses into the kernel is asserted against
            -- EVENT_BASE (+ the per-kind shape when known) before it goes.
            if k == "append" then
                local m = handle.append
                return function(_, ev)
                    return m(handle, assert_event_dev(ev))
                end
            end
            -- State surface: delegate to the kernel handle. Methods are
            -- re-bound so `ctx:append(...)` reaches `handle:append(...)`;
            -- plain fields pass through as-is.
            local m = handle[k]
            if type(m) == "function" then
                return function(_, ...)
                    return m(handle, ...)
                end
            end
            return m
        end,
        __newindex = function(_, k)
            error(
                "ctx is immutable: derive a new one with ctx:with{ "
                    .. tostring(k)
                    .. " = ... }",
                2
            )
        end,
        __metatable = "knl.ctx",
    }
    return setmetatable({}, mt)
end

--- Derive a new ctx from this one: same session (state), config with
--- `delta` merged over it. The original ctx is untouched and stays usable —
--- both point at the same fact-log, and the kernel's CAS guards concurrent
--- appends. Only CONFIG_KEYS may appear in the delta; `owner` / `store` /
--- `session` name a different session and are rejected loudly.
---
--- @param ctx table  a knl ctx
--- @param delta table  config fields to override
--- @return table ctx'  a new immutable ctx
ctx_with = function(ctx, delta)
    if type(delta) ~= "table" then
        error("ctx:with: delta must be a table", 2)
    end
    local merged = {}
    for k, v in pairs(ctx._config) do
        merged[k] = v
    end
    for k, v in pairs(delta) do
        if not CONFIG_KEYS[k] then
            error(
                "ctx:with: '"
                    .. tostring(k)
                    .. "' is not a config field (owner/store/session are not "
                    .. "derivable — open a new session instead)",
                2
            )
        end
        merged[k] = v
    end
    local problem = config_problem(merged)
    if problem then
        error("ctx:with: " .. problem, 2)
    end
    return make_ctx(ctx._handle, merged)
end

--- Split open/resume opts into state opts (for the syscall) and config
--- (frozen onto the ctx). Unknown keys raise — a typo in a policy field
--- must not silently become a no-op.
local function split_opts(opts, state_keys, who)
    local state, config = {}, {}
    for k, v in pairs(opts) do
        if state_keys[k] then
            state[k] = v
        elseif CONFIG_KEYS[k] then
            config[k] = v
        else
            error(who .. ": unknown option '" .. tostring(k) .. "'", 3)
        end
    end
    local problem = config_problem(config)
    if problem then
        error(who .. ": " .. problem, 3)
    end
    return state, config
end

-- ============================================================
-- open / resume — build a ctx
-- ============================================================

local OPEN_STATE_KEYS = { owner = true, budget = true, store = true }
local RESUME_STATE_KEYS = { store = true, session = true, budget = true }

--- Open a session and return its ctx.
---
--- State opts (`owner` / `budget` / `store`) go to `knl.open` (the Rust
--- bridge — `store` absent / "mem" for the in-memory log, `{ sqlite =
--- "<path>" }` for a durable stream). Everything else is default policy,
--- frozen onto the ctx: config is fixed here, per-beat variation derives
--- with `ctx:with{...}`.
---
--- @param opts table  { owner?, budget?, store?,
---                      llm?, tools?, tool_policy?, fold?, filters?,
---                      system?, max_turns? }
--- @return table ctx  an immutable knl ctx
function M.open(opts)
    opts = opts or {}
    if syscall == nil then
        error("knl.open: the knl syscall bridge is not available in this VM")
    end
    local state, config = split_opts(opts, OPEN_STATE_KEYS, "knl.open")
    local handle = syscall.open({
        owner = state.owner,
        budget = state.budget,
        store = state.store,
    })
    return make_ctx(handle, config)
end

--- Resume a persisted session and return its ctx.
---
--- The state comes back from the store (`knl.resume` reopens the durable
--- stream and re-folds the log); the config does NOT — it is a non-
--- serializable closure bundle, so every process supplies its own default
--- policy here (core-loop-design.md §4: state is durable, config is
--- per-process).
---
--- @param opts table  { store = { sqlite = <path> }, session = <id>, budget?,
---                      llm?, tools?, tool_policy?, fold?, filters?,
---                      system?, max_turns? }
--- @return table ctx  an immutable knl ctx, pre-loaded with the log
function M.resume(opts)
    opts = opts or {}
    if syscall == nil then
        error("knl.resume: the knl syscall bridge is not available in this VM")
    end
    local state, config = split_opts(opts, RESUME_STATE_KEYS, "knl.resume")
    local handle = syscall.resume({
        store = state.store,
        session = state.session,
        budget = state.budget,
    })
    return make_ctx(handle, config)
end

-- ============================================================
-- beat — one complete beat (the primitive; there is no loop here)
-- ============================================================

--- Run the tool_use blocks of a response, closing every one with a
--- tool_result. What runs / is skipped is `config.tool_policy` (a
--- `fn(tc, out) -> action`), the success result is the handler's, and the
--- pair-closing record — including the machine-minimal error for an unknown
--- tool or a raising handler — is the kernel's. `out.beat` (set by beat
--- before this runs) stamps both halves of every pair.
---
--- @param ctx table  a knl ctx
--- @return table summary  one { call_id, name, ok } per tool_use
local function execute_tools(ctx, config, out)
    local summary = {}
    local tools = config.tools or {}
    local policy = config.tool_policy

    for _, block in ipairs(out.content or {}) do
        if block.type == "tool_use" then
            local call_id = tostring(block.id or "")
            local name = tostring(block.name or "")
            local args = block.input or {}

            -- Record the call before running it: a run that dies mid-tool
            -- leaves a history that says a call was made. (Event field name
            -- `turn` is the Rust bridge's — the beat rename there is a
            -- deferred ST.)
            ctx:append({
                kind = "tool_call",
                turn = out.beat,
                call_id = call_id,
                name = name,
                args = args,
            })

            -- Ask the policy whether to run.  A nil return is "no opinion"
            -- (default: run) — but a policy that RAISES is fail-closed: a
            -- gate written to veto tools must not fall open on its own bug,
            -- so the raise denies the call and the reason lands in the
            -- tool_result instead of vanishing.
            local action = "run"
            if policy then
                local ok_p, decided = pcall(policy, block, out)
                if not ok_p then
                    action = "denied (policy raised: " .. tostring(decided) .. ")"
                elseif decided ~= nil then
                    action = decided
                end
            end

            local ok, result
            if action ~= "run" then
                ok, result = false, "tool '" .. name .. "' " .. tostring(action) .. " by policy"
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

            ctx:append({
                kind = "tool_result",
                turn = out.beat,
                call_id = call_id,
                ok = ok,
                result = result,
            })

            summary[#summary + 1] = { call_id = call_id, name = name, ok = ok }
        end
    end

    return summary
end

--- One complete beat: gate, fold, filter, record, call, record + charge, run
--- its tools. Single argument — everything is read off the ctx (state +
--- config), and the ctx is never mutated. Per-beat variation is the
--- caller's: `knl.beat(ctx:with{ llm = strong })`.
---
--- Re-entrant and stateless: a beat is decided entirely by its ctx, so it
--- can be called from any driver, resumed, or interleaved (one ctx per
--- concurrent strand; one shared session is the kernel CAS's problem).
---
--- @param ctx table  a knl ctx (from knl.open / knl.resume / ctx:with)
--- @return table outcome  an `Outcome`
function M.beat(ctx)
    -- [0] gate ------------------------------------------------------------
    if type(ctx) ~= "table" and type(ctx) ~= "userdata" then
        return emit(Outcome.err("conf", "beat takes a ctx (from knl.open / knl.resume)"))
    end
    local okc, config = pcall(function()
        return ctx._config
    end)
    local handle = okc and ctx._handle or nil
    if config == nil or handle == nil then
        return emit(Outcome.err("conf", "not a knl ctx (build one with knl.open)"))
    end
    -- The llm is beat's to call, off the ctx config (open-time or with-time).
    if config.llm == nil then
        return emit(Outcome.err("conf", "no llm in ctx (open with llm=..., or derive ctx:with{ llm = ... })"))
    end
    -- Budget stop is a planned stop, not a failure: Ok before the call.
    if ctx:exhausted() then
        return emit(Outcome.ok({ budget_stopped = true }))
    end

    -- [1] request <- fold(events, config) ---------------------------------
    local fold_fn = config.fold or M.fold
    local folded_ok, request = pcall(fold_fn, ctx:events(), config)
    if not folded_ok then
        return emit(Outcome.err("conf", "fold failed: " .. tostring(request)))
    end

    -- [2] filter chain (fn(req) -> req) -----------------------------------
    if config.filters then
        for i, filter in ipairs(config.filters) do
            local filtered_ok, filtered = pcall(filter, request)
            if not filtered_ok then
                return emit(Outcome.err("filter", tostring(filtered)))
            end
            -- A filter's return replaces the request wholesale, so a filter
            -- that mutates and forgets to return (nil) or returns a
            -- non-table would corrupt the write-ahead record and the wire.
            -- Loud in prod, not just under the dev assert below.
            if type(filtered) ~= "table" then
                return emit(Outcome.err(
                    "filter",
                    "filter #" .. i .. " returned " .. type(filtered) .. " (a filter must return the request table)"
                ))
            end
            request = filtered
        end
    end
    -- Dev-mode contract on what actually goes to the llm — a custom fold or
    -- a filter that broke the request shape fails loud here, not in the wire.
    shape.assert_dev(request, REQUEST, "knl_request")

    -- [3] record the request write-ahead (open kind "request") ------------
    -- The request as actually sent is a fact in the history before the call,
    -- so a call that then fails leaves the request event behind.  An append
    -- can fail (closed session, CAS head conflict, validation) — beat's
    -- contract is an Outcome, so a state failure is Error("state"), never
    -- a raw raise.
    local rec_ok, rec_err = pcall(function()
        ctx:append({ kind = "request", request = request })
    end)
    if not rec_ok then
        return emit(Outcome.err("state", "request append failed: " .. tostring(rec_err)))
    end

    -- [4] beat calls the llm directly ------------------------------------
    -- resp = { status = "ok"|"refused"|"error", content, usage, stop_reason }.
    -- The status is the adapter's judgement; beat reads it, it does not
    -- invent one (status is llm-supplied).  The call is pcall'd: an adapter
    -- that raises (instead of returning nil, err) is still a call failure,
    -- not an escape from the Outcome contract.
    local call_ok, resp, berr = pcall(config.llm, request)
    if not call_ok then
        berr = "llm raised: " .. tostring(resp)
        resp = nil
    end

    -- [5] status branch — beat lays down the record and the charge ---------
    -- error / transport failure: the beat did not come off. Note it and stop.
    -- The failure note is best-effort (the state may be what failed); the
    -- call error stays the primary detail either way.
    if resp == nil or resp.status == "error" then
        local reason = berr or (resp and resp.detail) or "llm reported error"
        local noted_ok, note_err = pcall(function()
            ctx:append({ kind = "model_call_failed", error = tostring(reason) })
        end)
        if not noted_ok then
            return emit(Outcome.err(
                "state",
                "call failed (" .. tostring(reason) .. ") and the failure note could not be recorded: " .. tostring(note_err)
            ))
        end
        return emit(Outcome.err("call", tostring(reason)))
    end
    -- ok / refused: the model answered. Appending the model_response is
    -- what records it, numbers it and charges it — the kernel does all
    -- three in the one append (no Lua-side spend, no Lua-side counting).
    -- Read the kernel-assigned number back afterwards to stamp the tools.
    -- `usage` defaults to an empty count: the llm contract leaves it
    -- optional, but the kernel validator requires the field on a
    -- model_response (the Lua/Rust contract meet in the middle here).
    local resp_ok, resp_err = pcall(function()
        ctx:append({
            kind = "model_response",
            content = resp.content,
            usage = resp.usage or {},
            stop_reason = resp.stop_reason,
        })
    end)
    if not resp_ok then
        return emit(Outcome.err("state", "model_response append failed: " .. tostring(resp_err)))
    end
    resp.beat = ctx:turns()
    -- A refusal is a recorded, charged response the model would not build on.
    if resp.status == "refused" then
        return emit(Outcome.refused(resp.stop_reason or "refused", resp))
    end

    -- [6] tool execution (skeleton) --------------------------------------
    -- execute_tools raises only on a state failure (an append that did not
    -- land); handler/policy failures close their pair as data (ok=false).
    local tools_ok, tools_or_err = pcall(execute_tools, ctx, config, resp)
    if not tools_ok then
        return emit(Outcome.err("state", "tool record append failed: " .. tostring(tools_or_err)))
    end
    resp.tools = tools_or_err

    return emit(Outcome.ok(resp))
end

-- Internals exposed for the spec (the fixture drives these directly).
M._execute_tools = execute_tools
M._wire_tools = wire_tools
M._make_ctx = make_ctx

return M
