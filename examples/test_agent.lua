-- test_agent.lua — agent.run() basic usage example
--
-- Requires ANTHROPIC_API_KEY to be set in the environment.
-- Run with:
--   agent-block -s examples/test_agent.lua
--
-- This is a sample script; it is NOT part of `cargo test`.

local agent = require("agent")

-- Register a local tool the agent can call
tool.register("get_time", {
    description = "Get the current date and time as a string",
    input_schema = {
        type = "object",
        properties = {},
    },
}, function(_input)
    return os.date("%Y-%m-%d %H:%M:%S")
end)

-- Run the agent
local result = agent.run({
    prompt = "What time is it? Use the get_time tool to find out, then tell me.",
    system = "You are a helpful assistant. Use available tools to answer questions. Be concise.",
    max_tokens = 512,
    max_iterations = 5,
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
        "Total usage: %d input + %d output = %d tokens in %d turns",
        result.usage.input_tokens,
        result.usage.output_tokens,
        result.usage.total_tokens,
        result.num_turns
    ))
else
    log.error("Agent failed: " .. (result.error or "unknown error"))
end
