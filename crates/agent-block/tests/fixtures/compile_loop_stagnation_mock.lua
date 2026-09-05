-- compile_loop gives up when the verify keeps saying the same thing.
--
-- Every iteration lands a real edit — a different line each time — so the
-- "no edits applied" counter never trips. What fires is the stagnation policy
-- over the verify's stderr, which is identical on every iteration: three is the
-- count at which "again" stops being a retry and starts being a pattern.
--
-- The budget is 10 iterations and the run must stop at 3, so the give-up is the
-- policy's and not the ceiling's.

local cl = require("compile_loop")

local target = std.env.get("COMPILE_LOOP_TARGET")
assert(target, "COMPILE_LOOP_TARGET must be set")

do
    local f = assert(io.open(target, "w"))
    for i = 1, 6 do
        f:write(string.format("line-%d\n", i))
    end
    f:close()
end

-- Always the same failure, whatever the file says.
local runner_calls = 0
local function runner(_path)
    runner_calls = runner_calls + 1
    return { ok = false, stdout = "", stderr = "boom", exit_code = 1 }
end

local td = cl.make({
    runner = runner,
    edit_mode = "diff",
    max_iters = 10,
    llm = {
        provider = "anthropic",
        base_url = std.env.get("ANTHROPIC_BASE_URL_TEST"),
        api_key = "dummy",
        model = "claude-haiku-mock",
    },
})

local result = std.json.decode(td.handler({
    spec = "edit one line per iteration",
    target_file = target,
}))

assert(runner_calls == 3, "the runner must run once per iteration, got " .. runner_calls)
assert(
    type(result.modified_files) == "table" and #result.modified_files == 1,
    "the edits that landed are reported, even on a give-up"
)

print("[STAG] ok=" .. tostring(result.ok))
print("[STAG] failure_reason=" .. tostring(result.failure_reason))
print("[STAG] iters=" .. tostring(result.iters))
print("[STAG] last_error=" .. tostring(result.last_error))
