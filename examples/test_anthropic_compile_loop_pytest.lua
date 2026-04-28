-- test_anthropic_compile_loop_pytest.lua — compile_loop with pytest runner (Anthropic)
--
-- Demonstrates blocks/compile_loop: structural Edit→Run→Feedback loop
-- using an inline pytest runner and the Anthropic Messages API as the LLM backend.
--
-- conf.llm is OMITTED from compile_loop.make() so that the parent agent's
-- provider/model/api_key are inherited at call time via _AGENT_LLM_CTX.
--
-- Run:
--   agent-block -s examples/test_anthropic_compile_loop_pytest.lua
--   (.env is auto-loaded by agent-block; no manual source needed)
--
-- Exit codes:
--   0 = PASS (tool called, returned well-formed filtered shape)
--   2 = SKIP (ANTHROPIC_API_KEY not set, or pytest not available) or tool never called

local compile_loop = require("compile_loop")
local agent        = require("agent")

local ANTHROPIC_API_KEY = std.env.get("ANTHROPIC_API_KEY")
if not ANTHROPIC_API_KEY or ANTHROPIC_API_KEY == "" then
    log.warn("ANTHROPIC_API_KEY not set — skipping smoke test")
    os.exit(2)
end

-- Crux #2: detect pytest absence at runtime before any LLM call.
-- `python3 -m pytest --version` verifies the actual CLI entrypoint (not just
-- that python3 exists or that pytest is importable but broken).
local function check_pytest()
    local p = io.popen("python3 -m pytest --version 2>&1; echo __EXIT__=$?", "r")
    if not p then return false end
    local out = p:read("*a") or ""
    p:close()
    local exit_code = tonumber(out:match("__EXIT__=(%d+)%s*$") or "1")
    return exit_code == 0
end

if not check_pytest() then
    log.error("pytest not available: install via 'pip install pytest'")
    os.exit(2)
end

local MODEL  = std.env.get_or("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")
local TARGET = "/tmp/pytest_compile_loop_work.py"

-- Runner: invoke pytest on the target file, capture all output.
-- Crux #1: pass judgment requires BOTH exit_code == 0 AND at least one
-- "N passed" count in stdout. Hardcoding ok=true is never permitted.
local function pytest_runner(file_path)
    local cmd = string.format(
        "python3 -m pytest %s -v --tb=short 2>&1; echo __EXIT__=$?",
        file_path
    )
    local p = io.popen(cmd, "r")
    if not p then return { ok = false, stdout = "", stderr = "popen failed", exit_code = -1 } end
    local out = p:read("*a") or ""
    p:close()
    local exit_str  = out:match("__EXIT__=(%d+)%s*$") or "1"
    local exit_code = tonumber(exit_str) or 1
    out = out:gsub("__EXIT__=%d+%s*$", "")
    -- Crux #1: extract "N passed" count; require count > 0 to guard against
    -- "0 passed, 1 skipped" false-positives even when exit code is 0.
    local passed_count = tonumber(out:match("(%d+) passed") or "0")
    local pass = exit_code == 0 and passed_count > 0
    log.info(string.format("pytest exit=%d passed=%d ok=%s", exit_code, passed_count, tostring(pass)))
    return { ok = pass, stdout = out, stderr = "", exit_code = exit_code }
end

local SPEC = [[Write a single Python file (`/tmp/pytest_compile_loop_work.py`) that:
1. Defines `def add(a, b)` returning `a + b`.
2. Defines 4 pytest tests (each function name must start with `test_`):
   - `test_basic`: add(1, 2) == 3
   - `test_negative`: add(-1, 1) == 0
   - `test_zero`: add(0, 0) == 0
   - `test_float`: add(1.5, 2.5) == 4.0
Use standard `assert` statements inside each test function (no print, no unittest).
Output ONLY the file contents in a single ```python ... ``` block.]]

log.info("Model:       " .. MODEL)
log.info("Target file: " .. TARGET)

-- llm is omitted → parent agent's provider/model/api_key are inherited at
-- handler call time from _AGENT_LLM_CTX (same pattern as test_anthropic_compile_loop.lua).
local td = compile_loop.make({
    runner    = pytest_runner,
    lang      = "python",
    max_iters = 5,
})

local result = agent.run({
    provider       = "anthropic",
    api_key        = ANTHROPIC_API_KEY,
    model          = MODEL,
    max_tokens     = 2048,
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

log.info(string.format(
    "compile_loop result: ok=%s iters=%s summary=%s",
    tostring(tool_output.ok),
    tostring(tool_output.iters),
    tostring(tool_output.summary)
))
if tool_output.failure_reason then
    log.info("failure_reason: " .. tostring(tool_output.failure_reason))
end

os.exit(result.ok and 0 or 2)
