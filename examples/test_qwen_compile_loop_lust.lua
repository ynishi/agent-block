-- test_qwen_compile_loop_lust.lua — compile_loop using mlua-probe (lua-debugger MCP)
-- as the runner. Structured per-test feedback instead of raw stdout grep.

local compile_loop = require("compile_loop")
local agent        = require("agent")

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

-- K-96: all LLM tuning fields are explicitly listed in the llm table.
local td = compile_loop.make({
    runner   = lust_runner,
    llm      = {
        provider         = "openai",
        base_url         = QWEN_BASE_URL,
        api_key          = "dummy",
        model            = "qwen",
        disable_thinking = true,
        temperature      = 0.2,
        max_tokens       = 2500,
    },
    max_iters = 5,
    lang      = "lua",
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

mcp.disconnect("luadbg")

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

log.info(string.format("ok=%s iters=%s", tostring(tool_output.ok), tostring(tool_output.iters)))
if tool_output.failure_reason then log.info("failure_reason: " .. tostring(tool_output.failure_reason)) end
os.exit(result.ok and 0 or 2)
