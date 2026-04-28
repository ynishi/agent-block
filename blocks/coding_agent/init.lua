-- blocks/coding_agent/init.lua — Structured compile_loop (full-file edit mode)
--
-- Different from blocks/agent (free ReAct). Here the loop is STRUCTURAL:
--   LLM emits code → host auto-edits target_file → host auto-runs runner →
--   on FAIL feed result back, on PASS exit. No tool_use indirection.
--
-- Child LLM action space is confined to next_full_file ONLY.
-- The child never receives tool arrays, tool schemas, or JSON dispatch.
-- Target file switching and spec modification are structurally unreachable.
--
-- Usage:
--   local coding = require("coding_agent")
--   local res = coding.run({
--       provider     = "openai" | "anthropic",
--       base_url     = "...",
--       api_key      = "...",
--       model        = "...",
--       target_file  = "/tmp/work/script.lua",
--       spec         = "Write a Lua function that ...",
--       lang         = "lua",          -- code fence label
--       runner       = function(file_path) return { ok=bool, stdout, stderr, exit_code } end,
--       max_iters    = 5,
--       on_iter      = function(info) end,  -- info = { iter, code, result }
--       disable_thinking = true,        -- Qwen-specific, no-op for others
--   })
--   -- res = { ok, code, artifact_path, iters, summary, history, failure_reason?, last_error? }

local M = {}

-- Stagnation detection window: give-up when the last N consecutive runner stderr
-- outputs are identical. This is a hard structural check, not a prompt heuristic.
local STAGNATION_WINDOW = 3

local DEFAULT_SYSTEM = [[You are an expert programmer.
You will be given a spec and asked to write code that runs and passes its self-checks.
Output ONLY the complete file contents in a single fenced code block (e.g. ```lua\n...\n```).
No prose before or after the block.
On retry, output the WHOLE corrected file (not a diff). Keep changes minimal.]]

-- Resolve path to absolute. If already absolute, return as-is.
local function to_abs(path)
    if path:sub(1, 1) == "/" then
        return path
    end
    return (os.getenv("PWD") or ".") .. "/" .. path
end

-- Build a human-readable summary string for all exit paths.
local function make_summary(ok, iters, max_iters, reason)
    if ok then
        return string.format("PASS in %d iters", iters)
    end
    if reason == "stagnation" then
        return string.format(
            "give-up: stagnation at iter %d/%d (stderr identical %dx)",
            iters, max_iters, STAGNATION_WINDOW
        )
    elseif reason == "max_iters" then
        return string.format("give-up: max_iters reached (%d)", max_iters)
    elseif reason == "llm_call" then
        return string.format("give-up: llm_call failed at iter %d/%d", iters, max_iters)
    elseif reason == "open_target_file" then
        return string.format("give-up: open_target_file failed at iter %d/%d", iters, max_iters)
    else
        return string.format("give-up: %s", tostring(reason))
    end
end

