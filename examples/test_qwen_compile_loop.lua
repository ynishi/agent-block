-- test_qwen_compile_loop.lua — compile_loop e2e (Qwen vLLM)
--
-- Demonstrates blocks/compile_loop: structural Edit→Run→Feedback loop.
-- Uses the SAME deep_merge spec that broke Qwen in 3 manual iters,
-- now driven autonomously through the loop.
--
-- Run:
--   QWEN_BASE_URL=https://<pod>-8188.proxy.runpod.net/v1 \
--   OPENAI_API_KEY=dummy \
--   agent-block -s examples/test_qwen_compile_loop.lua

local compile_loop = require("compile_loop")
local agent        = require("agent")

local QWEN_BASE_URL = std.env.get("QWEN_BASE_URL")
if not QWEN_BASE_URL or QWEN_BASE_URL == "" then
    log.error("QWEN_BASE_URL not set")
    os.exit(2)
end

local TARGET = "/tmp/qwen_react_work.lua"

-- Runner: invoke the local lua interpreter on the file, capture all output.
local function lua_runner(file_path)
    local p = io.popen("lua " .. file_path .. ' 2>&1; echo "__EXIT__=$?"', "r")
    if not p then return { ok=false, stdout="", stderr="popen failed", exit_code=-1 } end
    local out = p:read("*a") or ""
    p:close()
    -- Extract __EXIT__ marker
    local exit_str = out:match("__EXIT__=(%d+)%s*$") or "1"
    local exit_code = tonumber(exit_str) or 1
    out = out:gsub("__EXIT__=%d+%s*$", "")
    -- PASS condition: exit 0 AND stdout contains "ALL_PASS"
    local pass = exit_code == 0 and out:find("ALL_PASS", 1, true) ~= nil
    return { ok = pass, stdout = out, stderr = "", exit_code = exit_code }
end

local SPEC = [[Write a single Lua 5.3+ file (no external libs) that:

1. Defines `local M = {}` and `M.deep_merge(base, override)` returning a NEW table where:
   - override values win on key conflict
   - if BOTH sides at the same key are non-array tables, merge recursively
   - arrays (consecutive integer keys starting at 1) are replaced wholesale
   - override = nil leaves base unchanged (do NOT crash)
   - base table is NEVER mutated
2. At the bottom of the file, run inline `assert(...)` calls covering:
   (a) flat merge
   (b) nested table merge
   (c) array replacement (length and element values)
   (d) override = nil case (verify base values present, no extra keys)
   (e) base unchanged after merge
3. After all assertions pass, print exactly `ALL_PASS` to stdout.
4. End with `return M`.

Output ONLY the file contents in a single ```lua ... ``` block.]]

log.info("Connecting to: " .. QWEN_BASE_URL)
log.info("Target file:   " .. TARGET)

-- K-96: all LLM tuning fields are explicitly listed in the llm table.
local td = compile_loop.make({
    runner   = lua_runner,
    llm      = {
        provider         = "openai",
        base_url         = QWEN_BASE_URL,
        api_key          = "dummy",
        model            = "qwen",
        disable_thinking = true,
        temperature      = 0.2,
        max_tokens       = 2000,
    },
    max_iters = 5,
    lang      = "lua",
    register  = false,
})

-- Parent also uses Qwen (minimum env: only QWEN_BASE_URL required).
local result = agent.run({
    provider       = "openai",
    base_url       = QWEN_BASE_URL,
    api_key        = "dummy",
    model          = "qwen",
    max_iterations = 3,
    extra_tools    = { td },
    prompt         = string.format(
        "Use the compile_loop tool to solve the following coding task.\nTarget file: %s\nSpec:\n%s",
        TARGET, SPEC
    ),
})

log.info("=== RESULT ===")
log.info("ok:        " .. tostring(result.ok))
log.info("num_turns: " .. tostring(result.num_turns))
if not result.ok then
    log.error("parent agent failed: " .. tostring(result.error or "unknown"))
    os.exit(2)
end

-- ── Extract tool_result from messages ─────────────────────────────────────────
local captured = nil
for _, msg in ipairs(result.messages or {}) do
    if msg.role == "user" and type(msg.content) == "table" then
        for _, block in ipairs(msg.content) do
            if type(block) == "table" and block.type == "tool_result" then
                captured = block.content
                break
            end
        end
    end
    if captured then break end
end

if not captured then
    log.error("FAIL: no tool_result found — compile_loop was never called")
    os.exit(2)
end

log.info("tool_result JSON: " .. tostring(captured):sub(1, 400))

-- ── Decode and assert shape ────────────────────────────────────────────────────
local dec_ok, tool_output = pcall(std.json.decode, captured)
if not dec_ok or type(tool_output) ~= "table" then
    log.error("FAIL: tool_result is not valid JSON: " .. tostring(captured))
    os.exit(2)
end

-- Required keys
assert(tool_output.ok ~= nil,      "FAIL: tool_output.ok is absent")
assert(tool_output.iters ~= nil,   "FAIL: tool_output.iters is absent")
assert(tool_output.summary ~= nil, "FAIL: tool_output.summary is absent")

-- Counter WF-A: code / history must NOT appear in tool output
assert(tool_output.code == nil,    "Counter WF-A: code leaked to caller")
assert(tool_output.history == nil, "Counter WF-A: history leaked to caller")

log.info("compile_loop result: ok=" .. tostring(tool_output.ok)
    .. " iters=" .. tostring(tool_output.iters)
    .. " summary=" .. tostring(tool_output.summary))

if tool_output.ok then
    log.info("PASS: compile_loop converged in " .. tool_output.iters .. " iter(s)")
    os.exit(0)
else
    log.warn("compile_loop did not converge (ok=false) — smoke test still PASSES")
    log.warn("failure_reason: " .. tostring(tool_output.failure_reason))
    os.exit(2)
end
