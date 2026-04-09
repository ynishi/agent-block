-- fcloop.lua — Function Call Loop module (http.request based)
-- Usage: local fcloop = require("fcloop")
-- fcloop.run(messages, opts) -> final_messages

local M = {}

--- Call Anthropic Messages API via http.request.
--- @param messages table  Messages array
--- @param opts table      Options: system, model, max_tokens, tools
--- @return table          Parsed response JSON
local function llm_call(messages, opts)
    local api_key = std.env.get("ANTHROPIC_API_KEY")
    if not api_key then
        error("ANTHROPIC_API_KEY not set")
    end

    local model = opts.model or std.env.get_or("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")

    local body = {
        model = model,
        max_tokens = opts.max_tokens or 4096,
        messages = messages,
    }
    if opts.system then
        body.system = opts.system
    end
    if opts.tools and #opts.tools > 0 then
        body.tools = opts.tools
    end

    local resp = http.request("https://api.anthropic.com/v1/messages", {
        method = "POST",
        headers = {
            ["x-api-key"] = api_key,
            ["anthropic-version"] = "2023-06-01",
            ["content-type"] = "application/json",
        },
        body = std.json.encode(body),
        timeout = opts.timeout or 120,
    })

    if resp.status ~= 200 then
        error("API error " .. resp.status .. ": " .. resp.body)
    end

    return std.json.decode(resp.body)
end

--- Run a tool-use loop until the model stops calling tools.
--- @param messages table  Initial messages array
--- @param opts table      Options: system, model, max_tokens, max_iterations, timeout
--- @return table          Final messages array after all tool calls resolved
function M.run(messages, opts)
    opts = opts or {}
    local max_iter = opts.max_iterations or 20
    local iter = 0

    while true do
        -- Inject current tool schemas
        local call_opts = {}
        for k, v in pairs(opts) do
            call_opts[k] = v
        end
        call_opts.tools = tool.schema()

        local result = llm_call(messages, call_opts)

        -- Append assistant message
        table.insert(messages, {
            role = "assistant",
            content = result.content,
        })

        -- Check if any tool_use blocks exist
        local tool_calls = {}
        for _, block in ipairs(result.content) do
            if block.type == "tool_use" then
                table.insert(tool_calls, block)
            end
        end

        -- No tool calls → done
        if #tool_calls == 0 then
            break
        end

        iter = iter + 1
        if iter >= max_iter then
            log.warn("fcloop: max iterations (" .. max_iter .. ") reached, stopping")
            break
        end

        -- Execute tool calls and collect results
        local tool_results = {}
        for _, tc in ipairs(tool_calls) do
            local ok, res = pcall(tool.call, tc.name, tc.input)
            local content
            if ok then
                if type(res) == "table" then
                    content = std.json.encode(res)
                else
                    content = tostring(res)
                end
            else
                content = "error: " .. tostring(res)
            end
            table.insert(tool_results, {
                type = "tool_result",
                tool_use_id = tc.id,
                content = content,
            })
        end

        -- Append tool results as user message
        table.insert(messages, {
            role = "user",
            content = tool_results,
        })
    end

    return messages
end

return M
