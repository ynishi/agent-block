-- test_provider_switch.lua — Anthropic (Haiku) ↔ OpenAI-compat (Qwen vLLM, etc.) switch demo
--
-- Same tool.register + agent.run() block runs against either provider; only the
-- per-call opts (provider / base_url / api_key / model) differ.
--
-- Pre-req:
--   AGENT_PROVIDER=anthropic (default)
--     ANTHROPIC_API_KEY=<your-key>
--   AGENT_PROVIDER=openai
--     QWEN_BASE_URL=https://<your-host>/v1   (vLLM / llama.cpp / OpenRouter / RunPod)
--     OPENAI_API_KEY=dummy                    (vLLM ignores; non-empty required)
--
-- Run:
--   AGENT_PROVIDER=anthropic agent-block -s examples/test_provider_switch.lua
--   AGENT_PROVIDER=openai    agent-block -s examples/test_provider_switch.lua

local agent = require("agent")

tool.register("get_time", {
    description = "Get the current local date and time as YYYY-MM-DD HH:MM:SS string",
    input_schema = { type = "object", properties = {} },
}, function(_input)
    return os.date("%Y-%m-%d %H:%M:%S")
end)

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

local provider = std.env.get("AGENT_PROVIDER") or "anthropic"

local opts = {
    max_tokens = 512,
    timeout = 120,
    max_iterations = 5,
    system = "You are a helpful assistant. Use available tools to answer questions. Be concise.",
    prompt = "What is 17 + 25, and what time is it now? Use the tools, do not guess.",
    on_turn = function(info)
        log.info(
            string.format(
                "Turn %d: %d tool calls, tokens %d in / %d out",
                info.turn_number,
                #info.tool_calls,
                info.usage and info.usage.input_tokens or 0,
                info.usage and info.usage.output_tokens or 0
            )
        )
    end,
}

if provider == "openai" then
    local base_url = std.env.get("QWEN_BASE_URL")
    if not base_url or base_url == "" then
        log.error("QWEN_BASE_URL not set. Example: export QWEN_BASE_URL=https://<your-host>/v1")
        os.exit(2)
    end
    opts.provider = "openai"
    opts.base_url = base_url
    opts.api_key = "dummy"
    opts.model = "qwen"
elseif provider == "anthropic" then
    opts.model = "claude-haiku-4-5-20251001"
else
    log.error("AGENT_PROVIDER must be 'anthropic' or 'openai', got: " .. tostring(provider))
    os.exit(2)
end

log.info(string.format("Provider: %s | Model: %s", provider, opts.model))

local result = agent.run(opts)

if not result.ok then
    log.error("FAIL: agent.run returned error: " .. tostring(result.error))
    os.exit(1)
end

log.info("=== RESULT ===")
log.info("content: " .. tostring(result.content))
log.info(
    string.format(
        "usage: %d in + %d out = %d total (turns=%d)",
        result.usage.input_tokens or 0,
        result.usage.output_tokens or 0,
        result.usage.total_tokens or 0,
        result.num_turns or 0
    )
)

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
    log.info("PASS: provider=" .. provider .. " round-trip OK")
    os.exit(0)
else
    os.exit(2)
end
