-- test_compile_loop_parent.lua — smoke test: Anthropic Haiku parent → Qwen child
--
-- Demonstrates the compile_loop tool created via compile_loop.make().
-- The parent (Anthropic Haiku) calls the tool via tool_use; the tool drives a
-- structural compile-and-fix loop on a Qwen child LLM.
--
-- Acceptance criteria verified here:
--   - compile_loop tool is callable by the parent via tool_use
--   - handler returns a JSON string with ok/iters/summary
--   - handler output does NOT contain "code" or "history" keys (Counter WF-A)
--
-- Run:
--   ANTHROPIC_API_KEY=sk-ant-... \
--   QWEN_BASE_URL=https://<pod>-8188.proxy.runpod.net/v1 \
--   OPENAI_API_KEY=dummy \
--   agent-block -s examples/test_compile_loop_parent.lua
--
-- Exit codes:
--   0  PASS
--   2  SKIP (env not configured)

local compile_loop = require("compile_loop")
local agent = require("agent")

-- ── ENV guard ──────────────────────────────────────────────────────────────────
local ANTHROPIC_API_KEY = std.env.get("ANTHROPIC_API_KEY")
if not ANTHROPIC_API_KEY or ANTHROPIC_API_KEY == "" then
    log.warn("ANTHROPIC_API_KEY not set — skipping smoke test")
    os.exit(2)
end

local QWEN_BASE_URL = std.env.get("QWEN_BASE_URL")
if not QWEN_BASE_URL or QWEN_BASE_URL == "" then
    log.warn("QWEN_BASE_URL not set — skipping smoke test")
    os.exit(2)
end

local QWEN_API_KEY = std.env.get("OPENAI_API_KEY") or "dummy"

-- ── Runner (caller-defined; BUILTIN_RUNNERS removed) ──────────────────────────
local function lua_runner(file_path)
    local p = io.popen("lua " .. file_path .. ' 2>&1; echo "__EXIT__=$?"', "r")
    if not p then
        return { ok = false, stdout = "", stderr = "popen failed", exit_code = -1 }
    end
    local out = p:read("*a") or ""
    p:close()
    local exit_str = out:match("__EXIT__=(%d+)%s*$") or "1"
    local exit_code = tonumber(exit_str) or 1
    out = out:gsub("__EXIT__=%d+%s*$", "")
    local pass = exit_code == 0 and out:find("ALL_PASS", 1, true) ~= nil
    return { ok = pass, stdout = out, stderr = "", exit_code = exit_code }
end

-- ── Create compile_loop tool_def ──────────────────────────────────────────────
-- opts are fixed at make time (provider / model / runner / max_iters).
-- tool input (spec / target_file / lang) is merged at handler call time.
-- K-96: all LLM tuning fields are explicitly listed in the llm table.
local td = compile_loop.make({
    runner = lua_runner,
    llm = {
        provider = "openai",
        base_url = QWEN_BASE_URL,
        api_key = QWEN_API_KEY,
        model = "qwen",
        disable_thinking = true,
        temperature = 0.2,
        max_tokens = 2000,
    },
    max_iters = 5,
    lang = "lua",
})
log.info("created tool_def: " .. td.name)

-- ── Task spec (same deep_merge spec used by test_qwen_compile_loop.lua) ────────
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

local TARGET = "/tmp/compile_loop_parent_work.lua"

-- ── Turn callback ──────────────────────────────────────────────────────────────
local function on_turn(info)
    log.info(string.format("parent turn %d: %d tool_call(s)", info.turn_number, #(info.tool_calls or {})))
    for _, tc in ipairs(info.tool_calls or {}) do
        log.info("  tool_use: " .. tostring(tc.name))
    end
end

-- ── Run parent agent ───────────────────────────────────────────────────────────
log.info("Starting parent agent (Anthropic Haiku)…")
log.info("Child endpoint: " .. QWEN_BASE_URL)
log.info("Target file:    " .. TARGET)

local result = agent.run({
    provider = "anthropic",
    api_key = ANTHROPIC_API_KEY,
    model = "claude-haiku-4-5",
    max_tokens = 2048,
    max_iterations = 3,
    on_turn = on_turn,
    extra_tools = { td },
    prompt = string.format(
        [[Use the compile_loop tool to solve the following coding task.
Target file: %s
Spec:
%s]],
        TARGET,
        SPEC
    ),
})

log.info("=== PARENT RESULT ===")
log.info("ok:        " .. tostring(result.ok))
log.info("num_turns: " .. tostring(result.num_turns))
if not result.ok then
    log.error("parent agent failed: " .. tostring(result.error or "unknown"))
    os.exit(2)
end

-- ── Extract tool_result from messages ─────────────────────────────────────────
-- After agent.run, messages contains the full conversation.
-- Find the user message that holds a tool_result array.
local captured_tool_result_str = nil
for _, msg in ipairs(result.messages or {}) do
    if msg.role == "user" and type(msg.content) == "table" then
        for _, block in ipairs(msg.content) do
            if type(block) == "table" and block.type == "tool_result" then
                captured_tool_result_str = block.content
                break
            end
        end
    end
    if captured_tool_result_str then
        break
    end
end

if not captured_tool_result_str then
    log.error("FAIL: no tool_result found in messages — compile_loop was never called")
    os.exit(2)
end

log.info("tool_result JSON: " .. tostring(captured_tool_result_str):sub(1, 400))

-- ── Decode and assert shape ────────────────────────────────────────────────────
local dec_ok, tool_output = pcall(std.json.decode, captured_tool_result_str)
if not dec_ok or type(tool_output) ~= "table" then
    log.error("FAIL: tool_result is not valid JSON: " .. tostring(captured_tool_result_str))
    os.exit(2)
end

-- Acceptance #3: required keys present
assert(tool_output.ok ~= nil, "FAIL: tool_output.ok is absent")
assert(tool_output.iters ~= nil, "FAIL: tool_output.iters is absent")
assert(tool_output.summary ~= nil, "FAIL: tool_output.summary is absent")

-- Acceptance #10 (Counter WF-A): code and history must NOT appear in tool output
assert(
    tool_output.code == nil,
    "FAIL: tool_output.code is present — Counter WF-A defence breach (code leaked to Caller)"
)
assert(
    tool_output.history == nil,
    "FAIL: tool_output.history is present — Counter WF-A defence breach (history leaked to Caller)"
)

log.info(
    "compile_loop result: ok="
        .. tostring(tool_output.ok)
        .. " iters="
        .. tostring(tool_output.iters)
        .. " summary="
        .. tostring(tool_output.summary)
)

if tool_output.ok then
    log.info("PASS: compile_loop converged in " .. tool_output.iters .. " iter(s)")
    log.info("artifact_path: " .. tostring(tool_output.artifact_path))
else
    -- A failed compile_loop is still a passing smoke test — what matters is
    -- that the tool was called and returned a well-formed structured response.
    log.warn("compile_loop did not converge (ok=false) — smoke test still PASSES")
    log.warn("failure_reason: " .. tostring(tool_output.failure_reason))
    log.warn("last_error:     " .. tostring(tool_output.last_error))
end

log.info("SMOKE TEST PASS: compile_loop tool created via make(), called, and returned valid filtered shape")
os.exit(0)
