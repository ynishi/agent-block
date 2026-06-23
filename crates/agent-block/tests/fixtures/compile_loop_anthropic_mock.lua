local base_url = std.env.get("ANTHROPIC_BASE_URL_TEST")
assert(base_url, "ANTHROPIC_BASE_URL_TEST must be set")

local target_file = std.env.get("COMPILE_LOOP_TARGET")
assert(target_file, "COMPILE_LOOP_TARGET must be set")

local compile_loop = require("compile_loop")

-- mock_runner uses a Lua upvalue to track call order.
-- Call 1 → {ok=false} (forced fail), call 2 → {ok=true} (pass).
-- This enforces strict fail-then-pass sequencing (Crux #2).
local call_count = 0
local function mock_runner(path)
    call_count = call_count + 1
    if call_count == 1 then
        return {ok=false, stderr="forced fail iter 1", exit_code=1}
    else
        return {ok=true, stdout="", exit_code=0}
    end
end

local td = compile_loop.make({
    runner = mock_runner,
    llm = {
        provider = "anthropic",
        base_url = base_url,
        api_key  = "dummy",
        model    = "claude-haiku-mock",
    },
})

local result_json = td.handler({
    spec        = "emit a passing print statement",
    target_file = target_file,
})

assert(call_count == 2,
    "mock_runner must be called exactly 2 times, got " .. call_count)

print("COMPILE_LOOP_MOCK_PASS")
