-- blocks/compile_loop/init.lua — Tool factory for the autonomous compile-and-fix loop.
--
-- Primary surface: compile_loop.make(conf) → tool_def
--
-- conf = {
--     runner    = function(path) → {ok, stdout, stderr, exit_code},  -- required
--     llm       = { provider, base_url, api_key, api_key_env, model,
--                   max_tokens, temperature, disable_thinking, timeout }, -- optional
--     max_iters = int?,    -- default 5
--     lang      = string?, -- default "lua"
--     name      = string?, -- default "compile_loop"
--     system    = string?,
-- }
--
-- target_file dual role: read on entry if already present (content embedded in
-- the initial user message), then written in full on each iteration.
-- Absent or empty → spec-only message (synthesis use case, backward-compatible).
--
-- Returns tool_def = { name = string, schema = table, handler = function }
-- Side-effect: tool.register(name, schema, handler) is called so the registry
--   and the returned tool_def share the same handler identity.
--
-- LLM resolution order (per field, at call time):
--   conf.llm.<field> → _AGENT_LLM_CTX top.<field> → nil (llm_call env fallback)
--
-- Counter WF-A defence: handler output JSON never contains "code" or "history".

local M = {}

local agent = require("agent") -- for _llm_ctx_top()

-- ============================================================
-- Internal constants
-- ============================================================

-- Stagnation detection window: give-up when the last N consecutive runner stderr
-- outputs are identical. Hard structural check, not a prompt heuristic.
local STAGNATION_WINDOW = 3

-- ============================================================
-- Observability helpers (inline mirror from blocks/agent/init.lua:90-181)
-- Gated by AGENT_BLOCK_LLM_DUMP env (off/meta/full).
-- ============================================================

local function env_true(name)
    local v = std.env.get(name)
    if not v then return false end
    v = string.lower(tostring(v))
    return v == "1" or v == "true" or v == "yes" or v == "on"
end

local function normalize_dump_mode(v)
    if not v or v == "" then return nil end
    v = string.lower(tostring(v))
    if v == "off" or v == "none" then return "off" end
    if v == "meta" then return "meta" end
    if v == "full" then return "full" end
    return "off"
end

local function resolve_dump_mode()
    local mode = normalize_dump_mode(std.env.get("AGENT_BLOCK_LLM_DUMP"))
    if not mode then
        local rust_log = string.lower(std.env.get_or("RUST_LOG", ""))
        if rust_log:find("trace", 1, true) or rust_log:find("debug", 1, true) then
            mode = "meta"
        else
            mode = "off"
        end
    end
    if mode == "full" then
        local env_name = string.lower(std.env.get_or("AGENT_BLOCK_ENV", ""))
        local is_prod = env_name == "prod" or env_name == "production"
        if is_prod and not env_true("AGENT_BLOCK_LLM_DUMP_ALLOW_PROD") then
            log.warn("compile_loop: AGENT_BLOCK_LLM_DUMP=full blocked in production env; downgraded to meta")
            mode = "meta"
        end
    end
    return mode
end

local LLM_DUMP_PREFIX = "ab.obs"

local function kv_escape(v)
    if v == nil then return "nil" end
    if type(v) == "boolean" or type(v) == "number" then
        return tostring(v)
    end
    local s = tostring(v)
    if s == "" then return '""' end
    if s:find("[%s=]") then
        return std.json.encode(s)
    end
    return s
end

local function format_kv(parts)
    local out = {}
    for i, pair in ipairs(parts) do
        out[i] = tostring(pair[1]) .. "=" .. kv_escape(pair[2])
    end
    return table.concat(out, " ")
end

local function obs_event(mode, event_name, fields)
    if mode == "off" then return end
    local entries = {
        { "prefix",    LLM_DUMP_PREFIX },
        { "event",     event_name },
        { "component", "compile_loop" },
    }
    for _, f in ipairs(fields or {}) do
        table.insert(entries, f)
    end
    log.info(format_kv(entries))
