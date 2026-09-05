-- compile_loop tool_mode = "read_only" — the edit tool is withheld.
--
-- A read-only run is a dry run: it can look at the file and it cannot change
-- it, so it cannot converge either. What this fixture pins is the shape of that
-- ending — the loop spends its budget and gives up with max_iters — while the
-- Rust side asserts what the request declared (fs_read yes, fs_edit no).

local cl = require("compile_loop")

local target = std.env.get("COMPILE_LOOP_TARGET")
assert(target, "COMPILE_LOOP_TARGET must be set")

do
    local f = assert(io.open(target, "w"))
    f:write('print("hello")\n')
    f:close()
end

-- Nothing can make this pass, because nothing can write to the file.
local runner_calls = 0
local function runner(_path)
    runner_calls = runner_calls + 1
    return { ok = false, stdout = "", stderr = "still hello", exit_code = 1 }
end

local td = cl.make({
    runner = runner,
    edit_mode = "diff",
    tool_mode = "read_only",
    max_iters = 1,
    llm = {
        provider = "anthropic",
        base_url = std.env.get("ANTHROPIC_BASE_URL_TEST"),
        api_key = "dummy",
        model = "claude-haiku-mock",
    },
})

local result = std.json.decode(td.handler({
    spec = 'change print("hello") to print("world")',
    target_file = target,
}))

-- The verify still ran: it is the loop's step, and a read-only run is checked
-- like any other.
assert(runner_calls == 1, "the runner must run once for the one iteration, got " .. runner_calls)

print("[RO] ok=" .. tostring(result.ok))
print("[RO] failure_reason=" .. tostring(result.failure_reason))
print("[RO] iters=" .. tostring(result.iters))
