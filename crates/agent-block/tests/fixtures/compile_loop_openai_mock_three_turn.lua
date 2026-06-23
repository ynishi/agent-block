-- Fixture: compile_loop OpenAI provider e2e test (3-turn) via in-process mock server.
--
-- Reads OPENAI_BASE_URL_TEST and COMPILE_LOOP_TARGET from environment
-- (set by the Rust test harness). Drives compile_loop.make().handler()
-- directly (no agent.run wrapper). Uses an inline mock_runner with a
-- call_count upvalue to enforce strict 3-call sequencing (Crux: call 1,2:
-- ok=false with distinct stderr, call 3: ok=true).
-- api_key is always "dummy" literal — OPENAI_API_KEY is never read (Crux #3).

local base_url = std.env.get("OPENAI_BASE_URL_TEST")
assert(base_url, "OPENAI_BASE_URL_TEST must be set")

local target_file = std.env.get("COMPILE_LOOP_TARGET")
assert(target_file, "COMPILE_LOOP_TARGET must be set")

local compile_loop = require("compile_loop")

-- Strict 3-call mock runner (Crux constraint: call 1,2 ok=false, call 3 ok=true).
-- call_count is an upvalue closure — each invocation increments it and
-- returns a distinct result keyed to call order:
--   call 1: {ok=false, stderr="forced fail iter 1"} → compile_loop retries
--   call 2: {ok=false, stderr="forced fail iter 2"} → compile_loop retries
--     (distinct stderr prevents is_stagnant from firing before turn 3)
--   call 3+: {ok=true} → compile_loop exits the loop
local call_count = 0
local function mock_runner(path)
    call_count = call_count + 1
    if call_count == 1 then
        return {ok=false, stderr="forced fail iter 1", exit_code=1}
    elseif call_count == 2 then
        return {ok=false, stderr="forced fail iter 2", exit_code=1}
    else
        return {ok=true, stdout="", exit_code=0}
    end
end

local td = compile_loop.make({
    runner = mock_runner,
    llm = { provider="openai", base_url=base_url, api_key="dummy", model="x" }
})
local result_json = td.handler({spec="emit a passing print statement", target_file=target_file})

-- Double-gate assertion: Lua side verifies call_count, Rust side verifies
-- the HTTP call counter on the mock server (both must equal 3).
assert(call_count == 3, "mock_runner must be called exactly 3 times, got " .. call_count)
print("COMPILE_LOOP_MOCK_PASS")