end

local DEFAULT_SYSTEM = [[You are an expert programmer.
You will be given a spec and asked to write code that runs and passes its self-checks.
Output ONLY the complete file contents in a single fenced code block (e.g. ```lua\n...\n```).
No prose before or after the block.
On retry, output the WHOLE corrected file (not a diff). Keep changes minimal.]]

-- ============================================================
-- Internal helpers (moved from coding_agent/init.lua)
-- ============================================================

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
--
-- opts fields (K-96 full set):
--   provider, base_url, api_key, api_key_env, model,
--   max_tokens, temperature, disable_thinking, timeout
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
        local base_url = opts.base_url or "https://api.anthropic.com"
        local resp = http.request(base_url .. "/v1/messages", {
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

        -- 10. Normalize to OpenAI-compatible shape so run_loop requires zero changes
        return { choices = { { message = { content = joined } } } }

    elseif provider ~= "openai" then
        return nil, "provider " .. provider .. " not yet supported in compile_loop"
    end

    -- OpenAI-compatible path
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

-- Read target file if it already exists and is non-empty.
-- Returns file content as a string, or nil when the file is absent, empty, or unreadable.
-- Uses to_abs so that relative paths are resolved before io.open.
local function read_target_if_exists(path)
    local abs_path = to_abs(path)
    local f, _ = io.open(abs_path, "r")
    if not f then return nil end
    local content = f:read("*a")
    f:close()
    if not content or content == "" then return nil end
    return content
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

-- Filter run_loop result for tool output: remove code and history to prevent
-- caller context contamination (Counter WF-A defence).
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

-- ============================================================
-- Internal loop body (non-public; called only via make().handler)
-- ============================================================

-- run_loop(conf, messages_override?) executes the structural compile-and-fix loop.
-- conf fields (K-96 full set, all resolved before entry):
--   runner, lang, target_file, spec, max_iters, system,
--   provider, base_url, api_key, api_key_env, model,
--   max_tokens, temperature, disable_thinking, timeout,
--   on_iter (optional callback)
local function run_loop(conf)
    assert(type(conf) == "table", "conf table required")
    assert(conf.target_file, "conf.target_file required")
    assert(conf.spec,        "conf.spec required")
    assert(type(conf.runner) == "function", "conf.runner (function) required")

    local lang          = conf.lang or "lua"
    local max_iters     = conf.max_iters or 5
    local system        = conf.system or DEFAULT_SYSTEM
    local artifact_path = to_abs(conf.target_file)
    local mode          = resolve_dump_mode()

    -- Build the initial user message.
    -- When target_file already exists (build-resolver / minimal-edit use case),
    -- embed its content so the child LLM has the current file as context.
    -- Falls back to spec-only when the file is absent or empty (synthesis use case).
    -- No tool arrays, no extra_tools, no JSON schema.
    local existing = read_target_if_exists(conf.target_file)
    local user_content = conf.spec
    if existing then
        user_content = conf.spec
            .. "\n\n=== Current file content ===\n```" .. lang .. "\n"
            .. existing
            .. "\n```"
    end

    local messages = {
        { role = "system", content = system },
        { role = "user",   content = user_content },
    }

    local history = {}

    for iter = 1, max_iters do
        obs_event(mode, "iter_start", { { "iter", iter }, { "target_file", artifact_path } })
        local resp, err = llm_call(conf, messages)
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
        local f, werr = io.open(conf.target_file, "w")
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
        local rr = conf.runner(conf.target_file) or {}
        local entry = { iter = iter, code = code, result = rr, raw = content }
        table.insert(history, entry)
        obs_event(mode, "iter_result", {
            { "iter",       iter },
            { "ok",         rr.ok and true or false },
            { "exit_code",  rr.exit_code },
            { "stderr_len", #(tostring(rr.stderr or "")) },
        })

        if conf.on_iter then
            local cb_ok, cb_err = pcall(conf.on_iter, entry)
            if not cb_ok then
                log.warn("compile_loop: on_iter callback error: " .. tostring(cb_err))
            end
        end

        if rr.ok then
            obs_event(mode, "converged", { { "iters", iter } })
            return {
                ok            = true,
                code          = code,
                artifact_path = artifact_path,
                iters         = iter,
                summary       = make_summary(true, iter, max_iters, nil),
                history       = history,
            }
        end

        -- Stagnation detection
        if is_stagnant(history) then
            local last_stderr = tostring((rr.stderr) or ""):sub(-800)
            obs_event(mode, "stagnation", { { "iters", iter } })
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
        table.insert(messages, { role = "assistant", content = content })
        table.insert(messages, { role = "user",      content = build_failure_msg(lang, rr) })
    end

    -- max_iters reached without PASS
    local last = history[#history] or {}
    local last_stderr = tostring(((last.result) or {}).stderr or ""):sub(-800)
    obs_event(mode, "max_iters_reached", { { "iters", max_iters } })
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

-- ============================================================
-- Public API
-- ============================================================

--- compile_loop.make(conf) → tool_def
---
--- Factory function. Returns a tool_def = {name, schema, handler} that can be
--- passed directly to agent.run({extra_tools = {tool_def}}).
---
--- Side-effect: tool.register(name, schema, handler) is called so the tool
--- registry and tool_def.handler are identity-equal.
---
--- LLM resolution (at handler call time, i.e. when the parent agent invokes the tool):
---   conf.llm.<field> → _AGENT_LLM_CTX top.<field> → nil → llm_call env fallback
---
--- conf.runner is required and must be a function. Providing conf.llm is optional;
--- omitting it causes the parent agent's provider/model/api_key to be inherited.
function M.make(conf)
    assert(type(conf) == "table", "conf table required")
    assert(type(conf.runner) == "function", "conf.runner function required")

    local name = conf.name or "compile_loop"

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
                    description = "Absolute path of the file. Read on entry if it already exists, then written on each iteration.",
                },
                lang = {
                    type        = "string",
                    description = "Code fence language label (default: lua).",
                },
            },
        },
    }

    local function handler(input)
        -- Resolve LLM fields at call time (Crux #2).
        -- Priority: conf.llm.<field> → _AGENT_LLM_CTX top → nil (env fallback in llm_call)
        local parent_ctx = agent._llm_ctx_top() or {}
        local llm_conf   = conf.llm or {}

        local resolved_conf = {
            -- runner (from factory conf, never from input)
            runner   = conf.runner,

            -- tool input fields
            lang        = input.lang or conf.lang or "lua",
            target_file = input.target_file,
            spec        = input.spec,

            -- factory conf fields
            max_iters = conf.max_iters,
            system    = conf.system,
            on_iter   = conf.on_iter,

            -- LLM fields (K-96 full set, all explicit):
            provider         = llm_conf.provider         or parent_ctx.provider,
            base_url         = llm_conf.base_url         or parent_ctx.base_url,
            api_key          = llm_conf.api_key          or parent_ctx.api_key,
            api_key_env      = llm_conf.api_key_env      or parent_ctx.api_key_env,
            model            = llm_conf.model            or parent_ctx.model,
            max_tokens       = llm_conf.max_tokens,
            temperature      = llm_conf.temperature,
            disable_thinking = llm_conf.disable_thinking,
            timeout          = llm_conf.timeout,
        }

        local res      = run_loop(resolved_conf)
        local filtered = filter_for_tool_output(res)
        local enc_ok, enc_str = pcall(std.json.encode, filtered)
        if enc_ok then
            return enc_str
        end
        return '{"ok":false,"failure_reason":"encode_failed","iters":0,"summary":"json encode failed"}'
    end

    tool.register(name, schema, handler)
    return { name = name, schema = schema, handler = handler }
end

return M
