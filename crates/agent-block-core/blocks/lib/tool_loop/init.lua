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
---   Pass `session` and the run records what it does — the prompt, every model
---   response with its usage, every tool call and its result — into that
---   session, each fact written before the loop moves past it, and charges the
---   reported tokens against the session's budget. The loop still keeps no
---   budget of its own: it asks the session whether anything is left before
---   opening another turn, and stops with `stop_reason = "budget_exhausted"`
---   when there is not. Without a session none of this happens and the result
---   is what it always was.
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

--- POST with retries for the failures worth retrying.
---
--- Rate limits, overload and 5xx come back on their own; auth failures,
--- malformed requests and exhausted spend never will, so the classification
--- decides rather than the status class.
local function post_with_retry(url, request_opts, max_retries)
    local attempt = 0
    while true do
        local resp = http.request(url, request_opts)
        if resp.status == 200 or attempt >= max_retries then
            return resp
        end
        local classified = proto.classify_error(resp.status, resp.body, resp.headers)
        if not classified.retryable then
            return resp
        end
        attempt = attempt + 1
        local delay = proto.retry_delay(attempt, classified, attempt)
        log.warn(
            "tool_loop: "
                .. classified.kind
                .. " (HTTP "
                .. tostring(resp.status)
                .. "); retry "
                .. attempt
                .. "/"
                .. max_retries
        )
        std.task.sleep(delay * 1000)
    end
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

--- Charge `amount` tokens against the session's budget.
---
--- @param session table|nil
--- @param amount number
--- @return string|nil  error message, or nil when the charge landed
local function charge(session, amount)
    if not session then
        return nil
    end
    local ok, err = pcall(function()
        session:spend(amount)
    end)
    if ok then
        return nil
    end
    return "session spend failed: " .. tostring(err)
end

