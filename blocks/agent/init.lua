-- blocks/agent/init.lua — Generic Agent module (StdPkg)
--
-- Usage:
--   local agent = require("agent")
--   local result = agent.run({
--       prompt = "...",
--       system = "...",
--       model = "claude-sonnet-4-20250514",
--       max_tokens = 4096,
--       timeout = 120,
--       max_iterations = 20,
--       max_tokens_budget = nil,
--       mcp_servers = { { name = "outline", command = "outline-mcp", args = {} } },
--       on_turn = function(turn_info) end,
--       extra_tools = {},
--       -- Anthropic server-side context editing (default ON).
--       -- Set to false to opt out entirely (no beta header, no body field).
--       context_management = true,
--       -- Optional override for the default edits table (clear_tool_uses_20250919).
--       context_management_config = { edits = { ... } },
--   })
--
-- result: { ok, content, usage, num_turns, error, messages }
--
-- Notes:
--   - All MCP/HTTP bridge calls are async (coroutine yield). agent.run() must be
--     called inside isle.coroutine_eval() or equivalent async context.
--   - tool.call() is sync. tool.schema() is sync.
--   - NEVER throws. All errors returned as { ok=false, error=... }.
--   - on_turn payload keys: turn_number, content, tool_calls, usage,
--     context_management (additive; absent when the server reports no edits
--     this turn, i.e. response.context_management is nil).

local M = {}

-- ============================================================
-- Default context management config (Anthropic server-side
-- rolling history via clear_tool_uses_20250919).
-- Trigger at 80K input_tokens, keep last 3 tool_uses,
-- clear at least 10K input_tokens worth.
-- Opt-out via opts.context_management = false.
-- Override via opts.context_management_config = { ... }.
-- ============================================================

local DEFAULT_CONTEXT_MANAGEMENT = {
    edits = {
        {
            type = "clear_tool_uses_20250919",
            trigger = { type = "input_tokens", value = 80000 },
            keep   = { type = "tool_uses",    value = 3 },
            clear_at_least = { type = "input_tokens", value = 10000 },
        },
    },
}

-- ============================================================
-- Internal: LLM API call (Anthropic Messages API)
-- ============================================================

--- Call Anthropic Messages API via http.request.
--- @param messages table  Messages array
--- @param opts table      Options: system, model, max_tokens, tools, timeout,
---                        context_management (table|nil — table enables the
---                        context-management beta header and body field; nil
---                        means opt-out, no header and no body field).
--- @return table|nil      Parsed response JSON on success, nil on error
--- @return string|nil     Error string on failure
local function llm_call(messages, opts)
    local api_key = std.env.get("ANTHROPIC_API_KEY")
    if not api_key then
        return nil, "ANTHROPIC_API_KEY not set"
    end

    local model = opts.model or std.env.get_or("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")

    local body = {
        model = model,
        max_tokens = opts.max_tokens or 4096,
        messages = messages,
    }
    if opts.system and opts.system ~= "" then
        body.system = opts.system
    end
    if opts.tools and #opts.tools > 0 then
        body.tools = opts.tools
    end

    local headers = {
        ["x-api-key"] = api_key,
        ["anthropic-version"] = "2023-06-01",
        ["content-type"] = "application/json",
    }

    -- Anthropic context-management (beta): add header + body only when enabled.
    -- call_opts normalization in M.run() makes opts.context_management either
    -- nil (opt-out) or a table (enabled: default or user-provided override).
    if opts.context_management ~= nil then
        headers["anthropic-beta"] = "context-management-2025-06-27"
        body.context_management = opts.context_management
    end

    local resp = http.request("https://api.anthropic.com/v1/messages", {
        method = "POST",
        headers = headers,
        body = std.json.encode(body),
        timeout = opts.timeout or 120,
    })

    if resp.status ~= 200 then
        return nil, "API error " .. resp.status .. ": " .. resp.body
    end

    local decoded = std.json.decode(resp.body)
    return decoded, nil
end

-- ============================================================
-- Internal: Budget tracking
-- ============================================================

