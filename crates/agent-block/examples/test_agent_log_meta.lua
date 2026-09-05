-- test_agent_log_meta.lua — correlation ids, from the environment to both
-- places a run leaves a trace.
--
-- There is no option to pass them. `agent.run` reads AGENT_BLOCK_TRACE_ID /
-- _RUN_ID / _AGENT_ID / _AGENT_NAME off the environment, and they land in two
-- places from there:
--
--   * every model call is an `ab.obs` http_request / http_response line
--     carrying the four as `trace_id=` / `run_id=` / `agent_id=` /
--     `agent_name=` (RUST_LOG=info makes them visible);
--   * the run's seed event carries the same four as `meta` labels beside
--     `label = "prompt"`, so the session log is selected by the id the log
--     lines are grepped by:
--
--       session:query(
--           "SELECT * FROM events WHERE json_extract(meta, '$.run_id') = ?",
--           { std.env.get("AGENT_BLOCK_RUN_ID") })
--
-- Only the variables that are set become labels. `agent_id` is the one that
-- can differ: with AGENT_BLOCK_AGENT_ID unset the obs lines still carry a
-- per-process id the Rust side makes up, and the seed event carries no
-- `agent_id` at all — set it and both sides say the same thing.
--
-- Requires ANTHROPIC_API_KEY.
-- Suggested run:
--   RUST_LOG=info \
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