--- The content blocks to record for a model response.
---
--- A recorded response carries a non-empty array of blocks. A response with no
--- blocks at all is recorded as the one empty text block it amounts to — the
--- same thing `text_of` derives from it — because an empty Lua table reaches
--- the kernel as an empty mapping rather than an empty array, and losing the
--- response (with its usage) over that would be worse than the placeholder.
local function response_blocks(content)
    if type(content) ~= "table" or #content == 0 then
        return { { type = "text", text = "" } }
    end
    return content
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
---   session  (optional) kernel session (`knl.session`). When given, the run
---            records `msg_user` / `model_response` / `tool_call` /
---            `tool_result` into it — each before the loop advances past the
---            fact — and spends the tokens each response reports against its
---            budget. A run that exhausts the budget stops before the next
---            turn with `ok = true, stop_reason = "budget_exhausted"`; a
---            session that refuses a write ends the run with `ok = false`.
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
---   llm      (optional) { provider, model, base_url, api_key, api_key_env,
---                         max_tokens, temperature, thinking, tool_choice,
---                         dialect, timeout, ... } — forwarded to llm_proto
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
    local adapter, aerr = proto.adapter(llm.provider)
    if not adapter then
        return { ok = false, error = aerr, turns = 0, tool_calls = {}, messages = {} }
    end

    local max_turns = tonumber(opts.max_turns) or DEFAULT_MAX_TURNS
    local max_retries = tonumber(opts.max_retries) or DEFAULT_MAX_RETRIES
    local state = opts.state
    local session = opts.session

    local messages = {}
    for _, m in ipairs(opts.messages or {}) do
        table.insert(messages, m)
    end

    local all_tool_calls = {}
    local usage = { input_tokens = 0, output_tokens = 0, thinking_tokens = 0 }
    local last_content = ""
    local last_stop_reason = nil
    local last_tool_calls = {}

    --- The result for a session that refused a write. Named because the run
    --- can hit it at five points and they all report the same thing.
    local function session_failed(reason, turns_done)
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
        return session_failed(prompt_err, 0)
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

        -- `llm` is forwarded whole rather than through a whitelist: the
        -- adapters already drop what their provider does not accept, and a
        -- whitelist here would silently strip every knob added upstream.
        local build_args = {}
        for k, v in pairs(llm) do
            build_args[k] = v
        end
        build_args.messages = messages
        build_args.system = opts.system
        build_args.tools = wire_tools(specs)
        build_args.max_tokens = llm.max_tokens or 4096

        local req, build_err = adapter.build(build_args)
        if not req then
            return {
                ok = false,
                error = build_err,
                turns = turn - 1,
                tool_calls = all_tool_calls,
                usage = usage,
                messages = messages,
            }
        end

        local body_json = std.json.encode(req.body)
        if opts.on_request then
            pcall(opts.on_request, {
                turn = turn,
                url = req.url,
                headers = req.headers,
                body = req.body,
                body_json = body_json,
            })
        end

        local started = std.time.now()
        local resp = post_with_retry(req.url, {
            method = "POST",
            headers = req.headers,
            body = body_json,
            timeout = llm.timeout or 120,
            dump = llm.dump,
        }, max_retries)
        if opts.on_response then
            pcall(opts.on_response, {
                turn = turn,
                status = resp.status,
                headers = resp.headers,
                body = resp.body,
                latency_ms = math.floor((std.time.now() - started) * 1000),
            })
        end

        if resp.status ~= 200 then
            local classified = proto.classify_error(resp.status, resp.body, resp.headers)
            return {
                ok = false,
                error = "API error " .. tostring(resp.status) .. " (" .. classified.kind .. ")",
                turns = turn - 1,
                tool_calls = all_tool_calls,
                usage = usage,
                messages = messages,
            }
        end

        local ok_decode, raw = pcall(std.json.decode, resp.body)
        if not ok_decode then
            return {
                ok = false,
                error = "response JSON decode failed",
                turns = turn - 1,
                tool_calls = all_tool_calls,
                usage = usage,
                messages = messages,
            }
        end

        local decoded, perr = adapter.parse(raw)
        if not decoded then
            return {
                ok = false,
                error = perr,
                turns = turn - 1,
                tool_calls = all_tool_calls,
                usage = usage,
                messages = messages,
            }
        end

        local u = decoded.usage or {}

        -- Recorded before the response becomes part of the conversation the
        -- next turn is built from, and charged from what was just recorded
        -- rather than from a total kept somewhere else.
        local response_err = record(session, {
            kind = "model_response",
            turn = turn,
            content = response_blocks(decoded.content),
            usage = u,
        })
        if response_err then
            return session_failed(response_err, turn - 1)
        end
        local spent = (u.input_tokens or 0) + (u.output_tokens or 0) + (u.thinking_tokens or 0)
        local spend_err = charge(session, spent)
        if spend_err then
            return session_failed(spend_err, turn - 1)
        end

        -- Appended verbatim: Anthropic requires thinking blocks to come back
        -- unmodified during tool use, so the content is never filtered here.
        table.insert(messages, { role = "assistant", content = decoded.content })

        usage.input_tokens = usage.input_tokens + (u.input_tokens or 0)
        usage.output_tokens = usage.output_tokens + (u.output_tokens or 0)
        usage.thinking_tokens = usage.thinking_tokens + (u.thinking_tokens or 0)

        last_content = text_of(decoded.content)
        last_stop_reason = decoded.stop_reason

        local calls = {}
        for _, block in ipairs(decoded.content or {}) do
            if block.type == "tool_use" then
                table.insert(calls, block)
            end
        end
        last_tool_calls = calls

        -- A refusal is not an empty answer; report it rather than looping.
        if decoded.stop_reason == "refusal" then
            return {
                ok = false,
                error = "model refused to respond",
                content = last_content,
                turns = turn,
                tool_calls = all_tool_calls,
                usage = usage,
                messages = messages,
                stop_reason = decoded.stop_reason,
                stop_details = decoded.stop_details,
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
                usage = decoded.usage,
                decoded = decoded,
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
            if decoded.stop_reason == "pause_turn" then
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
        if decoded.stop_reason == "max_tokens" then
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
                turn = turn,
                call_id = call_id,
                name = tostring(block.name or ""),
                args = block.input or {},
            })
            if call_err then
                return session_failed(call_err, turn)
            end

            local text, is_error = dispatch(by_name, block)

            -- Failures are recorded too: the model is told about them quietly,
            -- and the record is the only place that says one happened.
            local result_err = record(session, {
                kind = "tool_result",
                turn = turn,
                call_id = call_id,
                ok = not is_error,
                result = text,
            })
            if result_err then
                return session_failed(result_err, turn)
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
M._response_blocks = response_blocks

return M