--- Create a new budget tracker.
--- @param max_tokens_budget number|nil  Total token limit (nil = unlimited)
--- @return table  Tracker with :add(usage), :exceeded(), :summary() methods
local function new_budget_tracker(max_tokens_budget)
    local tracker = {
        input_tokens = 0,
        output_tokens = 0,
        total_tokens = 0,
        limit = max_tokens_budget,
    }

    function tracker:add(usage)
        if usage then
            self.input_tokens = self.input_tokens + (usage.input_tokens or 0)
            self.output_tokens = self.output_tokens + (usage.output_tokens or 0)
            self.total_tokens = self.input_tokens + self.output_tokens
        end
    end

    function tracker:exceeded()
        if not self.limit then return false end
        return self.total_tokens >= self.limit
    end

    function tracker:summary()
        return {
            input_tokens = self.input_tokens,
            output_tokens = self.output_tokens,
            total_tokens = self.total_tokens,
        }
    end

    return tracker
end

-- ============================================================
-- Internal: MCP server integration
-- ============================================================

--- Connect to MCP servers and collect tool definitions.
--- Returns mcp_tool_map and list of connected server names.
--- On failure, returns nil + error string (with already-connected servers in third return).
---
--- @param servers table  Array of { name, command, args? }
--- @return table|nil     mcp_tool_map { ["server__tool"] = { server, tool, def } }
--- @return string|nil    Error string on failure
--- @return table         List of connected server names (for cleanup on failure)
local function connect_mcp_servers(servers)
    local mcp_tool_map = {}
    local connected = {}

    for _, srv in ipairs(servers) do
        local name = srv.name
        local command = srv.command
        local args = srv.args or {}

        -- Connect to MCP server (async)
        local ok, err = pcall(mcp.connect, name, command, args)
        if not ok then
            return nil, "mcp connect failed for '" .. name .. "': " .. tostring(err), connected
        end
        table.insert(connected, name)

        -- List tools (async)
        local list_result = mcp.list_tools(name)
        if not list_result.ok then
            return nil, "mcp list_tools failed for '" .. name .. "': " .. tostring(list_result.error), connected
        end

        local tools = list_result.tools or {}
        for _, t in ipairs(tools) do
            local ns_name = name .. "__" .. t.name
            -- Convert inputSchema (camelCase) -> input_schema (snake_case) for Anthropic API
            local input_schema = t.inputSchema or t.input_schema or { type = "object", properties = {} }
            mcp_tool_map[ns_name] = {
                server = name,
                tool = t.name,
                def = {
                    name = ns_name,
                    description = t.description or "",
                    input_schema = input_schema,
                },
            }
        end
    end

    return mcp_tool_map, nil, connected
end

--- Gracefully disconnect from MCP servers.
--- Logs errors but does not throw.
--- @param server_names table  Array of server name strings
local function disconnect_mcp_servers(server_names)
    for _, name in ipairs(server_names) do
        local ok, err = pcall(mcp.disconnect, name)
        if not ok then
            log.warn("agent: mcp disconnect error for '" .. name .. "': " .. tostring(err))
        end
    end
end

-- ============================================================
-- Internal: Build unified tools array
-- ============================================================

--- Build the unified tools array for the Anthropic API.
--- Merges tool.schema() (registered Lua tools) + MCP tools + extra_tools.
--- @param mcp_tool_map table  MCP namespace map (may be empty)
--- @param extra_tools table   Additional Anthropic tool definitions (may be nil/empty)
--- @return table              Unified tools array in Anthropic format
local function build_tools(mcp_tool_map, extra_tools)
    local tools = {}

    -- Add registered Lua tools from tool.schema()
    local lua_tools = tool.schema()
    for _, t in ipairs(lua_tools) do
        table.insert(tools, t)
    end

    -- Add MCP tools (already in Anthropic format from connect_mcp_servers)
    for _, entry in pairs(mcp_tool_map) do
        table.insert(tools, entry.def)
    end

    -- Add extra_tools if provided
    if extra_tools then
        for _, t in ipairs(extra_tools) do
            table.insert(tools, t)
        end
    end

    return tools
end

-- ============================================================
-- Internal: Tool dispatch (unified)
-- ============================================================

