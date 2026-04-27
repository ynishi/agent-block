-- test_qwen_openai.lua — OpenAI provider × Qwen3.6-27B (vLLM) e2e smoke test
--
-- Pre-req:
--   1. Qwen vLLM endpoint up at QWEN_BASE_URL (your own host: local / remote / proxied).
--      Example startup:
--        vllm serve /path/to/qwen-awq --enable-auto-tool-choice
--             --tool-call-parser qwen3_xml --reasoning-parser qwen3
--             --served-model-name qwen --enforce-eager
--   2. Set env vars (vLLM ignores OPENAI_API_KEY but the header builder requires non-empty):
--        export QWEN_BASE_URL=https://<your-host>/v1
--        export OPENAI_API_KEY=dummy
--
-- Run:
--   agent-block -s examples/test_qwen_openai.lua
--
-- What this exercises (issue 1777297062-44306 受け入れ基準):
--   - opts.provider = "openai" 経路全体
--   - opts.base_url override (RunPod proxy)
--   - tool.register された Lua tool が ReAct loop で 1 回以上 dispatch される
--   - 最終 content が文字列で返る
--   - usage / num_turns が記録される

local agent = require("agent")

-- Local tool: agent should call this rather than guessing.
tool.register("get_time", {
    description = "Get the current local date and time as YYYY-MM-DD HH:MM:SS string",
    input_schema = {
        type = "object",
        properties = {},
    },
}, function(_input)
    return os.date("%Y-%m-%d %H:%M:%S")
end)

-- Local tool: simple add to give the agent a 2nd tool to disambiguate selection.
tool.register("add", {
    description = "Add two integers a + b and return the sum",
    input_schema = {
        type = "object",
        properties = {
            a = { type = "integer" },
            b = { type = "integer" },
        },
        required = { "a", "b" },
    },
}, function(input)
    local a = tonumber(input.a) or 0
    local b = tonumber(input.b) or 0
    return tostring(a + b)
end)

local QWEN_BASE_URL = std.env.get("QWEN_BASE_URL")
if not QWEN_BASE_URL or QWEN_BASE_URL == "" then
    log.error("QWEN_BASE_URL not set. Example: export QWEN_BASE_URL=https://<your-host>/v1")
    os.exit(2)
end

log.info("Connecting to Qwen vLLM endpoint: " .. QWEN_BASE_URL)

local result = agent.run({
    provider   = "openai",
    base_url   = QWEN_BASE_URL,
    api_key    = "dummy", -- vLLM ignores; placeholder satisfies header builder
    model      = "qwen",  -- --served-model-name qwen
    max_tokens = 512,
    timeout    = 120,
    max_iterations = 5,

    system = "You are a helpful assistant. Use available tools to answer questions. Be concise.",
    prompt = "What is 17 + 25, and what time is it now? Use the tools, do not guess.",

    on_turn = function(info)
        log.info(string.format(
            "Turn %d: %d tool calls, tokens %d in / %d out",
            info.turn_number,
            #info.tool_calls,
            info.usage and info.usage.input_tokens or 0,
            info.usage and info.usage.output_tokens or 0
        ))
        for _, tc in ipairs(info.tool_calls) do
            log.info("  -> tool=" .. tostring(tc.name) .. " input=" .. std.json.encode(tc.input or {}))
        end
    end,
})

if not result.ok then
    log.error("FAIL: agent.run returned error: " .. tostring(result.error))
    os.exit(1)
end

log.info("=== RESULT ===")
log.info("content: " .. tostring(result.content))
log.info(string.format(
    "usage: %d in + %d out = %d total (turns=%d)",
    result.usage.input_tokens or 0,
    result.usage.output_tokens or 0,
    result.usage.total_tokens or 0,
    result.num_turns or 0
))

-- Acceptance gates
local pass = true
if not result.content or result.content == "" then
    log.error("GATE FAIL: empty content")
    pass = false
end
if (result.num_turns or 0) < 2 then
    log.error("GATE FAIL: expected >=2 turns (tool dispatch + final answer), got " .. tostring(result.num_turns))
    pass = false
end

if pass then
    log.info("PASS: openai provider + Qwen vLLM round-trip OK")
    os.exit(0)
else
    os.exit(2)
end
