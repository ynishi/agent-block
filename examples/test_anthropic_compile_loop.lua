-- test_anthropic_compile_loop.lua — compile_loop e2e (Anthropic)
--
-- Demonstrates blocks/coding_agent: structural Edit→Run→Feedback loop
-- using the Anthropic Messages API (claude-haiku) as the LLM backend.
--
-- Run:
--   agent-block -s examples/test_anthropic_compile_loop.lua
--   (.env is auto-loaded by agent-block; no manual source needed)
--
-- Exit codes:
--   0 = PASS (converged within max_iters)
--   1 = FAIL (did not converge or coding_agent returned error)
--   2 = SKIP (ANTHROPIC_API_KEY not set)

local coding = require("coding_agent")

local ANTHROPIC_API_KEY = std.env.get("ANTHROPIC_API_KEY")
if not ANTHROPIC_API_KEY or ANTHROPIC_API_KEY == "" then
    log.warn("ANTHROPIC_API_KEY not set — skipping smoke test")
    os.exit(2)
end

local MODEL = std.env.get_or("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")
local TARGET = "/tmp/coding_agent_anthropic_smoke.lua"

-- Runner: invoke the local lua interpreter on the file, capture all output.
-- Mirrors examples/test_qwen_compile_loop.lua. M.run requires opts.runner to be a
-- function (the runner_kind dispatch only happens inside M.register_tool).
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

local res = coding.run({
    provider     = "anthropic",
    api_key      = ANTHROPIC_API_KEY,
    model        = MODEL,
    target_file  = TARGET,
    lang         = "lua",
    spec         = SPEC,
    runner       = lua_runner,
    max_iters    = 5,
    max_tokens   = 2000,
    temperature  = 0.2,
    on_iter = function(info)
        local r = info.result
        log.info(string.format(
            "iter %d: ok=%s exit=%s stdout=%s",
            info.iter,
            tostring(r.ok),
            tostring(r.exit_code),
            (r.stdout or ""):gsub("\n", " | "):sub(1, 200)
        ))
    end,
})

log.info("=== RESULT ===")
log.info("ok:    " .. tostring(res.ok))
log.info("iters: " .. tostring(res.iters))
if res.failure_reason then log.info("failure_reason: " .. tostring(res.failure_reason)) end
if res.code then
    log.info("final code (first 200 chars): " .. (res.code:sub(1, 200) or ""))
end

if res.ok then
    print("ALL_PASS")
    os.exit(0)
else
    print("FAILED:", res.failure_reason, res.last_error)
    os.exit(1)
end