--- Dispatch a tool call to either MCP or the local Lua registry.
--- Errors are returned as (content, is_error=true) instead of throwing.
--- @param name string         Tool name (possibly namespaced as "server__tool")
--- @param input table         Tool input from LLM
--- @param mcp_tool_map table  MCP namespace map
--- @return string             Result content string
--- @return boolean            is_error flag
local function dispatch_tool(name, input, mcp_tool_map)
    -- Check if this is an MCP-namespaced tool
    if mcp_tool_map[name] then
        local entry = mcp_tool_map[name]
        local call_result = mcp.call(entry.server, entry.tool, input)
        -- ok=false covers transport / protocol / timeout failures only.
        if not call_result.ok then
            return tostring(call_result.error or "mcp.call failed"), true
        end

        -- Server-reported tool-execution error (MCP `isError`). Forward the
        -- content as-is so the LLM can self-correct in the ReAct loop.
        local is_error = call_result.is_error == true
        if is_error then
            log.warn(string.format("mcp tool '%s.%s' returned isError=true", entry.server, entry.tool))
        end

        -- Extract content from MCP result
        local content_blocks = call_result.content or {}
        if #content_blocks == 1 and content_blocks[1].type == "text" then
            return content_blocks[1].text, is_error
        elseif #content_blocks == 0 then
            return "", is_error
        else
            -- Multiple blocks or non-text: encode as JSON
            return std.json.encode(content_blocks), is_error
        end
    end

    -- Fall back to registered Lua tool
    local ok, res = pcall(tool.call, name, input)
    if not ok then
        return "tool error: " .. tostring(res), true
    end
    if type(res) == "table" then
        return std.json.encode(res), false
    end
    return tostring(res), false
end

-- ============================================================
-- Internal: Extract text content from Anthropic response
-- ============================================================

--- Collect all text blocks from the Anthropic content array and concatenate.
--- @param content table  Array of content blocks from Anthropic response
--- @return string        Concatenated text, or empty string
local function extract_text(content)
    local parts = {}
    for _, block in ipairs(content or {}) do
        if block.type == "text" and block.text then
            table.insert(parts, block.text)
        end
    end
    return table.concat(parts, "\n")
end

-- ============================================================
-- Public: agent.run(opts)
-- ============================================================

