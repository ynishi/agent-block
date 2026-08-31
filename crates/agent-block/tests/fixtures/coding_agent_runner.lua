-- e2e: the coding_agent built-in "lua" runner.
--
-- The runner shells out through sh.exec, so it returns a real exit code and
-- separated stdout/stderr (no "__EXIT__=$?" marker). This fixture drives it via
-- coding_agent._test_helpers() because register_tool() only returns a tool name.
--
-- Requires the repository root as CWD so `require("coding_agent")` resolves
-- from project_root/blocks/, and RUNNER_SCRATCH_DIR pointing at a per-run
-- tempdir for the scratch files (see tests/e2e_coding_agent.rs). Fixed /tmp
-- paths would race concurrent runs and collide with other users' files on a
-- shared machine.

local coding = require("coding_agent")

local SCRATCH = std.env.get("RUNNER_SCRATCH_DIR")
assert(SCRATCH and SCRATCH ~= "", "RUNNER_SCRATCH_DIR must be set by the test harness")

local runner = coding._test_helpers().resolve_runner("lua")
assert(type(runner) == "function", "resolve_runner('lua') must return a function")

local function assert_contract(res, label)
    assert(type(res) == "table", label .. ": runner must return a table")
    assert(type(res.ok) == "boolean", label .. ": ok must be a boolean")
    assert(type(res.stdout) == "string", label .. ": stdout must be a string")
    assert(type(res.stderr) == "string", label .. ": stderr must be a string")
    assert(type(res.exit_code) == "number", label .. ": exit_code must be a number")
end

-- The runner invokes a standalone `lua` interpreter, which is not part of this
-- binary. Without one only the failure path is observable; say so loudly rather
-- than passing quietly on an untested contract.
local probe = sh.exec("command -v lua")
local has_lua = probe.ok and probe.code == 0

if not has_lua then
    local res = runner(std.path.join(SCRATCH, "no_interpreter.lua"))
    assert_contract(res, "missing-interpreter")
    assert(res.ok == false, "missing interpreter must not pass")
    assert(res.exit_code ~= 0, "missing interpreter must report a non-zero exit code")
    print("SKIP_NO_LUA")
    print("RUNNER_CONTRACT_OK")
    return
end

-- Pass case: exit 0 and ALL_PASS on stdout.
local pass_path = std.path.join(SCRATCH, "pass.lua")
std.fs.write(pass_path, 'print("ALL_PASS")\n')
local pass = runner(pass_path)
assert_contract(pass, "pass")
assert(pass.ok == true, "pass case must succeed, stderr=" .. pass.stderr)
assert(pass.exit_code == 0, "pass case exit_code must be 0, got " .. pass.exit_code)
assert(pass.stdout:find("ALL_PASS", 1, true) ~= nil, "pass case stdout must carry ALL_PASS")
assert(pass.stdout:find("__EXIT__", 1, true) == nil, "the __EXIT__ marker must be gone")

-- Fail case: the interpreter errors, so the diagnostics land on stderr — the
-- channel io.popen used to merge into stdout via `2>&1`.
local fail_path = std.path.join(SCRATCH, "fail.lua")
std.fs.write(fail_path, 'error("boom")\n')
local fail = runner(fail_path)
assert_contract(fail, "fail")
assert(fail.ok == false, "fail case must not pass")
assert(fail.exit_code ~= 0, "fail case must report a non-zero exit code")
assert(fail.stderr ~= "", "fail case diagnostics must arrive on stderr, not stdout")

print("RUNNER_CONTRACT_OK")
