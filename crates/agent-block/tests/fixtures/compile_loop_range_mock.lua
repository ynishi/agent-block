-- Fixture: compile_loop's read tools against a file that is too big to send.
--
-- Scenario (Anthropic mock, 3 iterations):
--   Iter 1: the mock calls fs_read on the whole file. It is over the size
--           threshold, so the answer is its length and a pointer to
--           read_file_range — no digest, no summarising sub-call.
--   Iter 2: the mock calls read_file_range(path, 10, 20) and gets those lines
--           verbatim and line-numbered, which is what fs_edit addresses by.
--   Iter 3: the mock calls fs_edit on the marker line (REPLACE_ME → DONE).
--           The verify then passes.
--
-- The runner runs after every one of those iterations, including the two that
-- only read: the verify is the loop's step and not the model's.
--
-- File structure (600 lines, ~15000 chars, well above the threshold):
--   Lines 1-9:   "-- pre-range line N"
--   Lines 10-20: "-- verbatim-line-NN" (unique prefix for the range assertion)
--   Lines 21-599: "-- padding line NNN"
--   Line 600:    "-- marker: REPLACE_ME"
--
-- The mock (spawn_range_mock) is launched by the Rust test fn and its base URL
-- is passed via ANTHROPIC_BASE_URL_TEST.
--
-- Prints READ_FILE_RANGE_VERBATIM_PASS on success.

local base_url = std.env.get("ANTHROPIC_BASE_URL_TEST")
assert(base_url, "ANTHROPIC_BASE_URL_TEST must be set")

local target_path = std.env.get("COMPILE_LOOP_RANGE_TARGET")
assert(target_path, "COMPILE_LOOP_RANGE_TARGET must be set")

-- Write the large target file (> 10000 chars to exceed the read threshold).
-- Lines 10-20 carry a distinctive prefix so the test can confirm verbatim content.
do
    local f = assert(io.open(target_path, "w"))
    for i = 1, 9 do
        f:write(string.format("-- pre-range line %d\n", i))
    end
    for i = 10, 20 do
        f:write(string.format("-- verbatim-line-%02d\n", i))
    end
    for i = 21, 599 do
        f:write(string.format("-- padding line %03d\n", i))
    end
    f:write("-- marker: REPLACE_ME\n")
    f:close()
end

local compile_loop = require("compile_loop")

-- mock_runner: passes once the marker line says DONE.
local runner_call_count = 0
local function mock_runner(paths)
    runner_call_count = runner_call_count + 1
    local path = type(paths) == "table" and paths[1] or paths
    local f = io.open(path, "r")
    if not f then
        return { ok = false, stderr = "cannot open " .. tostring(path), stdout = "", exit_code = 1 }
    end
    local content = f:read("*a") or ""
    f:close()
    if content:find("-- marker: DONE", 1, true) then
        return { ok = true, stdout = "DONE marker found", stderr = "", exit_code = 0 }
    end
    return { ok = false, stderr = "DONE marker not found in file", stdout = "", exit_code = 1 }
end

local td = compile_loop.make({
    runner = mock_runner,
    edit_mode = "diff",
    llm = {
        provider = "anthropic",
        base_url = base_url,
        api_key = "dummy",
        model = "mock-model",
    },
})

local result_json = td.handler({
    spec = "change the marker line from REPLACE_ME to DONE",
    target_files = { target_path },
})

local result = std.json.decode(result_json)
assert(result.ok, "compile_loop must converge using the range read, got: " .. tostring(result.summary or "?"))

-- The verify ran after every iteration, not only the one that edited.
assert(runner_call_count == 3, "the runner must run once per iteration, got " .. runner_call_count)

print("READ_FILE_RANGE_VERBATIM_PASS")