--- Run a ReAct agent loop.
---
--- @param opts table  {
---   prompt          (required) Initial user prompt string
---   system          (optional) System prompt string
---   model           (optional) LLM model identifier
---   max_tokens      (optional) Per-request token limit (default: 4096)
---   timeout         (optional) HTTP timeout in seconds (default: 120)
---   max_iterations  (optional) Max tool-use loop iterations (default: 20)
---   max_tokens_budget (optional) Total token budget across all iterations (default: nil = unlimited)
---   mcp_servers     (optional) Array of { name, command, args? }
---   on_turn         (optional) Callback function(turn_info). turn_info has
---                   keys: turn_number, content, tool_calls, usage, and
---                   context_management (passed through from the Anthropic
---                   response; absent when no edits fired this turn).
---   extra_tools     (optional) Extra Anthropic tool definitions to include
---   context_management        (optional, default true) When false, opt out of
---                   Anthropic server-side context editing entirely (no beta
---                   header, no body field). Any non-false value (nil, true,
---                   table) keeps it enabled.
---   context_management_config (optional) Full override table passed as
---                   body.context_management. Defaults to DEFAULT_CONTEXT_MANAGEMENT
---                   (clear_tool_uses_20250919 with 80K/keep=3/clear>=10K).
---                   Ignored when context_management == false.
--- }
---
--- @return table  {
---   ok         boolean
---   content    string  (final text response)
---   usage      { input_tokens, output_tokens, total_tokens }
---   num_turns  number
---   error      string  (when ok=false)
---   messages   table   (full conversation history)
--- }
function M.run(opts)
    opts = opts or {}

    -- Validate required fields
    if not opts.prompt or opts.prompt == "" then
        return { ok = false, error = "prompt is required", usage = { input_tokens = 0, output_tokens = 0, total_tokens = 0 }, num_turns = 0, messages = {} }
    end

    -- Budget tracker
    local budget = new_budget_tracker(opts.max_tokens_budget)
    local max_iter = opts.max_iterations or 20

    -- Connect MCP servers if specified
    local mcp_tool_map = {}
    local connected_servers = {}

    if opts.mcp_servers and #opts.mcp_servers > 0 then
        local tool_map, err, partial_connected = connect_mcp_servers(opts.mcp_servers)
        if err then
            -- Disconnect any servers that did connect before the failure
            disconnect_mcp_servers(partial_connected)
            return {
                ok = false,
                error = err,
                usage = budget:summary(),
                num_turns = 0,
                messages = {},
            }
        end
        mcp_tool_map = tool_map
        connected_servers = partial_connected
    end

    -- Build unified tools array
    local tools = build_tools(mcp_tool_map, opts.extra_tools)

    -- Normalize context_management opts once:
    --   opts.context_management == false                   → cm_final = nil (opt-out)
    --   opts.context_management_config = { ... } (or nil)  → cm_final = override or DEFAULT
    -- Strict equality (~= false) is used so nil (unset) is treated as default-on.
    local cm_final
    if opts.context_management == false then
        cm_final = nil
    else
        cm_final = opts.context_management_config or DEFAULT_CONTEXT_MANAGEMENT
    end

    -- Build call options for llm_call
    local call_opts = {
        model = opts.model,
        max_tokens = opts.max_tokens or 4096,
        timeout = opts.timeout or 120,
        system = opts.system,
        tools = tools,
        context_management = cm_final,  -- nil = opt-out, table = enabled
    }

    -- Initialize message history
    local messages = {
        { role = "user", content = opts.prompt },
    }

    -- ReAct loop state
    local num_turns = 0
    local final_content = ""
    local loop_error = nil

    -- pcall wrapper for guaranteed MCP cleanup
    local loop_ok, loop_err = pcall(function()
        local iter = 0

        while true do
            -- Call LLM
            local response, api_err = llm_call(messages, call_opts)
            if not response then
                loop_error = api_err
                return
            end

            -- Append assistant message
            table.insert(messages, {
                role = "assistant",
                content = response.content,
            })

            -- Track usage BEFORE budget check
            budget:add(response.usage)
            num_turns = num_turns + 1

            -- Collect tool calls from response
            local tool_calls = {}
            for _, block in ipairs(response.content or {}) do
                if block.type == "tool_use" then
                    table.insert(tool_calls, block)
                end
            end

            -- Extract current text content
            final_content = extract_text(response.content)

            -- Fire on_turn callback (errors are logged, not propagated)
            if opts.on_turn then
                local cb_ok, cb_err = pcall(opts.on_turn, {
                    turn_number = num_turns,
                    content = response.content,
                    tool_calls = tool_calls,
                    usage = response.usage,
                    -- Pass-through of Anthropic response.context_management.
                    -- When the server didn't apply any edits this turn the
                    -- field is nil and Lua removes the key from the payload,
                    -- preserving the historical 4-key shape for existing callbacks.
                    context_management = response.context_management,
                })
                if not cb_ok then
                    log.warn("agent: on_turn callback error: " .. tostring(cb_err))
                end
            end

            -- No tool calls → done (end_turn or max_tokens)
            if #tool_calls == 0 then
                break
            end

            -- Check stop reason
            local stop_reason = response.stop_reason
            if stop_reason == "end_turn" or stop_reason == "max_tokens" then
                break
            end

            -- Budget checks
            iter = iter + 1
            if iter >= max_iter then
                log.warn("agent: max iterations (" .. max_iter .. ") reached")
                break
            end

            if budget:exceeded() then
                log.warn("agent: token budget exceeded (" .. budget.total_tokens .. "/" .. budget.limit .. ")")
                break
            end

            -- Dispatch tool calls and collect results
            local tool_results = {}
            for _, tc in ipairs(tool_calls) do
                local content_str, is_error = dispatch_tool(tc.name, tc.input, mcp_tool_map)
                table.insert(tool_results, {
                    type = "tool_result",
                    tool_use_id = tc.id,
                    content = content_str,
                    is_error = is_error or nil,
                })
            end

            -- Append tool results as user message
            table.insert(messages, {
                role = "user",
                content = tool_results,
            })
        end
    end)

    -- Always disconnect MCP servers, regardless of loop outcome
    disconnect_mcp_servers(connected_servers)

    -- Propagate unexpected pcall error
    if not loop_ok then
        return {
            ok = false,
            error = tostring(loop_err),
            usage = budget:summary(),
            num_turns = num_turns,
            messages = messages,
        }
    end

    -- Propagate structured API error
    if loop_error then
        return {
            ok = false,
            error = loop_error,
            usage = budget:summary(),
            num_turns = num_turns,
            messages = messages,
        }
    end

    return {
        ok = true,
        content = final_content,
        usage = budget:summary(),
        num_turns = num_turns,
        messages = messages,
    }
end

return M