-- Stagnation detection: check if the last STAGNATION_WINDOW entries in history
-- all have identical runner stderr. Independent of iter count.
local function is_stagnant(history)
    if #history < STAGNATION_WINDOW then
        return false
    end
    local ref = ((history[#history].result) or {}).stderr or ""
    for i = #history - STAGNATION_WINDOW + 1, #history do
        if (((history[i].result) or {}).stderr or "") ~= ref then
            return false
        end
    end
    return true
end

-- Extract the FIRST fenced code block matching the lang label, falling back to any fence.
local function extract_code(text, lang)
    lang = lang or "lua"
    -- Try language-specific fence first
    local m = text:match("```" .. lang .. "%s*\n(.-)\n```")
    if m then return m end
    -- Fallback: any fence
    m = text:match("```%w*%s*\n(.-)\n```")
    if m then return m end
    -- Last resort: raw text (LLM forgot fences)
    return text
end

-- Minimal OpenAI-compatible chat call. Mirrors agent/init.lua llm_call_openai
-- but stripped of tool dispatch / cache_control / context_management since the
-- coding loop never uses tools.
local function llm_call(opts, messages)
    local provider = opts.provider or "openai"
    if provider == "anthropic" then
        -- 1. Resolve api_key: opts.api_key → ANTHROPIC_API_KEY env → error
        local api_key = opts.api_key
        if not api_key or api_key == "" then
            api_key = std.env.get(opts.api_key_env or "ANTHROPIC_API_KEY")
        end
        if not api_key or api_key == "" then
            return nil, "no api_key (opts.api_key or ANTHROPIC_API_KEY env)"
        end

        -- 2. Model
        local model = opts.model or std.env.get_or("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")

        -- 3. Extract system role from messages → body.system
        --    M.run inserts {role="system"} as messages[1]; Anthropic requires top-level system field.
        local sys_text = nil
        local body_messages = {}
        for _, msg in ipairs(messages) do
            if msg.role == "system" and sys_text == nil then
                sys_text = msg.content
            else
                table.insert(body_messages, msg)
            end
        end

        -- 4. Build body
        local body = {
            model      = model,
            max_tokens = opts.max_tokens or 4096,
            messages   = body_messages,
        }
        if sys_text then
            body.system = sys_text
        end
        -- disable_thinking is Qwen-specific; silent no-op for anthropic

        -- 5. Headers
        local headers = {
            ["x-api-key"]         = api_key,
            ["anthropic-version"] = "2023-06-01",
            ["content-type"]      = "application/json",
        }

        -- 6. HTTP call
        local resp = http.request("https://api.anthropic.com/v1/messages", {
            method  = "POST",
            headers = headers,
            body    = std.json.encode(body),
            timeout = opts.timeout or 120,
        })

        -- 7. Status check
        if resp.status ~= 200 then
            return nil, "API error " .. tostring(resp.status) .. " body=" .. tostring(resp.body or "")
        end

        -- 8. pcall decode
        local ok, decoded = pcall(std.json.decode, resp.body)
        if not ok or type(decoded) ~= "table" then
            return nil, "decode failed: " .. tostring(decoded)
        end

        -- 9. Extract text content blocks
        if type(decoded.content) ~= "table" or #decoded.content == 0 then
            return nil, "anthropic response missing content blocks"
        end
        local text_parts = {}
        for _, block in ipairs(decoded.content) do
            if block.type == "text" then
                table.insert(text_parts, block.text or "")
            end
        end
        local joined = table.concat(text_parts, "\n")
        if joined == "" then
            return nil, "anthropic response missing content blocks"
        end

        -- 10. Normalize to OpenAI-compatible shape so M.run (L199-200) requires zero changes
        return { choices = { { message = { content = joined } } } }

    elseif provider ~= "openai" then
        return nil, "provider " .. provider .. " not yet supported in coding_agent"
    end

    local api_key = opts.api_key
    if not api_key or api_key == "" then
        api_key = std.env.get(opts.api_key_env or "OPENAI_API_KEY")
    end
    if not api_key or api_key == "" then
        return nil, "no api_key (opts.api_key or OPENAI_API_KEY env)"
    end

    local base_url = opts.base_url or "https://api.openai.com/v1"
    local body = {
        model       = opts.model or "gpt-4o-mini",
        max_tokens  = opts.max_tokens or 4096,
        temperature = opts.temperature or 0.2,
        messages    = messages,
    }
    if opts.disable_thinking then
        body.chat_template_kwargs = { enable_thinking = false }
    end

    local headers = {
        ["Content-Type"]  = "application/json",
        ["Authorization"] = "Bearer " .. api_key,
        ["User-Agent"]    = "Mozilla/5.0", -- RunPod proxy / Cloudflare gate
    }

    local resp = http.request(base_url .. "/chat/completions", {
        method  = "POST",
        headers = headers,
        body    = std.json.encode(body),
        timeout = opts.timeout or 120,
    })
    if resp.status ~= 200 then
        return nil, "API error " .. tostring(resp.status) .. " body=" .. tostring(resp.body or "")
    end
    local ok, decoded = pcall(std.json.decode, resp.body)
    if not ok or type(decoded) ~= "table" then
        return nil, "decode failed: " .. tostring(decoded)
    end
    return decoded
end

-- Build the failure-feedback user message.
-- NOTE: This message contains ONLY spec and build feedback — no tool names,
-- no JSON schema, no tool_use vocabulary. Child LLM action space is confined
-- to emitting a corrected file in a single fenced code block.
local function build_failure_msg(lang, rr)
    return string.format(
        "Run FAILED. Fix the code and re-output the WHOLE corrected file in a single ```%s ... ``` block.\n\n=== stdout ===\n%s\n\n=== stderr ===\n%s\n\n=== exit_code ===\n%s",
        lang,
        tostring(rr.stdout or ""),
        tostring(rr.stderr or ""),
        tostring(rr.exit_code or "unknown")
    )
end

function M.run(opts)
    assert(type(opts) == "table", "opts table required")
    assert(opts.target_file, "opts.target_file required")
    assert(opts.spec,        "opts.spec required")
    assert(type(opts.runner) == "function", "opts.runner (function) required")

    local lang         = opts.lang or "lua"
    local max_iters    = opts.max_iters or 5
    local system       = opts.system or DEFAULT_SYSTEM
    local artifact_path = to_abs(opts.target_file)

    -- Child LLM messages list: system + user(spec) only.
    -- No tool arrays, no extra_tools, no JSON schema.
    -- Target file and spec are fixed by the parent; the child cannot modify them.
    local messages = {
        { role = "system", content = system },
        { role = "user",   content = opts.spec },
    }

    local history = {}

    for iter = 1, max_iters do
        local resp, err = llm_call(opts, messages)
        if not resp then
            local err_str = tostring(err)
            return {
                ok             = false,
                failure_reason = "llm_call",
                last_error     = err_str:sub(-800),
                iters          = iter - 1,
                summary        = make_summary(false, iter - 1, max_iters, "llm_call"),
                artifact_path  = artifact_path,
                history        = history,
            }
        end

        local choice  = (resp.choices or {})[1] or {}
        local content = (choice.message or {}).content or ""
        local code    = extract_code(content, lang)

        -- Write target file (full-file replace — next_full_file action)
        local f, werr = io.open(opts.target_file, "w")
        if not f then
            local werr_str = tostring(werr)
            return {
                ok             = false,
                failure_reason = "open_target_file",
                last_error     = werr_str,
                iters          = iter,
                summary        = make_summary(false, iter, max_iters, "open_target_file"),
                artifact_path  = artifact_path,
                history        = history,
            }
        end
        f:write(code)
        f:close()

        -- Run
        local rr = opts.runner(opts.target_file) or {}
        local entry = { iter = iter, code = code, result = rr, raw = content }
        table.insert(history, entry)

        if opts.on_iter then
            local cb_ok, cb_err = pcall(opts.on_iter, entry)
            if not cb_ok then
                log.warn("coding_agent: on_iter callback error: " .. tostring(cb_err))
            end
        end

        if rr.ok then
            return {
                ok            = true,
                code          = code,
                artifact_path = artifact_path,
                iters         = iter,
                summary       = make_summary(true, iter, max_iters, nil),
                history       = history,
            }
        end

        -- Stagnation detection: if the last STAGNATION_WINDOW iterations all
        -- produced identical runner stderr, give up — this is independent of
        -- max_iters and detects infinite retry without progress.
        if is_stagnant(history) then
            local last_stderr = tostring((rr.stderr) or ""):sub(-800)
            return {
                ok             = false,
                failure_reason = "stagnation",
                last_error     = last_stderr,
                code           = code,
                iters          = iter,
                summary        = make_summary(false, iter, max_iters, "stagnation"),
                artifact_path  = artifact_path,
                history        = history,
            }
        end

        -- Append assistant + failure user message for the next turn.
        -- Only spec feedback is provided — no tool routing, no JSON schema injection.
        table.insert(messages, { role = "assistant", content = content })
        table.insert(messages, { role = "user",      content = build_failure_msg(lang, rr) })
    end

    -- max_iters reached without PASS
    local last = history[#history] or {}
    local last_stderr = tostring(((last.result) or {}).stderr or ""):sub(-800)
    return {
        ok             = false,
        failure_reason = "max_iters",
        last_error     = last_stderr,
        code           = last.code,
        iters          = max_iters,
        summary        = make_summary(false, max_iters, max_iters, "max_iters"),
        artifact_path  = artifact_path,
        history        = history,
    }
end

-- Built-in runner factories for runner_kind string dispatch.
-- These are resolved in M.register_tool before calling M.run,
-- so M.run's runner assertion remains satisfied.
local BUILTIN_RUNNERS = {
    -- "lua" runner: invoke lua interpreter, pass/fail by exit 0 + "ALL_PASS" in stdout
    lua = function(file_path)
        local p = io.popen("lua " .. file_path .. ' 2>&1; echo "__EXIT__=$?"', "r")
        if not p then
            return { ok = false, stdout = "", stderr = "popen failed", exit_code = -1 }
        end
        local out = p:read("*a") or ""
        p:close()
        local exit_str  = out:match("__EXIT__=(%d+)%s*$") or "1"
        local exit_code = tonumber(exit_str) or 1
        out = out:gsub("__EXIT__=%d+%s*$", "")
        local pass = exit_code == 0 and out:find("ALL_PASS", 1, true) ~= nil
        return { ok = pass, stdout = out, stderr = "", exit_code = exit_code }
    end,
    -- "cargo" runner: cd to target dir, run cargo test --offline, pass on "test result: ok"
    cargo = function(file_path)
        local dir = file_path:match("^(.*)/[^/]+$") or "."
        local cmd = "cd " .. dir .. " && cargo test --offline 2>&1"
        local p   = io.popen(cmd, "r")
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

-- Filter M.run result for tool output: remove code and history to prevent
-- Caller context contamination (Counter WF-A defence).
local function filter_for_tool_output(res)
    return {
        ok             = res.ok,
        artifact_path  = res.artifact_path,
        iters          = res.iters,
        summary        = res.summary,
        failure_reason = res.failure_reason,
        last_error     = res.last_error,
        -- code:    excluded (Counter WF-A defence)
        -- history: excluded (circular-ref risk + context contamination)
    }
end

-- M.register_tool(opts) — register the "compile_loop" tool with the host tool registry.
--
-- opts fields (all fixed at registration time; tool input may override spec/target_file/lang):
--   provider     string   "openai" | "anthropic"
--   base_url     string
--   api_key      string
--   model        string
--   runner_kind  string|function  "lua" | "cargo" | runner function
--   max_iters    int?    (default 5)
--   lang         string? (default "lua")
--
-- The handler merges opts with tool input; input fields override opts fields.
-- Returns the registered tool name ("compile_loop").
function M.register_tool(opts)
    assert(type(opts) == "table", "opts table required")
    assert(opts.runner_kind ~= nil, "opts.runner_kind required")

    local schema = {
        description = [[Run an autonomous compile-and-fix loop: a child LLM emits the
complete target file on every iteration, the runner executes it, and on
failure the stderr is fed back until the run passes or the give-up gate
triggers. Returns ok/iters/summary and, on failure, failure_reason/last_error.]],
        input_schema = {
            type     = "object",
            required = { "spec", "target_file" },
            properties = {
                spec = {
                    type        = "string",
                    description = "Full specification the child LLM must satisfy.",
                },
                target_file = {
                    type        = "string",
                    description = "Absolute path of the file the child LLM writes on each iteration.",
                },
                lang = {
                    type        = "string",
                    description = "Code fence language label (default: lua).",
                },
            },
        },
    }

    local function handler(input)
        -- Merge opts and tool input; input overrides opts for spec/target_file/lang.
        local merged = {
            provider    = opts.provider,
            base_url    = opts.base_url,
            api_key     = opts.api_key,
            model       = opts.model,
            max_iters   = opts.max_iters,
            lang        = input.lang or opts.lang or "lua",
            target_file = input.target_file,
            spec        = input.spec,
        }

        -- Resolve runner_kind → runner function.
        local runner, rerr = resolve_runner(opts.runner_kind)
        if not runner then
            local payload = {
                ok             = false,
                failure_reason = "runner_dispatch_failed",
                last_error     = tostring(rerr),
                iters          = 0,
                summary        = "runner_kind resolution failed: " .. tostring(rerr),
            }
            local enc_ok, enc_str = pcall(std.json.encode, payload)
            if enc_ok then return enc_str end
            return '{"ok":false,"failure_reason":"encode_failed","summary":"json encode failed","iters":0}'
        end
        merged.runner = runner

        -- Run the loop.
        local res = M.run(merged)

        -- Filter and encode; handler MUST return a string.
        local filtered = filter_for_tool_output(res)
        local enc_ok, enc_str = pcall(std.json.encode, filtered)
        if enc_ok then
            return enc_str
        end
        return '{"ok":false,"failure_reason":"encode_failed","summary":"json encode failed","iters":0}'
    end

    tool.register("compile_loop", schema, handler)
    return "compile_loop"
end

return M
