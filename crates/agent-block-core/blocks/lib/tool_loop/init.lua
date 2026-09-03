--- tool_loop — a ReAct loop over exactly the tools you hand it.
---
--- Purpose
---   Between "call the LLM once" (`llm_proto`) and "run an agent"
---   (`blocks/agent`) there was nothing. A block that wanted to iterate with
---   two specific tools had to either take the whole agent — MCP connections,
---   registry sweep, resource/prompt tools, token budgets — or write its own
---   loop. `compile_loop` wrote its own. This is the missing middle.
---
--- What it does
---   Call the model, dispatch the tool_use blocks it returns, append the
---   results, call again. Stop when the model stops asking for tools.
---
--- What it deliberately does not do
---   Read the tool registry. Connect to MCP servers. Track a token budget.
---   Expose resources or prompts. Load anything by name. The tool set is the
---   argument — a capability that was not passed in cannot be reached, and
---   that is the property callers are buying by using this instead of an
---   agent.
---
--- Adaptive tool sets
---   `tools` may be a function evaluated once per turn, so the set can shrink,
---   grow, or swap as the run progresses (drop the write tool after repeated
---   no-op turns, widen a group once a file has been read, add a tool that
---   only makes sense after the first result).
---
--- Kernel session (optional)
---   Pass `session` and the model call goes through it: `s:call` records the
---   response and charges its tokens before it returns, so there is no
---   arrangement of this loop's code in which a call happened and the history
---   does not say so. The loop records the facts around it — the prompt, every
---   tool call and its result — and keeps no budget of its own: it asks the
---   session whether anything is left before opening another turn, and stops
---   with `stop_reason = "budget_exhausted"` when there is not. Without a
---   session none of this happens and the result is what it always was.
---
--- The model call
---   Whatever the wire needs — the provider dialect, the retries, the parse —
---   belongs to `llm_proto.backend`, and this loop holds one of those closures
---   rather than any provider knowledge. A session that was opened with a
---   backend of its own uses that one instead; one that was not is handed this
---   loop's, per call, so a caller that has not been rewired keeps working.
---
--- Usage
---   local loop = require("tool_loop")
---   local res = loop.run({
---       system = "...",
---       prompt = "...",
---       tools  = { read_spec, edit_spec },   -- or function(ctx) -> specs
---       llm    = { provider = "anthropic", model = "..." },
---   })

local proto = require("llm_proto")

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

local M = {}

--- What `run` hands back.
---
--- Closed, and deliberately not split into ok / error alternatives the way
--- `agent.run`'s is: a refusal comes back with `ok = false` *and* the content
--- the model produced before refusing, so the two are not exclusive here. What
--- the shape pins instead is the set of keys and the type of each — `ok`,
--- `turns`, `tool_calls` and `messages` are on every one of the ten return
--- paths, and the rest depend on how far the loop got.
---
--- `usage` stays `T.table` rather than the agent's usage shape: the two early
--- returns fire before a tracker exists, and tightening it would describe the
--- accounting rather than this boundary.
---
--- Checked only in dev mode (LSHAPE_CHECK=1).
local RESULT = T.shape({
    ok = T.boolean,
    turns = T.number,
    tool_calls = T.array_of(T.table),
    messages = T.array_of(T.table),
    content = T.string:is_optional(),
    error = T.string:is_optional(),
    usage = T.table:is_optional(),
    stop_reason = T.string:is_optional(),
    stop_details = T.table:is_optional(),
}, { open = false })

--- The contract this module holds itself to, as data.
M.shapes = {
    result = RESULT,
}

--- Turns before the loop gives up on the model ever finishing.
local DEFAULT_MAX_TURNS = 16

--- Retries for transient API failures (rate limit / overload / 5xx).
local DEFAULT_MAX_RETRIES = 2

-- ============================================================
-- Internal
-- ============================================================

--- Resolve the tool set for this turn.
---
--- An array is used as-is; a function is called with the turn context so the
--- caller can decide from what has happened so far.
---
--- @param tools table|function
--- @param ctx table  { turn, last_tool_calls, state }
--- @return table specs, table by_name
local function resolve_tools(tools, ctx)
    local specs = tools
    if type(tools) == "function" then
        specs = tools(ctx) or {}
    end
    local by_name = {}
    for _, spec in ipairs(specs) do
        by_name[spec.name] = spec
    end
    return specs, by_name
end

--- Strip handlers: the wire only carries the declaration.
local function wire_tools(specs)
    local out = {}
    for _, spec in ipairs(specs) do
        table.insert(out, {
            name = spec.name,
            description = spec.description,
            input_schema = spec.input_schema,
        })
    end
    return out
