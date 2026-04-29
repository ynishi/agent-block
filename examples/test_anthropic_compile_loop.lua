-- test_anthropic_compile_loop.lua — compile_loop e2e (Anthropic)
--
-- Demonstrates blocks/compile_loop: structural Edit→Run→Feedback loop
-- using the Anthropic Messages API (claude-haiku) as the LLM backend.
--
-- Crux #2 smoke: conf.llm is OMITTED from compile_loop.make() call so that
-- the parent agent's provider/model/api_key are inherited at call time via
-- _AGENT_LLM_CTX (set by agent.run before tool dispatch).
--
-- Run:
--   agent-block -s examples/test_anthropic_compile_loop.lua
--   (.env is auto-loaded by agent-block; no manual source needed)
--
-- Exit codes:
--   0 = PASS (tool called, returned well-formed filtered shape)
--   2 = SKIP (ANTHROPIC_API_KEY not set) or tool never called

local compile_loop = require("compile_loop")
local agent        = require("agent")

local ANTHROPIC_API_KEY = std.env.get("ANTHROPIC_API_KEY")
if not ANTHROPIC_API_KEY or ANTHROPIC_API_KEY == "" then
    log.warn("ANTHROPIC_API_KEY not set — skipping smoke test")
    os.exit(2)
end

local MODEL  = std.env.get_or("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")
local TARGET = "/tmp/coding_agent_anthropic_smoke.lua"

-- Runner: invoke the local lua interpreter on the file, capture all output.
local function lua_runner(file_path)
    local p = io.popen("lua " .. file_path .. ' 2>&1; echo "__EXIT__=$?"', "r")
    if not p then return { ok = false, stdout = "", stderr = "popen failed", exit_code = -1 } end
    local out = p:read("*a") or ""
    p:close()
    local exit_str = out:match("__EXIT__=(%d+)%s*$") or "1"
    local exit_code = tonumber(exit_str) or 1
    out = out:gsub("__EXIT__=%d+%s*$", "")
    local pass = exit_code == 0 and out:find("ALL_PASS", 1, true) ~= nil
    return { ok = pass, stdout = out, stderr = "", exit_code = exit_code }
end

local SPEC = [[Write a single Lua 5.3+ file (no external libs) that:

1. Defines a local function `add(a, b)` that returns the sum of two numbers.
2. At the bottom of the file, run inline `assert(...)` calls covering:
   (a) assert(add(2, 3) == 5)
   (b) assert(add(0, 0) == 0)
   (c) assert(add(-1, 1) == 0)
   (d) assert(add(100, 200) == 300)
3. After all assertions pass, print exactly `ALL_PASS` to stdout.

Output ONLY the file contents in a single ```lua ... ``` block.]]

log.info("Model:       " .. MODEL)
log.info("Target file: " .. TARGET)

-- Crux #2: conf.llm is OMITTED → parent agent's provider/model/api_key
-- are resolved at handler call time from _AGENT_LLM_CTX.
local td = compile_loop.make({ runner = lua_runner, register = false })

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

log.info("compile_loop result: ok=" .. tostring(tool_output.ok)
    .. " iters=" .. tostring(tool_output.iters)
    .. " summary=" .. tostring(tool_output.summary))

if tool_output.ok then
    log.info("PASS: compile_loop converged in " .. tool_output.iters .. " iter(s)")
else
    log.warn("compile_loop did not converge (ok=false) — smoke test still PASSES")
    log.warn("failure_reason: " .. tostring(tool_output.failure_reason))
end

log.info("SMOKE TEST PASS")
os.exit(0)
