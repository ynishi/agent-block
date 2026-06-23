-- Fixture: compile_loop distill subloop e2e test via in-process shared mock server.
--
-- Provider is selected by DISTILL_MOCK_PROVIDER ("openai" or "anthropic").
-- Base URL is read from OPENAI_BASE_URL_TEST or ANTHROPIC_BASE_URL_TEST accordingly.
-- Target file path is read from COMPILE_LOOP_DISTILL_TARGET (absolute path).
--
-- The fixture writes a large file (> READ_FILE_FULL_THRESHOLD = 10000 chars) to
-- the target path, so that compile_loop triggers the distill subloop on read_file.
-- File structure:
--   Lines 1-599: "-- padding line NNN\n"
--   Line 600:    "-- marker: REPLACE_ME\n"
-- Total: ~600 lines × ~25 chars ≈ 15000 chars → 3 distill chunks (200 lines each).
--
-- Mock returns:
--   Turn 0 (with tools):     tool_use=read_file for the target path
--   Turn 1 (distill calls):  raw text summaries (no tools in request, detected by mock)
--   Turn 2 (with tools + tool results): SR block changing REPLACE_ME → DONE
--
-- mock_runner validates that the file now contains "DONE".
-- Prints COMPILE_LOOP_DISTILL_MOCK_PASS on success.

local provider = std.env.get("DISTILL_MOCK_PROVIDER")
assert(provider, "DISTILL_MOCK_PROVIDER must be set (openai or anthropic)")

local base_url
if provider == "anthropic" then
    base_url = std.env.get("ANTHROPIC_BASE_URL_TEST")
    assert(base_url, "ANTHROPIC_BASE_URL_TEST must be set for anthropic provider")
else
    base_url = std.env.get("OPENAI_BASE_URL_TEST")
    assert(base_url, "OPENAI_BASE_URL_TEST must be set for openai provider")
end

local target_path = std.env.get("COMPILE_LOOP_DISTILL_TARGET")
assert(target_path, "COMPILE_LOOP_DISTILL_TARGET must be set")

-- Write the large target file (> 10000 chars to trigger distill).
do
    local f = assert(io.open(target_path, "w"))
    for i = 1, 599 do
        f:write(string.format("-- padding line %03d\n", i))
    end
    f:write("-- marker: REPLACE_ME\n")
    f:close()
end

local compile_loop = require("compile_loop")

-- mock_runner: called after SR apply. Checks that "DONE" is in the file.
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
        return {ok=true, stdout="marker found", stderr="", exit_code=0}
    end
    return {ok=false, stderr="DONE marker not found", stdout="", exit_code=1}
end

local td = compile_loop.make({
    runner    = mock_runner,
    edit_mode = "diff",
    llm = {
        provider = provider,
        base_url = base_url,
        api_key  = "dummy",
        model    = "mock-model",
    },
})

local result_json = td.handler({
    spec         = "replace REPLACE_ME marker with DONE marker",
    target_files = { target_path },
})

local result = std.json.decode(result_json)
assert(result.ok,
    "compile_loop must succeed in distill mode, got: " .. tostring(result.summary or "?"))

print("COMPILE_LOOP_DISTILL_MOCK_PASS")
