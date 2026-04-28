-- blocks/coding_agent/init.lua — Structured coding-react loop (full-file edit mode)
--
-- Different from blocks/agent (free ReAct). Here the loop is STRUCTURAL:
--   LLM emits code → host auto-edits target_file → host auto-runs runner →
--   on FAIL feed result back, on PASS exit. No tool_use indirection.
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
--   -- res = { ok, code, iters, history, error? }

local M = {}

local DEFAULT_SYSTEM = [[You are an expert programmer.
You will be given a spec and asked to write code that runs and passes its self-checks.
Output ONLY the complete file contents in a single fenced code block (e.g. ```lua\n...\n```).
No prose before or after the block.
On retry, output the WHOLE corrected file (not a diff). Keep changes minimal.]]

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
    if provider ~= "openai" then
        -- TODO: anthropic path. For now structural loop targets openai-compat
        -- (vLLM / OpenAI / llama-cpp-python with proper parser).
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

    local lang      = opts.lang or "lua"
    local max_iters = opts.max_iters or 5
    local system    = opts.system or DEFAULT_SYSTEM

    local messages = {
        { role = "system", content = system },
        { role = "user",   content = opts.spec },
    }

    local history = {}

    for iter = 1, max_iters do
        local resp, err = llm_call(opts, messages)
        if not resp then
            return {
                ok = false,
                error = "llm_call: " .. tostring(err),
                iters = iter - 1,
                history = history,
            }
        end

        local choice  = (resp.choices or {})[1] or {}
        local content = (choice.message or {}).content or ""
        local code    = extract_code(content, lang)

        -- Write target file (full-file replace)
        local f, werr = io.open(opts.target_file, "w")
        if not f then
            return {
                ok = false,
                error = "open target_file: " .. tostring(werr),
                iters = iter,
                history = history,
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
                ok      = true,
                code    = code,
                iters   = iter,
                history = history,
            }
        end

        -- Append assistant + failure user message for the next turn
        table.insert(messages, { role = "assistant", content = content })
        table.insert(messages, { role = "user",      content = build_failure_msg(lang, rr) })
    end

    local last = history[#history] or {}
    return {
        ok      = false,
        error   = "max_iters reached without PASS",
        iters   = max_iters,
        code    = last.code,
        history = history,
    }
end

return M
