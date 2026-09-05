-- blocks/tools/coding_agent/init.lua — Thin backward-compatible facade over compile_loop.
--
-- Primary surface is now compile_loop.make(conf) in blocks/tools/compile_loop/init.lua.
-- This module remains for callers that used coding_agent.run() / register_tool().
--
-- NOTE: coding_agent.run() return shape is NOW:
--   { ok, iters, summary, failure_reason?, last_error?, artifact_path }
-- Fields "code" and "history" are NO LONGER returned (Q3 = A, breaking accepted).
-- Counter WF-A defence is maintained via filter_for_tool_output inside compile_loop.

local M = {}

local cl = require("compile_loop")

-- ============================================================
-- BUILTIN_RUNNERS — facade-local only (Issue §確定 5)
-- compile_loop itself does NOT have these; callers that pass runner_kind string
-- get them resolved here before the function-only compile_loop API is invoked.
--
-- Both runners go through sh.exec, not io.popen: sh.exec strips the host's own
-- credential env vars from the child (io.popen does not), and it returns a real
-- exit code plus separated stdout/stderr, so no "__EXIT__=$?" marker is needed.
--
-- cwd: sh.exec defaults to the project root, so that — not the host process's
-- cwd, which io.popen used to inherit — is where a runner command executes.
-- This is deliberate: it makes the working directory deterministic regardless
-- of where agent-block was started from. The cargo runner overrides it with the
-- target file's directory via opts.cwd; any runner needing another directory
-- does the same.
-- ============================================================

-- Wall-clock budget per runner invocation (seconds). io.popen had none, so a
-- hung compiler/interpreter used to stall the whole loop.
local LUA_RUNNER_TIMEOUT = 60
local CARGO_RUNNER_TIMEOUT = 300

-- Map an sh.exec result ({ok, code, stdout, stderr} | {ok=false, error}) onto
-- the runner contract compile_loop expects ({ok, stdout, stderr, exit_code}).
-- `ok` on the sh.exec side means "the command ran"; `ok` on the runner side
-- means "the code under test passed", hence the separate `pass` argument.
local function exec_result(res, pass_fn)
    if not res.ok then
        -- Spawn failure or timeout: no exit code exists, so report -1.
        return { ok = false, stdout = "", stderr = tostring(res.error), exit_code = -1 }
    end
    local stdout = res.stdout or ""
    local stderr = res.stderr or ""
    return {
        ok = res.code == 0 and pass_fn(stdout, stderr),
        stdout = stdout,
        stderr = stderr,
        exit_code = res.code,
    }
end

local BUILTIN_RUNNERS = {
    -- "lua" runner: invoke lua interpreter, pass/fail by exit 0 + "ALL_PASS" in stdout
    lua = function(file_path)
        local res = sh.exec("lua " .. file_path, { timeout = LUA_RUNNER_TIMEOUT })
        return exec_result(res, function(stdout)
            return stdout:find("ALL_PASS", 1, true) ~= nil
        end)
    end,
    -- "cargo" runner: run cargo test --offline in the target dir, pass on
    -- exit 0 + "test result: ok"
    cargo = function(file_path)
        local dir = file_path:match("^(.*)/[^/]+$") or "."
        local res = sh.exec("cargo test --offline", { cwd = dir, timeout = CARGO_RUNNER_TIMEOUT })
        return exec_result(res, function(stdout, stderr)
            -- cargo reports the summary on stdout and compiler diagnostics on
            -- stderr; the old `2>&1` form searched the merged text, so do too.
            return (stdout .. stderr):find("test result: ok", 1, true) ~= nil
        end)
    end,
}

-- Resolve runner_kind (string or function) → runner function.
-- Returns (fn, nil) on success, (nil, err_string) on failure.
local function resolve_runner(kind)
    if type(kind) == "function" then
        return kind, nil
    end
    if type(kind) == "string" then
        local fn = BUILTIN_RUNNERS[kind]
        if fn then
            return fn, nil
        end
        return nil, "unknown runner_kind: " .. kind
    end
    return nil, "runner_kind must be a string or function, got: " .. type(kind)
end

-- ============================================================
-- M.run(opts) — thin facade (backward-compatible signature)
-- ============================================================
-- Return shape: { ok, iters, summary, failure_reason?, last_error?, artifact_path }
-- "code" and "history" are intentionally absent (Q3 = A).
function M.run(opts)
    assert(type(opts) == "table", "opts table required")
    assert(opts.target_file, "opts.target_file required")
    assert(opts.spec, "opts.spec required")
    assert(type(opts.runner) == "function", "opts.runner (function) required")

    -- Build conf with all K-96 fields explicitly listed.
    local conf = {
        runner = opts.runner,
        lang = opts.lang,
        max_iters = opts.max_iters,
        system = opts.system,
        on_iter = opts.on_iter,
        name = "compile_loop",
        llm = {
            provider = opts.provider,
            base_url = opts.base_url,
            api_key = opts.api_key,
            api_key_env = opts.api_key_env,
            model = opts.model,
            max_tokens = opts.max_tokens,
            temperature = opts.temperature,
            disable_thinking = opts.disable_thinking,
            timeout = opts.timeout,
        },
    }

    local td = cl.make(conf)

    -- handler expects the tool input shape: {spec, target_file, lang?}
    local raw_json = td.handler({
        spec = opts.spec,
        target_file = opts.target_file,
        lang = opts.lang,
    })

    local ok, result = pcall(std.json.decode, raw_json)
    if not ok or type(result) ~= "table" then
        return {
            ok = false,
            failure_reason = "decode_failed",
            last_error = tostring(result),
            iters = 0,
            summary = "coding_agent.run: failed to decode compile_loop result",
        }
    end
    return result
end

-- ============================================================
-- M.register_tool(opts) — thin facade (backward-compatible signature)
-- ============================================================
-- Returns the registered tool name ("compile_loop" or opts.name).
--
-- The registration happens HERE. `compile_loop.make` builds the tool_def and
-- stops there: a factory that also put its def in the global registry meant two
-- runs in one process collided on a name neither of them chose. This entry
-- point is the one whose whole job is to register, so it is the one that calls
-- `tool.register`.
function M.register_tool(opts)
    assert(type(opts) == "table", "opts table required")
    assert(opts.runner_kind ~= nil, "opts.runner_kind required")

    -- Resolve runner_kind → runner function (facade-local, Issue §確定 5).
    local runner, rerr = resolve_runner(opts.runner_kind)
    if not runner then
        error("coding_agent.register_tool: " .. tostring(rerr))
    end

    -- Build conf with all K-96 fields explicitly listed.
    local conf = {
        runner = runner,
        lang = opts.lang,
        max_iters = opts.max_iters,
        system = opts.system,
        name = opts.name,
        llm = {
            provider = opts.provider,
            base_url = opts.base_url,
            api_key = opts.api_key,
            api_key_env = opts.api_key_env,
            model = opts.model,
            max_tokens = opts.max_tokens,
            temperature = opts.temperature,
            disable_thinking = opts.disable_thinking,
            timeout = opts.timeout,
        },
    }

    local td = cl.make(conf)
    tool.register(td.name, td.schema, td.handler)
    return td.name
end

--- Expose facade-local helpers for testing (read-only access).
--- `register_tool` only returns a tool name, so the built-in runners are
--- otherwise unreachable from a test; the e2e fixture
--- `coding_agent_runner.lua` drives them through this accessor.
function M._test_helpers()
    return {
        resolve_runner = resolve_runner,
        builtin_runners = BUILTIN_RUNNERS,
        exec_result = exec_result,
    }
end

return M
