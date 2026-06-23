-- test_algocline_e2e.lua — Step 4: ingredient → recipe E2E テスト
--
-- agent.run() が algocline の alc_advice (sc パッケージ) を通して
-- 複数回の alc.llm() pause/continue を自律的にハンドルできるか検証
--
-- sc (self-consistency) は N 個の独立サンプルを取って多数決する。
-- N=3 の最小構成で試す → 6回の alc.llm() 呼び出し (3 sample + 3 extract)
--
-- Run with:
--   agent-block -s examples/test_algocline_e2e.lua

local agent = require("agent")

local result = agent.run({
    prompt = [[
Use algocline to solve a math problem with the sc (self-consistency) package.

Call alc_advice with:
- package: "sc"
- task: "What is 17 * 23? Show your work and give the final answer."
- opts: { n = 3 }

The sc package will make multiple alc.llm() calls. Each call returns
status "needs_response" — you must call alc_continue with:
- session_id: from the response
- response: YOUR answer to the prompt (you are the LLM being queried)

IMPORTANT: When answering sc's prompts, give genuine reasoning and answers.
The prompts will ask you to solve the math problem independently each time.
For answer extraction prompts, extract just the number.

After all rounds complete, report:
1. The consensus answer
2. Number of LLM calls made
3. Vote distribution
]],
    system = [[You are an agent that orchestrates algocline package executions.
When alc_advice or alc_run returns status "needs_response", call alc_continue with:
- session_id: from the response
- query_id: from the response (if present)
- response: your genuine answer to the prompt

You ARE the LLM. Answer prompts thoughtfully and correctly.
For math: show work. For extraction: give just the answer.
Be concise in final reporting.]],
    model = "claude-haiku-4-5-20251001",
    max_tokens = 1024,
    max_iterations = 25,
    mcp_servers = {
        { name = "algocline", command = "alc", args = {} },
    },
    on_turn = function(info)
        log.info(
            string.format(
                "Turn %d: %d tool calls, tokens: %d in / %d out",
                info.turn_number,
                #info.tool_calls,
                info.usage and info.usage.input_tokens or 0,
                info.usage and info.usage.output_tokens or 0
            )
        )
    end,
})

if result.ok then
    log.info("=== Agent completed ===")
    log.info("Response: " .. result.content)
    log.info(
        string.format(
            "Total: %d input + %d output = %d tokens in %d turns",
            result.usage.input_tokens,
            result.usage.output_tokens,
            result.usage.total_tokens,
            result.num_turns
        )
    )
else
    log.error("Agent failed: " .. (result.error or "unknown"))
end
