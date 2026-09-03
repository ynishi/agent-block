--- knl — the Lua kernel (POC skeleton of turn / run / fold / Outcome).
---
--- What this is
---   The driving half of the kernel/shell split: Rust is the pure syscall
---   layer (session / append / events / view / spend / close), and this
---   module runs a Turn — one model call plus the tools that call asks for —
---   over it, returning an `Outcome`. See `knl-kernel-design.md` rev 6 (§1
---   ctx-handle, §2 the full beat) for the design; this file is the POC
---   skeleton (§2 turn, §4 run, §5 fold, §8 Outcome), not the full one.
---
--- The B-plan shape (design rev 6, §1/§2)
---   `knl.turn(conf)` takes one argument. The state handle — the "ctx", an
---   fd-like handle onto History / Budget — travels in `conf.ctx`, so a
---   driver is free to build its own loop by carrying `conf` (ctx included)
---   between beats. turn does NOT use the composite `session:call`; it calls
---   `conf.backend(request)` itself and lays down the record (`ctx:append`),
---   the charge (`ctx:spend`) and the turn number in that order — turn owns
---   the beat, no syscall bundles the steps for it.
---
--- Turn numbering, and the bridge gap (report this)
---   The design has the ctx be the numbering authority via `ctx:next_turn()`.
---   The current Rust bridge (crates/agent-block-core/src/bridge/knl.rs) does
---   NOT expose that method (the Rust *core* has `Session::next_turn`, it is
---   simply unbound). So this POC numbers a turn by counting the
---   `model_response` events already in the ctx and adding one
---   (`next_turn_of`). This is the one place the implementation diverges from
---   §2's `ctx:next_turn()`.
---
--- What the POC deliberately leaves out
---   Real provider adapters (backend is a stub `fn(req) -> {status, content,
---   usage, stop_reason}` passed via conf), the full fold vocabulary, and
---   realistic tool / filter / backend-selection policies. The skeleton
---   carries only the invariants: Outcome's three values, a Turn that folds
---   events into a request, records it write-ahead, calls the backend
---   directly, records + charges the response itself, and closes every
---   tool_use with a tool_result; and a run driver that cannot loop forever.

--- The Rust syscall bridge, captured before `require("knl")` shadows the
--- name (see the header). `nil` in a VM that has no bridge (e.g. the pure
--- lspec runner): `fold` / `Outcome` / `turn` never touch it, and `run`
--- reports its absence rather than indexing nil.
local syscall = knl

local M = {}

-- ============================================================
-- Outcome (§8) — the result type of a turn / run
-- ============================================================
--
-- Plain-data status tag tables, never metatable methods: an Outcome crosses
-- the JSON boundary with the kernel, and a metatable does not survive the
-- round trip. Predicates and the match are free functions for the same
-- reason — the value carries data, the module carries behaviour.

local Outcome = {}

--- The status values the kernel knows. Exactly these three, provider-neutral.
local STATUSES = { "ok", "refused", "error" }

