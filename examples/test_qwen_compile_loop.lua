-- test_qwen_compile_loop.lua — compile_loop e2e (Qwen vLLM)
--
-- Demonstrates blocks/coding_agent: structural Edit→Run→Feedback loop.
-- Uses the SAME deep_merge spec that broke Qwen in 3 manual iters,
-- now driven autonomously through the loop.
--
-- Run:
--   QWEN_BASE_URL=https://<pod>-8188.proxy.runpod.net/v1 \
--   OPENAI_API_KEY=dummy \
--   agent-block -s examples/test_qwen_compile_loop.lua

local coding = require("coding_agent")

local QWEN_BASE_URL = std.env.get("QWEN_BASE_URL")
if not QWEN_BASE_URL or QWEN_BASE_URL == "" then
    log.error("QWEN_BASE_URL not set")
    os.exit(2)
end

local TARGET = "/tmp/qwen_react_work.lua"

-- Runner: invoke the local lua interpreter on the file, capture all output.
local function lua_runner(file_path)
    local p = io.popen("lua " .. file_path .. " 2>&1; echo \"__EXIT__=$?\"", "r")
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

local res = coding.run({
    provider     = "openai",
    base_url     = QWEN_BASE_URL,
    api_key      = "dummy",
    model        = "qwen",
    target_file  = TARGET,
    lang         = "lua",
    spec         = SPEC,
    runner       = lua_runner,
    max_iters    = 5,
    max_tokens   = 2000,
    temperature  = 0.2,
    disable_thinking = true,
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
    log.info("final code (first 200 chars): " .. (res.code:sub(1,200) or ""))
end

if res.ok then
    log.info("PASS: coding_agent converged in " .. res.iters .. " iter(s)")
    os.exit(0)
else
    log.error("FAIL: did not converge")
    os.exit(2)
end
