--- knl — the Lua kernel (POC skeleton of turn / run / fold / Outcome).
---
--- What this is
---   The driving half of the kernel/shell split: Rust is the pure syscall
---   layer (session / append / events / view / spend / close / call /
---   has_backend), and this module runs a Turn — one model call plus the
---   tools that call asks for — over it, returning an `Outcome`. See
---   `knl-kernel-design.md` rev 5 for the whole design; this file is the
---   POC skeleton (§2 turn, §4 run, §5 fold, §8 Outcome), not the full one.
---
--- The name collision (design §0.5)
---   The Rust syscall bridge installs itself as the global `knl`. This module
---   is loaded as `require("knl")`, so a caller writes `local knl =
---   require("knl")` and shadows that global in its own scope. To keep the
---   two apart the module captures the bridge here, at load time, before the
---   name is shadowed — the design slates the bridge to be renamed `syscall`,
---   and this indirection is what lets both share the name until then.
---
--- What the POC deliberately leaves out
---   Real provider adapters (backend is a stub `fn(req) -> {status, content,
---   usage, stop_reason}` passed via conf), the full fold vocabulary, and
---   realistic tool / filter / backend-selection policies. The skeleton
---   carries only the invariants: Outcome's three values, a Turn that folds
---   events into a request, records it write-ahead, calls, and closes every
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
-- turn (§2) — one complete beat
-- ============================================================

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
--- tool_result (§2 [5], skeleton). What runs / is skipped is `conf.tool_policy`
--- (a `fn(tc, out) -> action`), the success result is the handler's, and the
--- pair-closing record — including the machine-minimal error for an unknown
--- tool or a raising handler — is the kernel's.
---
--- @return table summary  one { call_id, name, ok } per tool_use
local function execute_tools(session, conf, out)
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
            session:append({
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

            session:append({
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

--- One complete beat: gate, fold, filter, record, call, run its tools (§2).
---
--- Re-entrant and stateless: it is decided entirely by `(session, conf)`, so
--- it can be called from any driver, resumed, or interleaved.
---
--- @param session userdata  a `knl.session()` (the Rust bridge)
--- @param conf table
--- @return table outcome  an `Outcome` (§8)
function M.turn(session, conf)
    conf = conf or {}

    -- [0] gate ------------------------------------------------------------
    local problem = conf_problem(conf)
    if problem then
        return Outcome.err("conf", problem)
    end
    -- A backend must be reachable: brought by conf (the POC stub) or bound
    -- to the session. The stub means the session need not have one of its
    -- own, which is the property the POC is checking.
    local backend_available = conf.backend ~= nil or session:has_backend()
    if not backend_available then
        return Outcome.err("conf", "no backend available (pass conf.backend or open the session with one)")
    end
    -- Budget stop is a planned stop, not a failure: Ok before the call.
    if session:exhausted() then
        return Outcome.ok({ budget_stopped = true })
    end

    -- [1] request <- fold(events, conf) -----------------------------------
    local fold_fn = conf.fold or M.fold
    local folded_ok, request = pcall(fold_fn, session:events(), conf)
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
    session:append({ kind = "request", request = request })

    -- [4] call ------------------------------------------------------------
    -- The current `session:call` IF returns { turn, content, usage,
    -- stop_reason, remaining, exhausted } — it drops any `status` the backend
    -- returns (Rust `validate_backend_result`). So when the backend is the
    -- POC stub (which returns a status), we wrap it: the wrapper captures the
    -- status and hands `session:call` only the fields it records, and we read
    -- the status back after the call. A status of "error" is turned into a
    -- failed call so no model_response is recorded for it.
    local captured_status
    local out, call_err
    if conf.backend then
        local wrapped = function(req)
            local raw, raw_err = conf.backend(req)
            if raw == nil then
                return nil, raw_err
            end
            captured_status = raw.status
            if raw.status == "error" then
                return nil, raw.detail or "backend reported error"
            end
            return { content = raw.content, usage = raw.usage, stop_reason = raw.stop_reason }
        end
        out, call_err = session:call(request, { backend = wrapped })
    else
        -- Session-bound backend: the current IF gives no status back, so a
        -- successful call is taken as Ok (see the POC take-aways).
        out, call_err = session:call(request)
        captured_status = out and "ok" or nil
    end

    if out == nil then
        -- Transport failure, an unrecordable result, or status == "error":
        -- the beat did not come off.
        return Outcome.err("call", tostring(call_err))
    end

    -- The kernel does not invent the status; it loads what the backend gave.
    if captured_status == "refused" then
        return Outcome.refused(out.stop_reason or "refused", out)
    end

    -- [5] tool execution (skeleton) --------------------------------------
    out.tools = execute_tools(session, conf, out)

    return Outcome.ok(out)
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
---   budget    (optional) { tokens = N } — opens a session with this budget
---   max_turns (optional) cap on the number of model calls this run makes
---   session   (optional) a session to continue (not closed by run)
---   backend   (optional) POC stub fn(req) -> { status, content, usage, stop_reason }
---   input     (optional) first user message, appended before the first beat
---   system / tools / filters / tool_policy / fold — passed to each turn
--- }
--- Either `budget` or `max_turns` (or a brought-in session with a budget)
--- must bound the run: neither is Error(conf) at the door — the one place
--- run refuses to loop forever.
---
--- @return table outcome  an `Outcome` (§8)
--- @return userdata|nil session  the (closed, when run opened it) session
function M.run(conf)
    conf = conf or {}
    if type(conf) ~= "table" then
        return Outcome.err("conf", "conf must be a table"), nil
    end

    local session = conf.session

    -- Infinite-loop prevention, at the door: bounded by max_turns, by
    -- conf.budget, or by a brought-in session that already has a budget.
    -- A self-written `while knl.turn` loop does not pass through here, so its
    -- finiteness is the writer's responsibility.
    local bounded_by_budget = conf.budget ~= nil
        or (session ~= nil and session.remaining and session:remaining() ~= nil)
    if conf.max_turns == nil and not bounded_by_budget then
        return Outcome.err("conf", "run needs a budget or max_turns (infinite-loop prevention)"), session
    end

    -- Acquire the session: continue a brought-in one (never closed here), or
    -- open our own (closed on the way out).
    local own_session = false
    if session == nil then
        if syscall == nil then
            return Outcome.err("conf", "knl syscall bridge is not available in this VM"), nil
        end
        local opts = {}
        if conf.budget ~= nil then
            opts.budget = conf.budget
        end
        local opened_ok, opened = pcall(syscall.session, opts)
        if not opened_ok then
            return Outcome.err("conf", "session open failed: " .. tostring(opened)), nil
        end
        session = opened
        own_session = true
    end

    -- Seed the first user message (sugar for the initial input).
    if conf.input ~= nil then
        session:append({ kind = "msg_user", content = conf.input })
    end

    local function finish(outcome)
        if own_session then
            session:close("done")
        end
        return outcome, session
    end

    local calls = 0
    while true do
        local o = M.turn(session, conf)

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
