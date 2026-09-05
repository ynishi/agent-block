-- Fixture for the "broken OpenAI tool_calls shape" e2e test (OpenAI provider).
--
-- The mock returns a non-conformant-but-observed-in-the-wild shape:
--   * function.arguments is a JSON object (not a string) — Ollama native
--     /api/chat, Gemini functionCall.args, and some vLLM tool-call parsers
--     emit this shape through OpenAI-compatible endpoints.
--   * the id field is absent — a synthesized id must be put on the call so the
--     tool_result (role="tool", tool_call_id) pairing keeps working.
--
-- Scenario (2 iterations, 1 LLM call each):
--   Iter 1: two fs_edit tool_calls in the broken shape, aimed at text that is
--           not there. Both are rejected, so their results — carrying the
--           synthesized ids — have to go back in the next request.
--   Iter 2: the same two calls with a matching expect; both apply and the
--           verify passes.

local base_url = std.env.get("OPENAI_BASE_URL_TEST")
assert(base_url, "OPENAI_BASE_URL_TEST must be set")

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
    llm = {
        provider = "openai",
        base_url = base_url,
        api_key = "dummy",
        model = "gpt-mock",
    },
})

local result_json = td.handler({
    spec = "change a-old to a-new and b-old to b-new",
    target_files = { file_a_path, file_b_path },
})

assert(runner_call_count >= 1, "mock_runner must be called at least once, got " .. runner_call_count)

local result = std.json.decode(result_json)
assert(result.ok, "compile_loop must succeed despite object-arguments + missing ids, got: " .. (result.summary or "?"))
assert(result.iters == 2, "loop must converge in 2 iters (rejected, then applied), got " .. tostring(result.iters))
assert(type(result.modified_files) == "table", "result.modified_files must be a table in multi-file mode")
assert(#result.modified_files == 2, "result.modified_files must contain 2 paths, got " .. #result.modified_files)

for _, p in ipairs({ file_a_path, file_b_path }) do
    local f = assert(io.open(p, "r"))
    local content = f:read("*a") or ""
    f:close()
    assert(content:find("new", 1, true), "file " .. p .. " must contain 'new' after tool apply, got: " .. content)
end

print("COMPILE_LOOP_BROKEN_OPENAI_MOCK_PASS")
