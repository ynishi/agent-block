-- verify_compile_loop_multi_delete.lua — Step 4-a: 削除 pattern
--
-- 2 file pre-existing: calc.lua に add/multiply/divide、main.lua に 3 種 assert。
-- spec で `divide` 関数 + 関連 assert の **削除** を要求。
-- 期待: SEARCH/REPLACE で REPLACE 部が空のブロックが両 file に出る。
-- add / multiply は両 file で保持されること。

local compile_loop = require("compile_loop")
local agent = require("agent")

local ANTHROPIC_API_KEY = std.env.get("ANTHROPIC_API_KEY")
if not ANTHROPIC_API_KEY or ANTHROPIC_API_KEY == "" then
    log.warn("ANTHROPIC_API_KEY not set — skipping")
    os.exit(2)
end

local MODEL = std.env.get_or("ANTHROPIC_MODEL", "claude-sonnet-4-6")
local TARGET_A = "/tmp/verify_del_calc.lua"
local TARGET_B = "/tmp/verify_del_main.lua"

local SENTINEL_A = [[-- Pre-existing helper file: calc operations
local M = {}

function M.add(a, b)
    return a + b
end

function M.multiply(a, b)
    return a * b
end

function M.divide(a, b)
    return a / b
end

assert(M.add(1, 2) == 3)
assert(M.multiply(2, 3) == 6)
assert(M.divide(10, 2) == 5)
print("ALL_PASS")
return M
]]

local SENTINEL_B = [[-- Pre-existing helper file: main entry
local calc = dofile("/tmp/verify_del_calc.lua")

assert(calc.add(10, 5) == 15)
assert(calc.multiply(4, 5) == 20)
assert(calc.divide(20, 4) == 5)
print("ALL_PASS")
]]

local function write_file(path, content)
    local f = io.open(path, "w")
    f:write(content)
    f:close()
    log.info("Pre-existing target_file written: " .. path)
end

write_file(TARGET_A, SENTINEL_A)
write_file(TARGET_B, SENTINEL_B)

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

local SPEC = [[REMOVE the `divide` functionality from BOTH files.

Requirements:
- /tmp/verify_del_calc.lua: Remove the `M.divide` function definition AND remove the `assert(M.divide(10, 2) == 5)` assertion.
- /tmp/verify_del_main.lua: Remove the `assert(calc.divide(20, 4) == 5)` assertion.
- KEEP all other functions and assertions intact (add, multiply must remain).
- Both files must continue to print "ALL_PASS" only when all remaining assertions pass.
- Do NOT add any new functions.
]]

local td = compile_loop.make({
    runner = lua_runner_multi,
    target_files = { TARGET_A, TARGET_B },
    edit_mode = "diff",
    max_iters = 3,
})

local result = agent.run({
    provider = "anthropic",
    api_key = ANTHROPIC_API_KEY,
    model = MODEL,
    max_tokens = 4096,
    max_iterations = 3,
    extra_tools = { td },
    prompt = string.format(
        "Use the compile_loop tool to modify the existing files.\nTarget files:\n  - %s\n  - %s\nSpec:\n%s",
        TARGET_A,
        TARGET_B,
        SPEC
    ),
})

log.info("=== RESULT === ok=" .. tostring(result.ok))

local function read_file(path)
    local rf = io.open(path, "r")
    if not rf then
        return ""
    end
    local c = rf:read("*a")
    rf:close()
    return c
end

local final_a = read_file(TARGET_A)
local final_b = read_file(TARGET_B)

log.info("=== FINAL " .. TARGET_A .. " ===")
log.info(final_a)
log.info("=== FINAL " .. TARGET_B .. " ===")
log.info(final_b)

-- Preservation: add / multiply 残存
local has_add_a = final_a:find("function M.add", 1, true) ~= nil
local has_multiply_a = final_a:find("function M.multiply", 1, true) ~= nil
local has_add_b = final_b:find("calc.add", 1, true) ~= nil
local has_multiply_b = final_b:find("calc.multiply", 1, true) ~= nil

-- Deletion: divide 消滅
local divide_gone_a = final_a:find("divide", 1, true) == nil
local divide_gone_b = final_b:find("divide", 1, true) == nil

log.info(
    "preservation: add_a="
        .. tostring(has_add_a)
        .. " mul_a="
        .. tostring(has_multiply_a)
        .. " add_b="
        .. tostring(has_add_b)
        .. " mul_b="
        .. tostring(has_multiply_b)
)
log.info("deletion: divide_gone_a=" .. tostring(divide_gone_a) .. " divide_gone_b=" .. tostring(divide_gone_b))

local preservation_ok = has_add_a and has_multiply_a and has_add_b and has_multiply_b
local deletion_ok = divide_gone_a and divide_gone_b

if result.ok and preservation_ok and deletion_ok then
    log.info("VERIFY PASS: divide removed from BOTH files, add/multiply preserved")
    os.exit(0)
else
    log.error(
        "VERIFY FAIL: result.ok="
            .. tostring(result.ok)
            .. " preservation="
            .. tostring(preservation_ok)
            .. " deletion="
            .. tostring(deletion_ok)
    )
    os.exit(1)
end
