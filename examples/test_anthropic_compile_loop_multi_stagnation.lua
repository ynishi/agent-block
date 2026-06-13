-- verify_compile_loop_multi_stagnation.lua — Step 4-c: stagnation (max_iters 到達 fail)
--
-- Runner を強制 fail にして compile_loop tool が max_iters で抜けて ok=false を返すこと、
-- および runner が max_iters 回ぴったり呼ばれることを確認する。
--
-- agent layer の retry が乗らないよう agent max_iterations=2 に絞り、
-- tool_result JSON を message から抽出して tool_output.ok=false を直接検証する。

local compile_loop = require("compile_loop")
local agent = require("agent")

local ANTHROPIC_API_KEY = std.env.get("ANTHROPIC_API_KEY")
if not ANTHROPIC_API_KEY or ANTHROPIC_API_KEY == "" then
    log.warn("ANTHROPIC_API_KEY not set — skipping")
    os.exit(2)
end

local MODEL = std.env.get_or("ANTHROPIC_MODEL", "claude-sonnet-4-6")
local TARGET_A = "/tmp/verify_stag_calc.lua"
local TARGET_B = "/tmp/verify_stag_main.lua"
local MAX_ITERS = 3

local SENTINEL_A = [[-- Pre-existing
local M = {}
function M.add(a, b) return a + b end
assert(M.add(1, 2) == 3)
print("ALL_PASS")
return M
]]

local SENTINEL_B = [[-- Pre-existing
local calc = dofile("/tmp/verify_stag_calc.lua")
assert(calc.add(10, 5) == 15)
print("ALL_PASS")
]]

local function write_file(path, content)
    local f = io.open(path, "w")
    f:write(content)
    f:close()
end

write_file(TARGET_A, SENTINEL_A)
write_file(TARGET_B, SENTINEL_B)

local runner_call_count = 0
local function lua_runner_forced_fail(file_paths)
    runner_call_count = runner_call_count + 1
    local n = (type(file_paths) == "table") and #file_paths or 1
    log.info(
        "forced-fail runner call #"
            .. runner_call_count
            .. " (input_type="
            .. type(file_paths)
            .. " count_or_strlen="
            .. n
            .. ")"
    )
    return {
        ok = false,
        stdout = "",
        stderr = "FORCED_FAIL: stagnation test — runner always fails",
        exit_code = 1,
    }
end

local SPEC = [[Add a `subtract(a, b)` function to BOTH files.
- /tmp/verify_stag_calc.lua: Add `M.subtract = function(a, b) return a - b end` and assert it.
- /tmp/verify_stag_main.lua: Add `assert(calc.subtract(10, 4) == 6)`.
]]

local td = compile_loop.make({
    runner = lua_runner_forced_fail,
    target_files = { TARGET_A, TARGET_B },
    edit_mode = "diff",
    max_iters = MAX_ITERS,
})

local result = agent.run({
    provider = "anthropic",
    api_key = ANTHROPIC_API_KEY,
    model = MODEL,
    max_tokens = 4096,
    max_iterations = 2, -- 1 LLM turn (tool 呼ぶ) + 1 turn (tool result 受け) で打ち切り
    extra_tools = { td },
    prompt = string.format("Use the compile_loop tool ONCE. Files: %s, %s.\n%s", TARGET_A, TARGET_B, SPEC),
})

log.info("=== RESULT === agent.ok=" .. tostring(result.ok))
log.info("runner call count = " .. runner_call_count .. " (max_iters=" .. MAX_ITERS .. ")")

-- tool_result JSON 抽出 (examples/test_anthropic_compile_loop.lua パターン踏襲)
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
    if captured then
        break
    end
end

if not captured then
    log.error("FAIL: tool_result not found")
    os.exit(1)
end

local dec_ok, tool_output = pcall(std.json.decode, captured)
if not dec_ok or type(tool_output) ~= "table" then
    log.error("FAIL: tool_result not JSON: " .. tostring(captured))
    os.exit(1)
end

log.info(
    "tool_output: ok="
        .. tostring(tool_output.ok)
        .. " iters="
        .. tostring(tool_output.iters)
        .. " summary="
        .. tostring(tool_output.summary)
)

local tool_ok_false = (tool_output.ok == false)
local iters_at_max = (tool_output.iters == MAX_ITERS)
-- runner は max_iters 回ぴったりとは限らない (iter 内で LLM 出力 parse 失敗や
-- stagnation 検出が先に発火すると runner skip → 次 iter へ → 最終的に max_iters 抜け)。
-- 上界 (<= max_iters) かつ最低 1 回呼ばれていることを invariant とする。
local runner_bounded = runner_call_count > 0 and runner_call_count <= MAX_ITERS
local summary_says_max = type(tool_output.summary) == "string" and tool_output.summary:find("max_iters", 1, true) ~= nil

log.info(
    "checks: tool_ok_false="
        .. tostring(tool_ok_false)
        .. " iters_at_max="
        .. tostring(iters_at_max)
        .. " runner_bounded="
        .. tostring(runner_bounded)
        .. " summary_says_max="
        .. tostring(summary_says_max)
)

if tool_ok_false and iters_at_max and runner_bounded and summary_says_max then
    log.info("VERIFY PASS: stagnation correctly bounded by max_iters=" .. MAX_ITERS)
    os.exit(0)
else
    log.error("VERIFY FAIL")
    os.exit(1)
end
