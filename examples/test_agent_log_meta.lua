-- test_agent_log_meta.lua — structured LLM dump + external metadata example.
--
-- Requires ANTHROPIC_API_KEY.
-- Suggested run:
--   AGENT_BLOCK_LLM_DUMP=meta \
--   AGENT_BLOCK_TRACE_ID=trace-xyz \
--   AGENT_BLOCK_AGENT_ID=agent-42 \
--   AGENT_BLOCK_AGENT_NAME=planner \
--   AGENT_BLOCK_RUN_ID=run-001 \
--   agent-block -s examples/test_agent_log_meta.lua

local agent = require("agent")

tool.register("get_time", {
    description = "Get the current date and time as a string",
    input_schema = {
        type = "object",
        properties = {},
    },
}, function(_input)
    return os.date("%Y-%m-%d %H:%M:%S")
end)

local result = agent.run({
    prompt = "Use get_time and tell me the current time in one sentence.",
    system = "You are concise and use tools when available.",
    max_tokens = 256,
    max_iterations = 4,
    log_meta = {
        -- Override explicitly, but keep env defaults easy to inject.
        trace_id = std.env.get_or("AGENT_BLOCK_TRACE_ID", "demo-trace-id"),
        agent_id = std.env.get_or("AGENT_BLOCK_AGENT_ID", "demo-agent-id"),
        agent_name = std.env.get_or("AGENT_BLOCK_AGENT_NAME", "demo-agent-name"),
        run_id = std.env.get_or("AGENT_BLOCK_RUN_ID", "demo-run-id"),
    },
    on_turn = function(info)
        log.info(
            string.format(
                "on_turn turn=%d tool_calls=%d in=%d out=%d",
                info.turn_number,
                #info.tool_calls,
                info.usage and info.usage.input_tokens or 0,
                info.usage and info.usage.output_tokens or 0
            )
        )
    end,
})

if not result.ok then
    log.error("agent failed: " .. tostring(result.error))
    return
end

log.info("agent ok: " .. tostring(result.content))
