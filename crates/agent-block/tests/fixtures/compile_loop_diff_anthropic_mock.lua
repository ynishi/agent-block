-- Fixture for compile_loop diff-mode e2e test (Anthropic mock).
--
-- Scenario (2 iterations):
--   Iter 1: Mock returns a SEARCH/REPLACE block with a wrong SEARCH text.
--           apply_blocks fails → failure feedback loop → 2nd LLM call.
--   Iter 2: Mock returns a correct SEARCH/REPLACE block.
--           apply_blocks succeeds → file updated → mock_runner returns {ok=true}.
--
-- Initial file content written before the loop:  print("hello")
-- After correct SEARCH/REPLACE:                  print("world")
-- mock_runner checks output contains "world" to determine pass.

local base_url = std.env.get("ANTHROPIC_BASE_URL_TEST")
assert(base_url, "ANTHROPIC_BASE_URL_TEST must be set")

local target_file = std.env.get("COMPILE_LOOP_TARGET")
assert(target_file, "COMPILE_LOOP_TARGET must be set")

-- Write initial file that the diff mode will read and patch.
do
    local f = assert(io.open(target_file, "w"))
    f:write('print("hello")\n')
    f:close()
end

local compile_loop = require("compile_loop")

-- mock_runner: call 1 → always fails (apply_blocks failed, file unchanged).
-- call 2 → file has been patched to print("world"); runner passes.
local runner_call_count = 0
local function mock_runner(path)
    runner_call_count = runner_call_count + 1
    -- Execute the file and check for "world" in output.
    local p = io.popen("lua " .. path .. " 2>&1", "r")
    if not p then
        return {ok=false, stderr="popen failed", stdout="", exit_code=-1}
    end
    local out = p:read("*a") or ""
    p:close()
    local passed = out:find("world", 1, true) ~= nil
    return {ok=passed, stdout=out, stderr="", exit_code=passed and 0 or 1}
end

local td = compile_loop.make({
    runner    = mock_runner,
    edit_mode = "diff",
    llm = {
        provider = "anthropic",
        base_url = base_url,
        api_key  = "dummy",
        model    = "claude-haiku-mock",
    },
})

local result_json = td.handler({
    spec        = "change print(\"hello\") to print(\"world\")",
    target_file = target_file,
})

-- The loop must have converged (2 LLM calls: 1 SEARCH fail + 1 success).
assert(runner_call_count >= 1,
    "mock_runner must be called at least once, got " .. runner_call_count)

local result = std.json.decode(result_json)
assert(result.ok, "compile_loop must succeed in diff mode, got: " .. (result.summary or "?"))

print("COMPILE_LOOP_DIFF_MOCK_PASS")
