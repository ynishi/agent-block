-- Fixture for the compile_loop tool_mode="none" e2e test.
--
-- Scenario (1 iteration, 1 LLM call):
--   The caller inlines both file contents in the spec and sets tool_mode="none",
--   so the request must NOT declare any tools (the mock records this; the Rust
--   side asserts tools_declared_count == 0). The mock returns path-header
--   SEARCH/REPLACE text for both files in a single turn.
--
-- This is the formalized path for the "strip tools via proxy" experiment from
-- issue #1: newer agentic models emit well-formed SR text when no tools are
-- declared.

local base_url = std.env.get("ANTHROPIC_BASE_URL_TEST")
assert(base_url, "ANTHROPIC_BASE_URL_TEST must be set")

local target_files_env = std.env.get("COMPILE_LOOP_TARGET_FILES")
assert(target_files_env, "COMPILE_LOOP_TARGET_FILES must be set")

local target_files = {}
for p in target_files_env:gmatch("[^:]+") do
    table.insert(target_files, p)
end
assert(#target_files == 2, "expected 2 paths in COMPILE_LOOP_TARGET_FILES, got " .. #target_files)
local file_a_path = target_files[1]
local file_b_path = target_files[2]

do
    local fa = assert(io.open(file_a_path, "w"))
    fa:write('print("a-old")\n')
    fa:close()

    local fb = assert(io.open(file_b_path, "w"))
    fb:write('print("b-old")\n')
    fb:close()
end

local compile_loop = require("compile_loop")

local runner_call_count = 0
local function mock_runner(paths)
    assert(type(paths) == "table", "multi-file mode must pass list to runner, got: " .. type(paths))
    runner_call_count = runner_call_count + 1

    local all_ok = true
    local combined_stdout = ""
    for _, p in ipairs(paths) do
        local f = io.open(p, "r")
        if not f then
            return { ok = false, stderr = "cannot open " .. p, stdout = "", exit_code = 1 }
        end
        local content = f:read("*a") or ""
        f:close()
        combined_stdout = combined_stdout .. content
        if not content:find("new", 1, true) then
            all_ok = false
        end
    end

    return { ok = all_ok, stdout = combined_stdout, stderr = "", exit_code = all_ok and 0 or 1 }
end

local td = compile_loop.make({
    runner = mock_runner,
    edit_mode = "diff",
    tool_mode = "none",
    llm = {
        provider = "anthropic",
        base_url = base_url,
        api_key = "dummy",
        model = "claude-haiku-mock",
    },
})

-- Caller inlines all target file contents in the spec (the tool_mode="none" contract).
local spec = "change a-old to a-new and b-old to b-new\n\n"
    .. "=== "
    .. file_a_path
    .. ' ===\nprint("a-old")\n\n'
    .. "=== "
    .. file_b_path
    .. ' ===\nprint("b-old")\n'

local result_json = td.handler({
    spec = spec,
    target_files = { file_a_path, file_b_path },
})

assert(runner_call_count >= 1, "mock_runner must be called at least once, got " .. runner_call_count)

local result = std.json.decode(result_json)
assert(result.ok, "compile_loop must succeed with tool_mode=none, got: " .. (result.summary or "?"))
assert(type(result.modified_files) == "table", "result.modified_files must be a table in multi-file mode")
assert(#result.modified_files == 2, "result.modified_files must contain 2 paths, got " .. #result.modified_files)

for _, p in ipairs({ file_a_path, file_b_path }) do
    local f = assert(io.open(p, "r"))
    local content = f:read("*a") or ""
    f:close()
    assert(content:find("new", 1, true), "file " .. p .. " must contain 'new' after apply, got: " .. content)
end

print("COMPILE_LOOP_TOOL_MODE_NONE_MOCK_PASS")
