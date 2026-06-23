-- Fixture for compile_loop multi-file diff-mode 2-iter e2e test (Anthropic mock).
--
-- Scenario (2 iterations):
--   Iter 1: Mock returns file_a SEARCH/REPLACE with wrong SEARCH text ("WRONG").
--           apply_blocks fails for file_a → failure feedback loop → 2nd LLM call.
--   Iter 2: Mock returns correct SEARCH/REPLACE for both file_a and file_b.
--           apply_blocks succeeds → mock_runner returns {ok=true}.
--
-- Initial file contents written before the loop:
--   file_a: print("a-old")
--   file_b: print("b-old")
-- After correct SEARCH/REPLACE apply:
--   file_a: print("a-new")
--   file_b: print("b-new")

local base_url = std.env.get("ANTHROPIC_BASE_URL_TEST")
assert(base_url, "ANTHROPIC_BASE_URL_TEST must be set")

local target_files_env = std.env.get("COMPILE_LOOP_TARGET_FILES")
assert(target_files_env, "COMPILE_LOOP_TARGET_FILES must be set")

-- Parse colon-separated paths.
local target_files = {}
for p in target_files_env:gmatch("[^:]+") do
    table.insert(target_files, p)
end
assert(#target_files == 2, "expected 2 paths in COMPILE_LOOP_TARGET_FILES, got " .. #target_files)
local file_a_path = target_files[1]
local file_b_path = target_files[2]

-- Write initial file contents that the diff mode will read and patch.
do
    local fa = assert(io.open(file_a_path, "w"))
    fa:write('print("a-old")\n')
    fa:close()

    local fb = assert(io.open(file_b_path, "w"))
    fb:write('print("b-old")\n')
    fb:close()
end

local compile_loop = require("compile_loop")

-- mock_runner receives a list of paths (multi-file mode, Crux #3 runner signature toggle).
-- Iter 1: file_a apply failed (SEARCH mismatch), file unchanged → runner may or may not be called.
-- Iter 2: both files contain "new" after correct apply → passes.
local runner_call_count = 0
local function mock_runner(paths)
    assert(type(paths) == "table", "multi-file mode must pass list to runner, got: " .. type(paths))
    runner_call_count = runner_call_count + 1

    local all_ok = true
    local combined_stdout = ""
    for _, p in ipairs(paths) do
        local f = io.open(p, "r")
        if not f then
            return {ok=false, stderr="cannot open " .. p, stdout="", exit_code=1}
        end
        local content = f:read("*a") or ""
        f:close()
        combined_stdout = combined_stdout .. content
        if not content:find("new", 1, true) then
            all_ok = false
        end
    end

    return {ok=all_ok, stdout=combined_stdout, stderr="", exit_code=all_ok and 0 or 1}
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
    spec         = "change a-old to a-new and b-old to b-new",
    target_files = { file_a_path, file_b_path },
})

-- 2-iter path: runner called at least once (after successful apply on iter 2).
assert(runner_call_count >= 1,
    "mock_runner must be called at least once, got " .. runner_call_count)

local result = std.json.decode(result_json)
assert(result.ok, "compile_loop must succeed in multi-file diff mode (2-iter), got: " .. (result.summary or "?"))

-- multi-file mode: modified_files must be a list of 2 paths.
assert(type(result.modified_files) == "table",
    "result.modified_files must be a table in multi-file mode")
assert(#result.modified_files == 2,
    "result.modified_files must contain 2 paths, got " .. #result.modified_files)

-- multi-file mode: artifact_path must be nil.
assert(result.artifact_path == nil,
    "result.artifact_path must be nil in multi-file mode, got: " .. tostring(result.artifact_path))

-- Verify each file was actually updated.
for _, p in ipairs({ file_a_path, file_b_path }) do
    local f = assert(io.open(p, "r"))
    local content = f:read("*a") or ""
    f:close()
    assert(content:find("new", 1, true),
        "file " .. p .. " must contain 'new' after apply, got: " .. content)
end

print("COMPILE_LOOP_DIFF_MULTI_MOCK_TWO_ITER_PASS")
