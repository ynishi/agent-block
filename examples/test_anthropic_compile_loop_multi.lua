-- verify_compile_loop_multi.lua — verify multi-file diff mode with Sonnet 4.6
--
-- 2 file pre-existing 状態から spec で `subtract` 追加要求。
-- multi-file mode では LLM が `<<< path=... >>>` ヘッダ + SEARCH/REPLACE を両 file 分出す。
-- 結果として multiply / add / square は SEARCH/REPLACE 対象にならず保持され、
-- subtract 関数 + assert が両 file に追加されるのを確認。

local compile_loop = require("compile_loop")
local agent        = require("agent")

local ANTHROPIC_API_KEY = std.env.get("ANTHROPIC_API_KEY")
if not ANTHROPIC_API_KEY or ANTHROPIC_API_KEY == "" then
    log.warn("ANTHROPIC_API_KEY not set — skipping")
    os.exit(2)
end

local MODEL    = std.env.get_or("ANTHROPIC_MODEL", "claude-sonnet-4-6")
local TARGET_A = "/tmp/verify_multi_calc.lua"
local TARGET_B = "/tmp/verify_multi_main.lua"

local SENTINEL_A = [[-- Pre-existing helper file: calc operations
local M = {}

function M.multiply(a, b)
    return a * b
end

function M.add(a, b)
    return a + b
end

assert(M.add(1, 2) == 3)
assert(M.multiply(2, 3) == 6)
print("ALL_PASS")
return M
]]

local SENTINEL_B = [[-- Pre-existing helper file: main entry
local calc = dofile("/tmp/verify_multi_calc.lua")

local function square(n)
    return calc.multiply(n, n)
end

assert(square(4) == 16)
assert(calc.add(10, 5) == 15)
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

-- multi-file runner: list<string> を受けて全 file を順次 lua 実行、いずれか fail で ok=false。
local function lua_runner_multi(file_paths)
    local all_stdout = {}
    local all_stderr = {}
    local last_exit = 0
    local all_pass = true
    for _, path in ipairs(file_paths) do
        local p = io.popen("lua " .. path .. ' 2>&1; echo "__EXIT__=$?"', "r")
        if not p then
            return { ok = false, stdout = "", stderr = "popen failed for " .. path, exit_code = -1 }
        end
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

local SPEC = [[Add a `subtract(a, b)` function to BOTH files.

Requirements:
- /tmp/verify_multi_calc.lua: Add `M.subtract = function(a, b) return a - b end` and `assert(M.subtract(5, 3) == 2)` after the existing assertions.
- /tmp/verify_multi_main.lua: Add `assert(calc.subtract(10, 4) == 6)` after the existing assertions.
- Keep ALL existing functions (multiply / add / square) and assertions intact.
- Both files must print "ALL_PASS" only when all assertions pass.
]]

-- multi-file diff mode opt-in
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
        "Use the compile_loop tool to modify the existing files.\nTarget files:\n  - %s\n  - %s\nSpec:\n%s",
        TARGET_A, TARGET_B, SPEC
    ),
})

log.info("=== RESULT === ok=" .. tostring(result.ok))

local function read_file(path)
    local rf = io.open(path, "r")
    if not rf then return "" end
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

-- Preservation checks (full-rewrite ではなく minimal patch である証拠)
local has_multiply = final_a:find("multiply", 1, true) ~= nil
local has_add      = final_a:find("function M.add", 1, true) ~= nil or final_a:find("M.add =", 1, true) ~= nil
local has_square   = final_b:find("square", 1, true) ~= nil
local sentinel_a   = final_a:find("Pre%-existing helper file: calc operations", 1) ~= nil
local sentinel_b   = final_b:find("Pre%-existing helper file: main entry", 1) ~= nil

-- Addition checks
local subtract_a   = final_a:find("subtract", 1, true) ~= nil
local subtract_b   = final_b:find("subtract", 1, true) ~= nil

log.info("preservation: multiply=" .. tostring(has_multiply)
    .. " add=" .. tostring(has_add)
    .. " square=" .. tostring(has_square)
    .. " sentinel_a=" .. tostring(sentinel_a)
    .. " sentinel_b=" .. tostring(sentinel_b))
log.info("addition: subtract_a=" .. tostring(subtract_a)
    .. " subtract_b=" .. tostring(subtract_b))

local preservation_ok = has_multiply and has_add and has_square and sentinel_a and sentinel_b
local addition_ok     = subtract_a and subtract_b

if result.ok and preservation_ok and addition_ok then
    log.info("VERIFY PASS: minimal patch applied to BOTH files via path-aware SEARCH/REPLACE")
    os.exit(0)
else
    log.error("VERIFY FAIL: result.ok=" .. tostring(result.ok)
        .. " preservation=" .. tostring(preservation_ok)
        .. " addition=" .. tostring(addition_ok))
    os.exit(1)
end
