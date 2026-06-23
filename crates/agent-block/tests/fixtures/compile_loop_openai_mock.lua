-- Fixture: compile_loop OpenAI provider e2e test via in-process mock server.
--
-- Reads OPENAI_BASE_URL_TEST and COMPILE_LOOP_TARGET from environment
-- (set by the Rust test harness). Drives compile_loop.make().handler()
-- directly (no agent.run wrapper). Uses an inline mock_runner with a
-- call_count upvalue to enforce strict fail-then-pass sequencing (Crux #2).
-- api_key is always "dummy" literal — OPENAI_API_KEY is never read (Crux #3).

local base_url = std.env.get("OPENAI_BASE_URL_TEST")
assert(base_url, "OPENAI_BASE_URL_TEST must be set")

local target_file = std.env.get("COMPILE_LOOP_TARGET")
assert(target_file, "COMPILE_LOOP_TARGET must be set")

local compile_loop = require("compile_loop")

-- Strict fail-then-pass mock runner (Crux #2 constitutional constraint).
-- call_count is an upvalue closure — each invocation increments it and
-- returns a distinct result keyed to call order:
--   call 1: {ok=false} → compile_loop retries with a second LLM call
--   call 2: {ok=true}  → compile_loop exits the loop
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
    llm = { provider="openai", base_url=base_url, api_key="dummy", model="x" }
})
local result_json = td.handler({spec="emit a passing print statement", target_file=target_file})

-- Double-gate assertion: Lua side verifies call_count, Rust side verifies
-- the HTTP call counter on the mock server (both must equal 2).
assert(call_count == 2, "mock_runner must be called exactly 2 times, got " .. call_count)
print("COMPILE_LOOP_MOCK_PASS")
