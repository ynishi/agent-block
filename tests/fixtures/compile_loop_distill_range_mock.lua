-- Fixture: compile_loop read_file_range verbatim e2e test (crux-card §3).
--
-- Verifies that read_file_range returns verbatim source lines without distillation,
-- even when the file as a whole exceeds READ_FILE_FULL_THRESHOLD (crux-card §3).
--
-- Scenario (Anthropic mock, 2 turns):
--   Turn 0: mock returns tool_use=read_file_range(path, 10, 20).
--           compile_loop dispatches to read_file_range_tool_handler.
--           Handler reads lines 10-20 verbatim from the large target file.
--   Turn 1: mock returns SR block (REPLACE_ME → DONE) using the tool result.
--           compile_loop applies the SR block → mock_runner checks for DONE.
--
-- File structure (600 lines, ~15000 chars, well above threshold):
--   Lines 1-9:   "-- pre-range line N"
--   Lines 10-20: "-- verbatim-line-NN" (unique prefix for range assertion)
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

-- Write the large target file (> 10000 chars to exceed READ_FILE_FULL_THRESHOLD).
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

-- mock_runner: verifies that "DONE" marker was applied (SR from mock turn 1).
local runner_call_count = 0
local function mock_runner(paths)
    runner_call_count = runner_call_count + 1
    local path = type(paths) == "table" and paths[1] or paths
    local f = io.open(path, "r")
    if not f then
        return {ok=false, stderr="cannot open " .. tostring(path), stdout="", exit_code=1}
    end
    local content = f:read("*a") or ""
    f:close()
    if content:find("DONE", 1, true) then
        return {ok=true, stdout="DONE marker found", stderr="", exit_code=0}
    end
    return {ok=false, stderr="DONE marker not found in file", stdout="", exit_code=1}
end

local td = compile_loop.make({
    runner    = mock_runner,
    edit_mode = "diff",
    llm = {
        provider = "anthropic",
        base_url = base_url,
        api_key  = "dummy",
        model    = "mock-model",
    },
})

local result_json = td.handler({
    spec         = "apply REPLACE_ME → DONE using read_file_range for verbatim access",
    target_files = { target_path },
})

local result = std.json.decode(result_json)
assert(result.ok,
    "compile_loop must succeed with read_file_range verbatim access, got: "
    .. tostring(result.summary or "?"))

print("READ_FILE_RANGE_VERBATIM_PASS")