end

--- Run one tool call and render its result as tool_result text.
---
--- A name outside this turn's set is answered, not raised: the model gets to
--- see that the tool does not exist and pick another, which is the same
--- recovery path as any other tool error.
local function dispatch(by_name, block)
    local spec = by_name[block.name]
    if not spec then
        return "ERROR: unknown tool '" .. tostring(block.name) .. "'", true
    end
    local ok, res, res_is_error = pcall(spec.handler, block.input or {})
    if not ok then
        return "ERROR: " .. tostring(res), true
    end
    if type(res) == "table" then
        local enc_ok, enc = pcall(std.json.encode, res)
        return enc_ok and enc or tostring(res), res_is_error == true or res.ok == false
    end
    -- A handler that returns plain text says so with the second value; a table
    -- can also carry `ok = false`.
    return tostring(res), res_is_error == true
end

--- Concatenate the text blocks of a decoded response.
local function text_of(content)
    local parts = {}
    for _, block in ipairs(content or {}) do
        if block.type == "text" and block.text then
            table.insert(parts, block.text)
        end
    end
    return table.concat(parts, "\n")
end

--- Record `event` in the session, or do nothing when there is none.
---
--- Every call site records before the loop's own state moves past the fact, so
--- a run that dies mid-turn leaves a history that says how far it got rather
--- than one that trails it. A session that refuses the write — a closed run, an
--- event the kernel does not accept — ends the run with that reason: continuing
--- would leave a hole in the record that nothing downstream could see.
---
--- @param session table|nil
--- @param event table
--- @return string|nil  error message, or nil when the write landed
local function record(session, event)
    if not session then
        return nil
    end
    local ok, err = pcall(function()
        session:append(event)
    end)
    if ok then
        return nil
    end
    return "session append failed: " .. tostring(err)
end

-- ============================================================
-- Public
-- ============================================================

--- Run the loop.
---
--- @param opts table {
---   prompt   (required) initial user message
---   system   (optional) system prompt
---   tools    (optional) array of { name, description, input_schema, handler },
---            where handler(input) returns text or a table, optionally with a
---            second return value marking the result as an error,
---            or function(ctx) returning one. Same shape `std.fs.tool_specs`
---            returns, so specs can be passed straight through.
---   messages (optional) prior turns to continue from
---   max_turns   (optional, default 16)
---   max_retries (optional, default 2) transient API failures only
---   session  (optional) kernel session (`knl.session`). When given, the model
---            call goes through `s:call`, which records `model_response` and
---            charges its tokens before it returns; the loop records
---            `msg_user` / `tool_call` / `tool_result` around it, each before
---            it advances past the fact, with the turn the kernel stamped. A
---            run that exhausts the budget stops before the next turn with
---            `ok = true, stop_reason = "budget_exhausted"`; a session that
---            refuses a write ends the run with `ok = false`.
---   state    (optional) caller value handed to the tools function and on_turn
---   on_turn  (optional) function({ turn, content, tool_calls, usage, decoded,
---            state }), fired once per model call including paused ones.
---            Returning false stops the loop: termination policy
---            beyond "the model stopped asking" belongs to the caller (token
---            budgets, wall clock, an external signal).
---   on_request  (optional) function({ turn, url, headers, body, body_json })
---            fired before the call; `body` is the table, `body_json` the wire bytes
---   on_response (optional) function({ turn, status, body, headers, latency_ms })
---            Observability only — the loop does not read what they return.
---            Both are the backend's hooks with this loop's turn added, so a
---            session that brought a backend of its own fires neither: the
---            wire it talks is that backend's business, and its hooks are too.
---   llm      (optional) { provider, model, base_url, api_key, api_key_env,
---                         max_tokens, temperature, thinking, tool_choice,
---                         dialect, timeout, ... } — the conf of the backend
---                         this loop builds (see `llm_proto.backend`)
--- }
--- @return table {
---   ok, content, turns, tool_calls, usage, messages, stop_reason, error?
--- }
---
--- The contract is checked on the way out in dev mode (LSHAPE_CHECK=1); see
--- `M.shapes.result`. Wrapped rather than asserted at each `return` because
--- there are ten of them.
function M.run(opts)
    return shape.assert_dev(M._run_impl(opts), RESULT, "tool_loop.run result")
end

