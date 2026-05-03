-- verify_compile_loop_multi_selective.lua — Step 4-b: selective edit
--
-- 2 file pre-existing。spec で「calc.lua のみに subtract 追加、main.lua は触るな」を要求。
-- 期待: path header が calc 側にしか出ない (main 側は byte 単位で不変)。

local compile_loop = require("compile_loop")
local agent        = require("agent")

local ANTHROPIC_API_KEY = std.env.get("ANTHROPIC_API_KEY")
if not ANTHROPIC_API_KEY or ANTHROPIC_API_KEY == "" then
    log.warn("ANTHROPIC_API_KEY not set — skipping")
    os.exit(2)
end

local MODEL    = std.env.get_or("ANTHROPIC_MODEL", "claude-sonnet-4-6")
local TARGET_A = "/tmp/verify_sel_calc.lua"
local TARGET_B = "/tmp/verify_sel_main.lua"

local SENTINEL_A = [[-- Pre-existing helper file: calc operations
local M = {}

function M.add(a, b)
    return a + b
end

function M.multiply(a, b)
    return a * b
end

assert(M.add(1, 2) == 3)
assert(M.multiply(2, 3) == 6)
print("ALL_PASS")
return M
]]

local SENTINEL_B = [[-- Pre-existing helper file: main entry (DO NOT MODIFY)
local calc = dofile("/tmp/verify_sel_calc.lua")

assert(calc.add(10, 5) == 15)
assert(calc.multiply(4, 5) == 20)
print("ALL_PASS")
]]

local function write_file(path, content)
    local f = io.open(path, "w")
    f:write(content)
    f:close()
    log.info("Pre-existing target_file written: " .. path)
end

local function read_file(path)
    local rf = io.open(path, "r")
    if not rf then return "" end
    local c = rf:read("*a")
    rf:close()
    return c
end

write_file(TARGET_A, SENTINEL_A)
write_file(TARGET_B, SENTINEL_B)

-- Capture pre-state of B for byte-equal check
local B_BEFORE = read_file(TARGET_B)

local function lua_runner_multi(file_paths)
    local all_stdout = {}
    local all_stderr = {}
    local last_exit = 0
    local all_pass = true
    for _, path in ipairs(file_paths) do
        local p = io.popen("lua " .. path .. ' 2>&1; echo "__EXIT__=$?"', "r")
        local out = p:read("*a") or ""
        p:close()
        local exit_str = out:match("__EXIT__=(%d+)%s*$") or "1"
        local exit_code = tonumber(exit_str) or 1
        out = out:gsub("__EXIT__=%d+%s*$", "")
        table.insert(all_stdout, "=== " .. path .. " (exit=" .. exit_code .. ") ===\n" .. out)
        if exit_code ~= 0 or not out:find("ALL_PASS", 1, true) then
            all_pass = false
            last_exit = exit_code ~= 0 and exit_code or 1
            table.insert(all_stderr, path .. ": exit=" .. exit_code)
        end
    end
    return {
        ok = all_pass,
        stdout = table.concat(all_stdout, "\n"),
        stderr = table.concat(all_stderr, "\n"),
        exit_code = last_exit,
    }
end

local SPEC = string.format([[Add `subtract(a, b)` to ONE file only.

Requirements:
- /tmp/verify_sel_calc.lua: Add `function M.subtract(a, b) return a - b end` and `assert(M.subtract(5, 3) == 2)` after the existing assertions.
- /tmp/verify_sel_main.lua: DO NOT MODIFY this file at all. It already passes and must remain byte-identical. Do not emit any SEARCH/REPLACE block for this file.
- Keep all existing functions in calc (add, multiply) intact.
- Both files must continue to print "ALL_PASS".
]])

local td = compile_loop.make({
    runner       = lua_runner_multi,
    target_files = { TARGET_A, TARGET_B },
    edit_mode    = "diff",
    max_iters    = 3,
})

local result = agent.run({
    provider       = "anthropic",
    api_key        = ANTHROPIC_API_KEY,
    model          = MODEL,
    max_tokens     = 4096,
    max_iterations = 3,
    extra_tools    = { td },
    prompt         = string.format(
        "Use the compile_loop tool. Edit ONLY %s. DO NOT touch %s.\nSpec:\n%s",
        TARGET_A, TARGET_B, SPEC
    ),
})

log.info("=== RESULT === ok=" .. tostring(result.ok))

local final_a = read_file(TARGET_A)
local final_b = read_file(TARGET_B)

log.info("=== FINAL " .. TARGET_A .. " ===")
log.info(final_a)
log.info("=== FINAL " .. TARGET_B .. " ===")
log.info(final_b)

-- A: subtract 追加 / 既存保持
local has_subtract_a  = final_a:find("subtract", 1, true) ~= nil
local has_add_a       = final_a:find("function M.add", 1, true) ~= nil
local has_multiply_a  = final_a:find("function M.multiply", 1, true) ~= nil

-- B: byte-identical
local b_unchanged     = (final_b == B_BEFORE)

log.info("a: subtract=" .. tostring(has_subtract_a)
    .. " add=" .. tostring(has_add_a)
    .. " mul=" .. tostring(has_multiply_a))
log.info("b: byte_unchanged=" .. tostring(b_unchanged)
    .. " (before=" .. #B_BEFORE .. " bytes, after=" .. #final_b .. " bytes)")

local addition_ok    = has_subtract_a and has_add_a and has_multiply_a

if result.ok and addition_ok and b_unchanged then
    log.info("VERIFY PASS: only calc edited, main byte-identical")
    os.exit(0)
else
    log.error("VERIFY FAIL: result.ok=" .. tostring(result.ok)
        .. " addition_ok=" .. tostring(addition_ok)
        .. " b_unchanged=" .. tostring(b_unchanged))
    os.exit(1)
end
