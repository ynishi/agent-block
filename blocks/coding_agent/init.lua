-- blocks/coding_agent/init.lua — Thin backward-compatible facade over compile_loop.
--
-- Primary surface is now compile_loop.make(conf) in blocks/compile_loop/init.lua.
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
-- ============================================================

local BUILTIN_RUNNERS = {
    -- "lua" runner: invoke lua interpreter, pass/fail by exit 0 + "ALL_PASS" in stdout
    lua = function(file_path)
        local p = io.popen("lua " .. file_path .. ' 2>&1; echo "__EXIT__=$?"', "r")
        if not p then
            return { ok = false, stdout = "", stderr = "popen failed", exit_code = -1 }
        end
        local out = p:read("*a") or ""
        p:close()
        local exit_str = out:match("__EXIT__=(%d+)%s*$") or "1"
        local exit_code = tonumber(exit_str) or 1
        out = out:gsub("__EXIT__=%d+%s*$", "")
        local pass = exit_code == 0 and out:find("ALL_PASS", 1, true) ~= nil
        return { ok = pass, stdout = out, stderr = "", exit_code = exit_code }
    end,
    -- "cargo" runner: cd to target dir, run cargo test --offline, pass on "test result: ok"
    cargo = function(file_path)
        local dir = file_path:match("^(.*)/[^/]+$") or "."
        local cmd = "cd " .. dir .. " && cargo test --offline 2>&1"
        local p = io.popen(cmd, "r")
        if not p then
            return { ok = false, stdout = "", stderr = "popen failed", exit_code = -1 }
        end
        local out = p:read("*a") or ""
        p:close()
        local pass = out:find("test result: ok", 1, true) ~= nil
        return { ok = pass, stdout = out, stderr = "", exit_code = pass and 0 or 1 }
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
    return td.name
end

return M