function M._run_impl(opts)
    opts = opts or {}
    if type(opts.prompt) ~= "string" or opts.prompt == "" then
        return { ok = false, error = "prompt is required", turns = 0, tool_calls = {}, messages = {} }
    end

    local llm = opts.llm or {}
    local max_turns = tonumber(opts.max_turns) or DEFAULT_MAX_TURNS
    local max_retries = tonumber(opts.max_retries) or DEFAULT_MAX_RETRIES
    local state = opts.state
    local session = opts.session

    -- The turn the observability hooks report, and the reply the backend
    -- parsed. Both are written once per turn, just before the call, and read
    -- from inside it: the backend is handed neither, because neither is
    -- anything the wire needs.
    local current_turn = 0
    local parsed = nil

    --- The caller's hook with the turn the backend cannot know added.
    local function relay(hook)
        if not hook then
            return nil
        end
        return function(info)
            info.turn = current_turn
            hook(info)
        end
    end

    -- `llm` is forwarded whole rather than through a whitelist: the adapters
    -- already drop what their provider does not accept, and a whitelist here
    -- would silently strip every knob added upstream.
    local backend_conf = {}
    for k, v in pairs(llm) do
        backend_conf[k] = v
    end
    backend_conf.max_retries = max_retries
    backend_conf.on_request = relay(opts.on_request)
    backend_conf.on_response = relay(opts.on_response)
    backend_conf.on_decoded = function(decoded)
        parsed = decoded
    end

    -- Built even when the session brings its own: an unusable provider is a
    -- refusal to start rather than a failure five turns in.
    local backend, berr = proto.backend(backend_conf)
    if not backend then
        return { ok = false, error = berr, turns = 0, tool_calls = {}, messages = {} }
    end

    --- Ask the model: through the session when there is one, so the answer is
    --- recorded and charged before it comes back here.
    ---
    --- Which backend makes the call is the session's answer, not a guess: a
    --- session opened with one of its own uses it, and one that was not is
    --- handed this loop's, per call. Asked once per turn rather than
    --- remembered, because the answer cannot change and there is nothing to
    --- gain from caching it.
    ---
    --- @param req table  provider-neutral request
    --- @return table|nil out, string|nil err
    local function ask(req)
        if not session then
            return backend(req)
        end
        if session:has_backend() then
            return session:call(req)
        end
        return session:call(req, { backend = backend })
    end

    local messages = {}
    for _, m in ipairs(opts.messages or {}) do
        table.insert(messages, m)
    end

    local all_tool_calls = {}
    local usage = { input_tokens = 0, output_tokens = 0, thinking_tokens = 0 }
    local last_content = ""
    local last_stop_reason = nil
    local last_tool_calls = {}

    --- The result for a run that cannot go on — a session that refused a
    --- write, a model call that produced no answer. Named because the run can
    --- hit it at five points and they all report the same thing.
    local function stopped(reason, turns_done)
        return {
            ok = false,
            error = reason,
            turns = turns_done,
            tool_calls = all_tool_calls,
            usage = usage,
            messages = messages,
        }
    end

    -- Recorded before the prompt becomes part of the conversation.
    local prompt_err = record(session, { kind = "msg_user", content = opts.prompt })
    if prompt_err then
        return stopped(prompt_err, 0)
    end
    table.insert(messages, { role = "user", content = opts.prompt })

    for turn = 1, max_turns do
        -- The budget is charged as each response is recorded, so a run that
        -- has used it up stops here rather than opening another turn. `ok` is
        -- true because nothing failed: the allowance ran out, and the history
        -- says exactly where, which is what makes the run resumable.
        if session and session:exhausted() then
            return {
                ok = true,
                content = last_content,
                turns = turn - 1,
                tool_calls = all_tool_calls,
                usage = usage,
                messages = messages,
                stop_reason = "budget_exhausted",
            }
        end

        local specs, by_name = resolve_tools(opts.tools or {}, {
            turn = turn,
            last_tool_calls = last_tool_calls,
            state = state,
        })

        -- Provider-neutral: what to ask, not how to ask it. The backend turns
        -- this into a request, and a session that brought its own turns it
        -- into whatever that one speaks.
        current_turn = turn
        parsed = nil
        local out, ask_err = ask({
            messages = messages,
            system = opts.system,
            tools = wire_tools(specs),
        })

        -- One shape for every way a call can produce nothing: the transport
        -- failed, the provider refused the request, the session would not
        -- record the answer. The run ends either way, and the message says
        -- which it was.
        if not out then
            return stopped(ask_err, turn - 1)
        end

        -- The answer as the backend parsed it, which carries what the neutral
        -- result does not: `stop_details`, and the provider extras `on_turn`
        -- hands on. A session that brought its own backend leaves only the
        -- kernel's checked copy — three fields, and no way to reach behind
        -- them, which is the price of not knowing whose wire it was.
        local answer = parsed or out
        local u = answer.usage or {}
        -- The kernel owns the numbering once there is a session, so the facts
        -- around the response are filed under the turn it stamped rather than
        -- under this loop's count, which restarts every run.
        local model_turn = out.turn or turn

        -- Appended verbatim: Anthropic requires thinking blocks to come back
        -- unmodified during tool use, so the content is never filtered here.
        table.insert(messages, { role = "assistant", content = answer.content })

        usage.input_tokens = usage.input_tokens + (u.input_tokens or 0)
        usage.output_tokens = usage.output_tokens + (u.output_tokens or 0)
        usage.thinking_tokens = usage.thinking_tokens + (u.thinking_tokens or 0)

        last_content = text_of(answer.content)
        last_stop_reason = answer.stop_reason

        local calls = {}
        for _, block in ipairs(answer.content or {}) do
            if block.type == "tool_use" then
                table.insert(calls, block)
            end
        end
        last_tool_calls = calls

        -- A refusal is not an empty answer; report it rather than looping.
        if answer.stop_reason == "refusal" then
            return {
                ok = false,
                error = "model refused to respond",
                content = last_content,
                turns = turn,
                tool_calls = all_tool_calls,
                usage = usage,
                messages = messages,
                stop_reason = answer.stop_reason,
                stop_details = answer.stop_details,
            }
        end

        -- Fired once per model call, before any branching: a caller counting
        -- tokens or logging turns has to see every call, including the paused
        -- ones. Returning false stops the run, and doing it here means the
        -- caller does not pay for tool calls it has decided not to continue past.
        if opts.on_turn then
            local cb_ok, cb_res = pcall(opts.on_turn, {
                turn = turn,
                content = last_content,
                tool_calls = calls,
                usage = answer.usage,
                decoded = answer,
                state = state,
            })
            if not cb_ok then
                log.warn("tool_loop: on_turn callback error: " .. tostring(cb_res))
            elseif cb_res == false then
                return {
                    ok = true,
                    content = last_content,
                    turns = turn,
                    tool_calls = all_tool_calls,
                    usage = usage,
                    messages = messages,
                    stop_reason = "caller_stopped",
                }
            end
        end

        if #calls == 0 then
            -- `pause_turn` means the server paused its own tool loop; the turn
            -- is unfinished even though it asked us for nothing.
            if answer.stop_reason == "pause_turn" then
                goto continue_turn
            end
            return {
                ok = true,
                content = last_content,
                turns = turn,
                tool_calls = all_tool_calls,
                usage = usage,
                messages = messages,
                stop_reason = last_stop_reason,
            }
        end

        -- Tool calls that arrive with `max_tokens` were cut off mid-emission,
        -- so their arguments cannot be trusted enough to run.
        if answer.stop_reason == "max_tokens" then
            return {
                ok = true,
                content = last_content,
                turns = turn,
                tool_calls = all_tool_calls,
                usage = usage,
                messages = messages,
                stop_reason = "max_tokens",
            }
        end

        local results = {}
        for _, block in ipairs(calls) do
            local call_id = tostring(block.id or "")
            local call_err = record(session, {
                kind = "tool_call",
                turn = model_turn,
                call_id = call_id,
                name = tostring(block.name or ""),
                args = block.input or {},
            })
            if call_err then
                return stopped(call_err, turn)
            end

            local text, is_error = dispatch(by_name, block)

            -- Failures are recorded too: the model is told about them quietly,
            -- and the record is the only place that says one happened.
            local result_err = record(session, {
                kind = "tool_result",
                turn = model_turn,
                call_id = call_id,
                ok = not is_error,
                result = text,
            })
            if result_err then
                return stopped(result_err, turn)
            end

            table.insert(results, {
                type = "tool_result",
                tool_use_id = block.id,
                content = text,
                is_error = is_error or nil,
            })
            table.insert(all_tool_calls, {
                turn = turn,
                name = block.name,
                input = block.input,
                result = text,
                ok = not is_error,
            })
        end
        table.insert(messages, { role = "user", content = results })

        ::continue_turn::
    end

    return {
        ok = false,
        error = "max_turns (" .. max_turns .. ") reached",
        content = last_content,
        turns = max_turns,
        tool_calls = all_tool_calls,
        usage = usage,
        messages = messages,
        stop_reason = last_stop_reason,
    }
end

M._resolve_tools = resolve_tools
M._wire_tools = wire_tools
M._dispatch = dispatch
M._text_of = text_of

return M
