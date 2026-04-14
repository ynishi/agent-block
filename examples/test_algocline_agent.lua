-- test_algocline_agent.lua — agent.run() × algocline MCP 統合テスト
--
-- Step 3b: ReAct ループが alc_run → needs_response → alc_continue を
--          自律的にハンドルできるか検証
--
-- Run with:
--   agent-block -s examples/test_algocline_agent.lua

local agent = require("agent")

local result = agent.run({
    prompt = [[
Use the algocline MCP tools to run a simple Lua script that calls alc.llm().

Steps:
1. Call alc_run with this code:
   local resp = alc.llm("What is the capital of Japan? Answer in one word.")
   return { answer = resp }

2. The result will have status "needs_response" with a prompt field.
   You ARE the LLM — just answer the prompt directly by calling alc_continue
   with the session_id and your answer as the response.

3. Report the final result.
]],
    system = [[You are an agent that orchestrates algocline executions.
When alc_run returns status "needs_response", call alc_continue with:
- session_id: from the alc_run response
- query_id: from the alc_run response (if present)
- response: YOUR answer to the prompt

You are the LLM that answers the prompt. Do not call any external API.
Be concise in your answers.]],
    model = "claude-haiku-4-5-20251001",
    max_tokens = 1024,
    max_iterations = 10,
    mcp_servers = {
        { name = "algocline", command = "alc", args = {} },
    },
    on_turn = function(info)
        log.info(string.format(
            "Turn %d: %d tool calls, tokens: %d in / %d out",
            info.turn_number,
            #info.tool_calls,
            info.usage and info.usage.input_tokens or 0,
            info.usage and info.usage.output_tokens or 0
        ))
    end,
})

if result.ok then
    log.info("Agent completed successfully")
    log.info("Response: " .. result.content)
    log.info(string.format(
        "Total: %d input + %d output = %d tokens in %d turns",
        result.usage.input_tokens,
        result.usage.output_tokens,
        result.usage.total_tokens,
        result.num_turns
    ))
else
    log.error("Agent failed: " .. (result.error or "unknown error"))
end
