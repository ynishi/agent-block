-- test_qwen_compile_loop_lust.lua — compile_loop using mlua-probe (lua-debugger MCP)
-- as the runner. Structured per-test feedback instead of raw stdout grep.
local coding = require("coding_agent")

local QWEN_BASE_URL = std.env.get("QWEN_BASE_URL")
if not QWEN_BASE_URL or QWEN_BASE_URL == "" then
    log.error("QWEN_BASE_URL not set")
    os.exit(2)
end

local TARGET = "/tmp/qwen_react_lust.lua"

-- Connect to mlua-probe MCP server (stdio).
mcp.connect("luadbg", "mlua-probe-mcp", {})

-- Runner: invoke test_launch on the file, transform structured result into the
-- runner contract {ok, stdout, stderr, exit_code}.
local function lust_runner(file_path)
    local r = mcp.call("luadbg", "test_launch", { code_file = file_path })
    if not r.ok then
        return { ok = false, stdout = "", stderr = "mcp.call failed: " .. tostring(r.error or ""), exit_code = -1 }
    end
    -- The MCP wraps content blocks; first block should be JSON text.
    local txt = (r.content and r.content[1] and r.content[1].text) or ""
    local ok_decode, parsed = pcall(std.json.decode, txt)
    if not ok_decode or type(parsed) ~= "table" then
        return { ok = false, stdout = txt, stderr = "JSON decode failed", exit_code = -1 }
    end
    -- Build a human-readable diagnostic that the LLM can act on.
    local lines = {}
    table.insert(lines, string.format("total=%d passed=%d failed=%d",
        parsed.total or 0, parsed.passed or 0, parsed.failed or 0))
    for _, t in ipairs(parsed.tests or {}) do
        if t.passed then
            table.insert(lines, string.format("  PASS  %s :: %s", tostring(t.suite), tostring(t.name)))
        else
            table.insert(lines, string.format("  FAIL  %s :: %s\n        error: %s",
                tostring(t.suite), tostring(t.name), tostring(t.error or "(no message)")))
        end
    end
    local pass = (parsed.failed or 0) == 0 and (parsed.total or 0) > 0
    return {
        ok        = pass,
        stdout    = table.concat(lines, "\n"),
        stderr    = "",
        exit_code = pass and 0 or 1,
    }
end

local SPEC = [[Write a single Lua 5.3+ file (no external libs) that:

1. Defines `local M = {}` and `M.deepcopy(t)` returning a deep copy with these properties:
   - Nested tables are recursively cloned (no shared references with input).
   - Cycles are handled (`t.self = t` must NOT cause infinite recursion; preserve cycle topology).
   - Metatables are preserved on every cloned table (set the SAME metatable identity, not a clone).
   - Tables used as keys are also deep-copied with identity preservation across multiple appearances.
   - Non-table values (number / string / boolean / function / userdata) are referenced as-is (no clone).

2. **Use the mlua-lspec test framework**, NOT raw `assert(...)`. The `lust` global is pre-loaded.
   Pattern:
   ```lua
   local describe, it, expect = lust.describe, lust.it, lust.expect
   describe("deepcopy", function()
       it("clones nested table", function()
           expect(...).to.equal(...)
       end)
   end)
   ```
   Add at least 5 it() cases:
   (a) primitives untouched / shallow array clone is a different table but same values
   (b) cycle handling (`t.self=t`)
   (c) metatable identity preserved
   (d) shared sub-table identity preserved across multiple references
   (e) function values are NOT cloned (reference equality)

3. End with `return M`. **Do NOT print anything**, do NOT use `assert()`, do NOT call `os.exit()`. The test runner reads results from lust automatically.

Output ONLY the file contents in a single ```lua ... ``` block.]]

log.info("compile_loop + mlua-probe lust runner. Target: " .. TARGET)

local res = coding.run({
    provider     = "openai",
    base_url     = QWEN_BASE_URL,
    api_key      = "dummy",
    model        = "qwen",
    target_file  = TARGET,
    lang         = "lua",
    spec         = SPEC,
    runner       = lust_runner,
    max_iters    = 5,
    max_tokens   = 2500,
    temperature  = 0.2,
    disable_thinking = true,
    on_iter = function(info)
        local r = info.result
        log.info(string.format("iter %d: ok=%s", info.iter, tostring(r.ok)))
        for line in (r.stdout or ""):gmatch("[^\n]+") do
            log.info("  " .. line)
        end
    end,
})

mcp.disconnect("luadbg")

log.info("=== RESULT ===")
log.info(string.format("ok=%s iters=%d", tostring(res.ok), res.iters))
if res.failure_reason then log.info("failure_reason: " .. tostring(res.failure_reason)) end
os.exit(res.ok and 0 or 2)