--- A turn / run that ran (a budget stop is one too — it ran and stopped on
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

--- The turn did not come off: the mechanism failed. `kind` is one of the
--- kernel's own failure points — "conf", "filter" or "call".
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
-- fold (§5) — events -> request (pure)
-- ============================================================

--- Render a tool_result's payload as request text: a string verbatim, any
--- other value JSON-encoded (the envelope already carries the rest).
local function result_text(result)
    if type(result) == "string" then
        return result
    end
    return std.json.encode(result)
end

--- The wire declarations for `conf.tools` (name -> { description,
--- input_schema, handler }), handler stripped: the request carries what the
--- model may call, not how to call it. Sorted by name so fold stays
--- deterministic (pure), which is what lets a KV cache hold across turns.
local function wire_tools(tools)
    local names = {}
    for name in pairs(tools) do
        names[#names + 1] = name
    end
    table.sort(names)
    local out = {}
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
--- `conf` and writes nothing. Three kinds map to messages and the rest are
--- skipped (tool_call / run_* / model_call_failed / request / open kinds):
---
---   msg_user       -> { role = "user", content = <verbatim> }
---   model_response -> { role = "assistant", content = <verbatim> }
---   tool_result    -> collected, in seq order, into the user message that
---                     follows the assistant turn they answer (consecutive
---                     tool_results batch together, which for a well-formed
---                     history is the same as grouping by turn)
---
--- `system` and `tools` are composed from conf each turn, not read from the
--- history.
---
--- @param events table  array of stored events (from `session:events()`)
--- @param conf table
--- @return table request  { system?, messages, tools? }
function M.fold(events, conf)
    conf = conf or {}
    local messages = {}
    local batch = {}

    local function flush()
        if #batch > 0 then
            messages[#messages + 1] = { role = "user", content = batch }
            batch = {}
        end
    end

    for _, ev in ipairs(events or {}) do
        local kind = ev.kind
        if kind == "msg_user" then
            flush()
            messages[#messages + 1] = { role = "user", content = ev.content }
        elseif kind == "model_response" then
            flush()
            messages[#messages + 1] = { role = "assistant", content = ev.content }
        elseif kind == "tool_result" then
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
    flush()

    local request = { messages = messages }
    if conf.system ~= nil then
        request.system = conf.system
    end
    if conf.tools ~= nil then
        request.tools = wire_tools(conf.tools)
    end
    return request
end

-- ============================================================
-- scope handle (§1 In, §2 single write path) + usage (§3 projection)
-- ============================================================
--
-- The ownership fix (scope-design.md). The old code stamped an `author`
-- (kernel|caller) from *which append method was called*, so turn's
-- model_response — laid down through the plain append path — read back as
-- author=caller and fell out of the account (usage=0, the confused-deputy
-- bug). Here ownership is intrinsic to a *scope handle*: `open` mints a
-- scope id once (§1), every write through the handle carries that scope
-- automatically (§2), and aggregation is a projection filtered by that
-- scope (§3). The consumer (turn) never decides ownership — it just writes
-- through the handle it was handed.
--
-- POC boundary (scope-design.md §5/§6, stated so it is not mistaken for the
-- finished thing):
--   * NOT enforced at the Rust boundary. A raw `ctx:append` can still
--     bypass scope stamping — the handle stamps scope in Lua, it is not yet
--     the *only* reachable write path. Making a raw append unable to skip
--     the stamp is the follow-up Rust hardening step, out of scope here;
--     this POC validates the *behaviour* of the model.
--   * NO multi-scope continuation. The lineage branch in `Handle:append`
--     (leave an already-set scope alone) is here so a carried-over event
--     keeps its origin scope, but nothing in this POC drives continuation
--     across scopes — that is the persistence step (§6.5), flagged only.

local Handle = {}
Handle.__index = Handle

--- The single consumer-facing write path (§2). Stamp this handle's scope
--- onto an event that does not carry one, then delegate to the underlying
--- ctx, returning the seq it assigns.
---
--- An event that *already* carries a scope keeps it: that is a carried-over
--- event holding its origin scope (§4 lineage), and overwriting it would
--- re-bill another scope's work to this one. The consumer sets no scope of
--- its own — routing the write through the handle is what stamps it.
---
--- Mutates `event` in place, by contract: auto-stamping scope on the way
--- through is the whole point of the single write path (a caller that keeps
--- a reference sees the scope the write was recorded under).
function Handle:append(event)
    if event.scope == nil then
        event.scope = self.scope
    end
    return self.ctx:append(event)
end

--- Read / budget / lifecycle delegate straight to the underlying ctx: the
--- handle adds ownership to writes, it does not reinterpret the rest.
function Handle:events(from)
    return self.ctx:events(from)
end

function Handle:spend(n)
    return self.ctx:spend(n)
end

function Handle:remaining()
    return self.ctx:remaining()
end

function Handle:exhausted()
    return self.ctx:exhausted()
end

function Handle:close(reason)
    return self.ctx:close(reason)
end

function Handle:id()
    return self.ctx:id()
end

--- Open a scope (§1 In) and return its handle — the only capability the
--- consumer needs to write to the scope.
---
--- The session already mints a UUID (`ctx:id()`); that id *is* the scope id
--- (no extra field — the session's identity and the scope's are one). owner
--- / user are the scope's incidental identity (whose scope it is, for later
--- multi-app separation) and do not affect accounting.
---
--- @param opts table  { owner?, user?, budget?, backend? }
--- @return table handle  { scope, owner, user, ctx } + Handle methods
function M.open(opts)
    opts = opts or {}
    if syscall == nil then
        error("knl.open: the knl syscall bridge is not available in this VM")
    end
    local ctx = syscall.session({ budget = opts.budget, backend = opts.backend })
    return setmetatable({
        ctx = ctx,
        scope = ctx:id(),
        owner = opts.owner,
        user = opts.user,
    }, Handle)
end

--- Usage as a scope-keyed projection (§3). Fold the handle's events and
--- count only the model_response events stamped with *this* handle's scope:
--- those are the calls this scope made and paid for. A model_response
--- carrying a different scope (a carried-over, lineage event) stays in the
--- record but out of this account.
---
--- This is the design's "aggregation is a projection filtered by scope",
--- and the reason it does not lean on the Rust `view("usage")`: that view
--- keys on `author`, the stamp the model replaced — a turn's own response,
--- written through the plain path, reads back author=caller and the Rust
--- view scores it 0. Keying on scope is what makes the count come out right.
---
--- @param handle table  a handle from `M.open`
--- @return table  { input_tokens, output_tokens, thinking_tokens, model_calls }
function M.usage(handle)
    local totals = {
        input_tokens = 0,
        output_tokens = 0,
        thinking_tokens = 0,
        model_calls = 0,
    }
    for _, ev in ipairs(handle:events()) do
        if ev.kind == "model_response" and ev.scope == handle.scope then
            totals.model_calls = totals.model_calls + 1
            local usage = ev.usage
            if type(usage) == "table" then
                for _, counter in ipairs({ "input_tokens", "output_tokens", "thinking_tokens" }) do
                    local n = usage[counter]
                    if type(n) == "number" then
                        totals[counter] = totals[counter] + n
                    end
                end
            end
        end
    end
    return totals
end

-- ============================================================
-- turn (§2) — one complete beat
-- ============================================================

--- The three usage counters the budget charges on, summed. Mirrors the Rust
--- `ModelResult::charge` (input + output + thinking, a missing counter is 0
--- and the total floors at 0), because turn now does the charge itself with
--- `ctx:spend` rather than leaning on the composite `session:call`.
local function tokens_of(usage)
    if type(usage) ~= "table" then
        return 0
    end
    local total = 0
    for _, counter in ipairs({ "input_tokens", "output_tokens", "thinking_tokens" }) do
        local n = usage[counter]
        if type(n) == "number" and n > 0 then
            total = total + n
        end
    end
    return total
end

--- The turn number the next recorded response takes — scope-keyed.
---
--- The design (§2) reads this off the ctx (`ctx:next_turn()`), which is the
--- Rust authority. The bridge does not expose that method today, so the POC
--- derives the same number the Rust core would: one past the count of
--- `model_response` events already in the history *under this scope*
--- (scope-design.md §2/§3 — numbering is scope-keyed, exactly like usage).
--- Called before the append, so a fresh scope's first turn is 1.
---
--- `ctx.scope` is the handle's scope; a model_response from a different
--- scope (lineage) does not advance this scope's counter. A raw session
--- brought in without a handle reads `ctx.scope == nil`, and its unstamped
--- model_responses (`ev.scope == nil`) match it — the pre-handle path keeps
--- numbering as it did.
local function next_turn_of(ctx)
    local n = 0
    local scope = ctx.scope
    for _, ev in ipairs(ctx:events()) do
        if ev.kind == "model_response" and ev.scope == scope then
            n = n + 1
        end
    end
    return n + 1
end

--- Minimal conf shape check (§2 [0] ShapePort). Returns an error string, or
--- nil when the conf is usable.
local function conf_problem(conf)
    if type(conf) ~= "table" then
        return "conf must be a table"
    end
    if conf.filters ~= nil and type(conf.filters) ~= "table" then
        return "conf.filters must be an array of functions"
    end
    if conf.tools ~= nil and type(conf.tools) ~= "table" then
        return "conf.tools must be a table"
    end
    if conf.tool_policy ~= nil and type(conf.tool_policy) ~= "function" then
        return "conf.tool_policy must be a function"
    end
    if conf.fold ~= nil and type(conf.fold) ~= "function" then
        return "conf.fold must be a function"
    end
    return nil
end

--- Run the tool_use blocks of a response, closing every one with a
--- tool_result (§2 [6], skeleton). What runs / is skipped is `conf.tool_policy`
--- (a `fn(tc, out) -> action`), the success result is the handler's, and the
--- pair-closing record — including the machine-minimal error for an unknown
--- tool or a raising handler — is the kernel's. `out.turn` (set by turn
--- before this runs) stamps both halves of every pair.
---
--- @param ctx userdata  a `knl.session()` handle (conf.ctx)
--- @return table summary  one { call_id, name, ok } per tool_use
local function execute_tools(ctx, conf, out)
    local summary = {}
    local tools = conf.tools or {}
    local policy = conf.tool_policy

    for _, block in ipairs(out.content or {}) do
        if block.type == "tool_use" then
            local call_id = tostring(block.id or "")
            local name = tostring(block.name or "")
            local args = block.input or {}

            -- Record the call before running it: a run that dies mid-tool
            -- leaves a history that says a call was made.
            ctx:append({
                kind = "tool_call",
                turn = out.turn,
                call_id = call_id,
                name = name,
                args = args,
            })

            -- Ask the policy whether to run (default: run).
            local action = "run"
            if policy then
                local ok_p, decided = pcall(policy, block, out)
                if ok_p and decided ~= nil then
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
                turn = out.turn,
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
--- its tools (§2, B-plan). turn calls the backend itself and lays down the
--- record / charge / turn number — no `session:call` bundles the steps.
---
--- Re-entrant and stateless: it is decided entirely by `conf` (the ctx
--- handle included), so it can be called from any driver, resumed, or
--- interleaved. The state handle is `conf.ctx`; turn never opens it.
---
--- @param conf table  { ctx = <knl.session handle>, backend, fold?, filters?,
---                      tools?, tool_policy?, system? }
--- @return table outcome  an `Outcome` (§8)
function M.turn(conf)
    conf = conf or {}

    -- [0] gate ------------------------------------------------------------
    local problem = conf_problem(conf)
    if problem then
        return Outcome.err("conf", problem)
    end
    local ctx = conf.ctx
    if ctx == nil then
        return Outcome.err("conf", "no ctx (pass conf.ctx, a knl.session handle)")
    end
    -- The backend is turn's to call now, so it must be in conf. There is no
    -- session-bound fallback here — that path belonged to `session:call`.
    if conf.backend == nil then
        return Outcome.err("conf", "no backend (pass conf.backend)")
    end
    -- Budget stop is a planned stop, not a failure: Ok before the call.
    if ctx:exhausted() then
        return Outcome.ok({ budget_stopped = true })
    end

    -- [1] request <- fold(events, conf) -----------------------------------
    local fold_fn = conf.fold or M.fold
    local folded_ok, request = pcall(fold_fn, ctx:events(), conf)
    if not folded_ok then
        return Outcome.err("conf", "fold failed: " .. tostring(request))
    end

    -- [2] filter chain (fn(req) -> req) -----------------------------------
    if conf.filters then
        for _, filter in ipairs(conf.filters) do
            local filtered_ok, filtered = pcall(filter, request)
            if not filtered_ok then
                return Outcome.err("filter", tostring(filtered))
            end
            request = filtered
        end
    end

    -- [3] record the request write-ahead (open kind "request") ------------
    -- The request as actually sent is a fact in the history before the call,
    -- so a call that then fails leaves the request event behind (§6).
    ctx:append({ kind = "request", request = request })

    -- [4] turn calls the backend directly ---------------------------------
    -- resp = { status = "ok"|"refused"|"error", content, usage, stop_reason }.
    -- The status is the adapter's judgement; turn reads it, it does not
    -- invent one (§8, status is backend-supplied).
    local resp, berr = conf.backend(request)

    -- [5] status branch — turn lays down the record and the charge ---------
    -- error / transport failure: the beat did not come off. Note it and stop.
    if resp == nil or resp.status == "error" then
        local reason = berr or (resp and resp.detail) or "backend reported error"
        ctx:append({ kind = "model_call_failed", error = tostring(reason) })
        return Outcome.err("call", tostring(reason))
    end
    -- ok / refused: the model answered. Record it, then charge it
    -- (write-ahead: append before spend), numbering the turn ourselves.
    local turn_no = next_turn_of(ctx)
    resp.turn = turn_no
    ctx:append({
        kind = "model_response",
        turn = turn_no,
        content = resp.content,
        usage = resp.usage,
        stop_reason = resp.stop_reason,
    })
    ctx:spend(tokens_of(resp.usage))
    -- A refusal is a recorded, charged response the model would not build on.
    if resp.status == "refused" then
        return Outcome.refused(resp.stop_reason or "refused", resp)
    end

    -- [6] tool execution (skeleton) --------------------------------------
    resp.tools = execute_tools(ctx, conf, resp)

    return Outcome.ok(resp)
end

-- ============================================================
-- run (§4) — the standard driver: while turn
-- ============================================================

--- Whether a response asks for at least one tool.
local function has_tool_use(out)
    for _, block in ipairs(out.content or {}) do
        if block.type == "tool_use" then
            return true
        end
    end
    return false
end

--- The standard driver: open (or take) a session, seed the input, and beat
--- until the model settles, the budget stops it, or a turn fails (§4).
---
--- @param conf table {
---   budget    (optional) { tokens = N } — opens a ctx with this budget
---   max_turns (optional) cap on the number of model calls this run makes
---   ctx       (optional) a ctx handle to continue (not closed by run);
---             `session` is accepted as an alias for a brought-in handle
---   backend   (optional) POC stub fn(req) -> { status, content, usage, stop_reason }
---   input     (optional) first user message, appended before the first beat
---   system / tools / filters / tool_policy / fold — passed to each turn
--- }
--- Either `budget` or `max_turns` (or a brought-in ctx with a budget) must
--- bound the run: neither is Error(conf) at the door — the one place run
--- refuses to loop forever.
---
--- run is the sugar that opens a ctx when conf.ctx is absent (§1/§4); turn
--- itself never does. The opened ctx is placed on `conf.ctx` and handed to
--- every beat, and closed on the way out. A brought-in ctx is left open.
---
--- @return table outcome  an `Outcome` (§8)
--- @return userdata|nil ctx  the (closed, when run opened it) ctx handle
function M.run(conf)
    conf = conf or {}
    if type(conf) ~= "table" then
        return Outcome.err("conf", "conf must be a table"), nil
    end

    -- The state handle: conf.ctx is the design name, conf.session an alias
    -- for a handle a caller brings in.
    local ctx = conf.ctx or conf.session

    -- Infinite-loop prevention, at the door: bounded by max_turns, by
    -- conf.budget, or by a brought-in ctx that already has a budget.
    -- A self-written `while knl.turn` loop does not pass through here, so its
    -- finiteness is the writer's responsibility.
    local bounded_by_budget = conf.budget ~= nil or (ctx ~= nil and ctx.remaining and ctx:remaining() ~= nil)
    if conf.max_turns == nil and not bounded_by_budget then
        return Outcome.err("conf", "run needs a budget or max_turns (infinite-loop prevention)"), ctx
    end

    -- Acquire the ctx: continue a brought-in one (never closed here), or open
    -- our own (closed on the way out). run opens a *scope handle* via
    -- M.open (§1/§4), so run drives a scope like turn does — its writes get
    -- scope-stamped automatically. A brought-in handle is used as-is.
    local own_ctx = false
    if ctx == nil then
        if syscall == nil then
            return Outcome.err("conf", "knl syscall bridge is not available in this VM"), nil
        end
        local opened_ok, opened = pcall(M.open, {
            budget = conf.budget,
            backend = conf.backend,
        })
        if not opened_ok then
            return Outcome.err("conf", "ctx open failed: " .. tostring(opened)), nil
        end
        ctx = opened
        own_ctx = true
    end

    -- Hand the ctx to each beat through conf.ctx, the handle turn reads.
    conf.ctx = ctx

    -- Seed the first user message (sugar for the initial input).
    if conf.input ~= nil then
        ctx:append({ kind = "msg_user", content = conf.input })
    end

    local function finish(outcome)
        if own_ctx then
            ctx:close("done")
        end
        return outcome, ctx
    end

    local calls = 0
    while true do
        local o = M.turn(conf)

        if Outcome.is_error(o) then
            return finish(o) -- the beat did not come off; run is Error too
        end
        if Outcome.is_refused(o) then
            return finish(o) -- cannot proceed; run is Refused too
        end

        -- From here it is Ok. The gate may have stopped on the budget before
        -- calling; that is a stop, not a step.
        if o.out and o.out.budget_stopped then
            return finish(o)
        end

        calls = calls + 1

        -- Continue only while the model is still asking for tools; a plain
        -- answer means the conversation settled.
        if not has_tool_use(o.out) then
            return finish(o)
        end

        -- max_turns is a run concept, checked here rather than in the gate.
        if conf.max_turns ~= nil and calls >= conf.max_turns then
            return finish(o)
        end
    end
end

-- Internals exposed for the spec (the fixture drives these directly).
M._execute_tools = execute_tools
M._wire_tools = wire_tools

return M
