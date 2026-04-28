-- test_qwen_coding_react_hard.lua — harder Lua spec to exercise loop iterations
-- Target: deepcopy that survives cycles + metatables + tables-as-keys.
local coding = require("coding_agent")

local QWEN_BASE_URL = std.env.get("QWEN_BASE_URL")
if not QWEN_BASE_URL or QWEN_BASE_URL == "" then
    log.error("QWEN_BASE_URL not set")
    os.exit(2)
end

local TARGET = "/tmp/qwen_react_deepcopy.lua"

local function lua_runner(file_path)
    local p = io.popen("lua " .. file_path .. " 2>&1; echo \"__EXIT__=$?\"", "r")
    if not p then return { ok=false, stdout="", stderr="popen failed", exit_code=-1 } end
    local out = p:read("*a") or ""
    p:close()
    local exit_str = out:match("__EXIT__=(%d+)%s*$") or "1"
    local exit_code = tonumber(exit_str) or 1
    out = out:gsub("__EXIT__=%d+%s*$", "")
    local pass = exit_code == 0 and out:find("ALL_PASS", 1, true) ~= nil
    return { ok = pass, stdout = out, stderr = "", exit_code = exit_code }
end

local SPEC = [[Write a single Lua 5.3+ file (no external libs) defining `local M = {}` and `M.deepcopy(t)` that returns a deep copy of `t` such that:

(1) Nested tables are recursively cloned (no shared references with input).
(2) **Cycles are handled** — `t.self = t` does NOT cause infinite recursion. The output preserves the same cycle topology.
(3) **Metatables are preserved** on every cloned table (use setmetatable on the copy with the original metatable).
(4) **Tables used as keys** are also deep-copied, with identity-preservation: if the same key-table appears in multiple places, the SAME copy must be used in all.
(5) Non-table values (number / string / boolean / nil / function / userdata) are copied by reference (no clone).

At the bottom of the file, run inline `assert(...)` calls verifying:
- (a) shallow primitive: `M.deepcopy({1,2,3})` equals `{1,2,3}` element-wise but is a different table.
- (b) nested: deep clone, modifying clone does not affect original.
- (c) cycle: `local t={}; t.self=t; local c=M.deepcopy(t); assert(c.self == c); assert(c ~= t)`
- (d) metatable: original has metatable with `__index`; clone has the SAME metatable (same identity, not cloned).
- (e) shared sub-table identity: `local s={}; local t={a=s, b=s}; local c=M.deepcopy(t); assert(c.a == c.b); assert(c.a ~= s)`
- (f) function value: function values are NOT cloned (reference equality preserved).

Print `ALL_PASS` on success. End with `return M`.

Output ONLY the file in a single ```lua ... ``` block.]]

log.info("Running CodingReact hard task: " .. TARGET)

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
    max_tokens   = 2500,
    temperature  = 0.2,
    disable_thinking = true,
    on_iter = function(info)
        local r = info.result
        log.info(string.format(
            "iter %d: ok=%s exit=%s",
            info.iter, tostring(r.ok), tostring(r.exit_code)
        ))
        if not r.ok then
            log.info("  stdout: " .. (r.stdout or ""):gsub("\n", " | "):sub(1,300))
        end
    end,
})

log.info("=== RESULT ===")
log.info(string.format("ok=%s iters=%d", tostring(res.ok), res.iters))
if res.error then log.info("error: " .. tostring(res.error)) end
os.exit(res.ok and 0 or 2)
