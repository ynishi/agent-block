-- blocks/compile_loop/init.lua — Tool factory for the autonomous compile-and-fix loop.
--
-- Primary surface: compile_loop.make(conf) → tool_def
--
-- conf = {
--     runner    = function(path) → {ok, stdout, stderr, exit_code},  -- required (single-file)
--               | function(paths) → {ok, stdout, stderr, exit_code}, -- required (multi-file)
--     llm       = { provider, base_url, api_key, api_key_env, model,
--                   max_tokens, temperature, disable_thinking, timeout }, -- optional
--     max_iters = int?,    -- default 5
--     lang      = string?, -- default "lua"
--     name      = string?, -- default "compile_loop"
--     system    = string?,
--     edit_mode = "full"|"diff"?, -- default "full"; "diff" uses SEARCH/REPLACE patches
-- }
--
-- target_file (string) XOR target_files (list<string>): mutually exclusive.
-- target_file dual role: read on entry if already present (content embedded in
-- the initial user message), then written in full on each iteration.
-- Absent or empty → spec-only message (synthesis use case, backward-compatible).
-- target_files: multi-file mode, requires edit_mode="diff".
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

local DIFF_SYSTEM = [[You are an expert programmer editing an existing file.
Output only SEARCH/REPLACE blocks in this exact format:

<<<<<<< SEARCH
<existing text to replace, character-exact>
=======
<replacement text>
>>>>>>> REPLACE

- Multiple blocks allowed.
- SEARCH text must match the file character-exactly (whitespace included).
- Do NOT output the full file. Do NOT use code fences.
- Make the SMALLEST changes that satisfy the spec.]]

-- System prompt for multi-file diff mode.
-- Each group of SEARCH/REPLACE blocks must be preceded by a path header line:
--   <<< path=<relative/or/absolute/path> >>>
-- All SEARCH/REPLACE blocks that follow a path header apply to that file until the
-- next path header appears. The path must exactly match one of the provided target files.
local DIFF_SYSTEM_MULTI = [[You are an expert programmer editing multiple existing files simultaneously.
Output SEARCH/REPLACE blocks grouped by file. Each group must start with a path header:

<<< path=<file_path> >>>
<<<<<<< SEARCH
<existing text to replace, character-exact>
=======
<replacement text>
>>>>>>> REPLACE

Rules:
- Every SEARCH/REPLACE block MUST be preceded by a <<< path=... >>> header.
- The path must exactly match one of the target files provided.
- Multiple SEARCH/REPLACE blocks for the same file: repeat the path header before each block, or place all blocks consecutively under one header.
- SEARCH text must match the file character-exactly (whitespace included).
- Do NOT output full file contents. Do NOT use code fences.
- Make the SMALLEST changes that satisfy the spec.]]

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

-- FNV-1a 32-bit hash (inline fallback; no external dependency required).
-- Returns a decimal string representation of the 32-bit hash value.
local function fnv1a_hash(s)
    s = s or ""
    local hash = 2166136261  -- FNV offset basis (32-bit)
    for i = 1, #s do
        local byte = string.byte(s, i)
        -- XOR with byte then multiply by FNV prime (16777619), truncated to 32-bit.
        hash = (hash ~ byte) * 16777619
        -- Keep only lower 32 bits to prevent integer overflow accumulation.
        hash = hash & 0xFFFFFFFF
    end
    return tostring(hash)
end

-- Compute a stable hash for an SR block text (path header + SEARCH/REPLACE content).
-- Normalises whitespace before hashing to avoid collisions due to trivial formatting differences.
local function compute_sr_hash(sr_text)
    local text = tostring(sr_text or "")
    -- Normalise: collapse all whitespace runs to single space, strip leading/trailing.
    text = text:gsub("%s+", " "):gsub("^%s+", ""):gsub("%s+$", "")
    return fnv1a_hash(text)
end

-- Stagnation detection for multi-file branch (independent of messages[] reset).
-- Uses state.sr_history (list of sr_hash strings) rather than history[].result.stderr.
--
-- Conditions (all must hold):
--   (1) #state.sr_history >= STAGNATION_WINDOW (= 3)
--   (2) Among the last STAGNATION_WINDOW entries, >= 2 share the same sr_hash
--   (3) The most recent verify outcome is failure (caller passes last_verify_failed = true)
--
-- Returns: boolean
local function is_stagnant_v2(state, last_verify_failed)
    assert(type(state) == "table", "state required")
    assert(type(state.sr_history) == "table", "state.sr_history must be initialized as table")

    if #state.sr_history < STAGNATION_WINDOW then return false end
    if not last_verify_failed then return false end

    -- Collect the last STAGNATION_WINDOW entries.
    local recent = {}
    for i = #state.sr_history - STAGNATION_WINDOW + 1, #state.sr_history do
        recent[#recent + 1] = state.sr_history[i]
    end

    -- Count occurrences of each hash within the recent window.
    local counts = {}
    for _, h in ipairs(recent) do
        counts[h] = (counts[h] or 0) + 1
    end
    for _, c in pairs(counts) do
        if c >= 2 then return true end
    end
    return false
end

-- Update mf_state fields with optional trim policies (single write point — DRY).
--   opts.last_err:         trim to <= 2000 chars (tail)
--   opts.sr_digest_prev:   trim to <= 500 chars (head)
--   opts.sr_hash_append:   append to sr_history
--   opts.iter:             set state.iter
local function update_state(state, opts)
    if opts.last_err ~= nil then
        local s = tostring(opts.last_err)
        state.last_err = s:sub(-2000)
    end
    if opts.sr_digest_prev ~= nil then
        local s = tostring(opts.sr_digest_prev)
        state.sr_digest_prev = s:sub(1, 500)
    end
    if opts.sr_hash_append ~= nil then
        table.insert(state.sr_history, opts.sr_hash_append)
    end
    if opts.iter ~= nil then
        state.iter = opts.iter
    end
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
-- but extended for tool_use (multi-file lazy-load path).
--
-- opts fields (K-96 full set):
--   provider, base_url, api_key, api_key_env, model,
--   max_tokens, temperature, disable_thinking, timeout,
--   tools (optional: list of tool spec tables for anthropic tool_use)
--
-- Return shape:
--   success (text-only): { choices = { { message = { content = joined_text } } } }
--   success (tool_use):  { choices = { { message = {
--                            content        = joined_text,   -- may be ""
--                            tool_use_blocks = { {id, name, input}, ... },
--                            stop_reason    = "tool_use"|"end_turn"|"max_tokens",
--                          } } } }
--   failure: nil, error_string

-- ============================================================
-- Internal: OpenAI tool-use helpers (写経 from blocks/agent/init.lua, do NOT shared-extract)
-- ============================================================

--- Map OpenAI finish_reason to internal stop_reason string.
--- @param finish_reason string|nil
--- @return string
local function cl_oai_map_finish_reason(finish_reason)
    if finish_reason == "stop" then
        return "end_turn"
    elseif finish_reason == "tool_calls" then
        return "tool_use"
    elseif finish_reason == "length" then
        return "max_tokens"
    else
        return tostring(finish_reason or "end_turn")
    end
end

--- Normalize a raw OpenAI chat completion response into compile_loop internal shape.
--- Internal shape (tools path):
---   { choices = { { message = { content = joined_text,
---                               tool_use_blocks = [{id, name, input}],
---                               stop_reason = string } } } }
--- @param raw table  Parsed OpenAI JSON response
--- @return table|nil  compile_loop-shape table on success
--- @return string|nil Error string on failure
local function cl_oai_normalize(raw)
    if not raw or not raw.choices or #raw.choices == 0 then
        return nil, "invalid OpenAI response: missing choices"
    end
    local choice  = raw.choices[1]
    local message = choice and choice.message
    if not message then
        return nil, "invalid OpenAI response: missing choices[0].message"
    end

    local text_parts      = {}
    local tool_use_blocks = {}

    -- Text portion (may be nil/empty on pure tool_calls turns).
    local text = message.content
    if text and text ~= "" then
        table.insert(text_parts, text)
    end

    -- tool_calls → tool_use_blocks.
    for _, tc in ipairs(message.tool_calls or {}) do
        local fn    = tc["function"] or {}
        local input = {}
        local ok, parsed = pcall(std.json.decode, fn.arguments or "{}")
        if ok and type(parsed) == "table" then
            input = parsed
        else
            log.warn("compile_loop: OpenAI tool_call arguments JSON parse failed for tool '"
                .. tostring(fn.name) .. "'; using empty input")
            -- Acceptance Criteria #7 equivalent: input={}, is_error_hint, loop continues.
            table.insert(tool_use_blocks, {
                id            = tc.id,
                name          = fn.name or "",
                input         = {},
                is_error_hint = "arguments_parse_failed",
            })
            goto continue_tc
        end
        table.insert(tool_use_blocks, {
            id    = tc.id,
            name  = fn.name or "",
            input = input,
        })
        ::continue_tc::
    end

    local joined = table.concat(text_parts, "\n")
    return {
        choices = {
            {
                message = {
                    content         = joined,
                    tool_use_blocks = tool_use_blocks,
                    stop_reason     = cl_oai_map_finish_reason(choice.finish_reason),
                },
            },
        },
    }, nil
end

--- Convert compile_loop Anthropic-shaped messages to OpenAI-shaped messages.
--- Handles:
---   assistant messages with tool_use blocks → assistant + tool_calls array
---   user messages with tool_result blocks  → role="tool" + tool_call_id messages
---   string content messages                → pass-through
--- @param messages table    Anthropic-shaped messages array (role="system" already removed)
--- @param system   string|nil  Optional system prompt text
--- @return table              OpenAI-shaped messages array
local function cl_oai_convert_messages(messages, system)
    local out = {}

    -- Insert system message first if provided.
    if system and system ~= "" then
        table.insert(out, { role = "system", content = system })
    end

    for _, msg in ipairs(messages) do
        if type(msg.content) == "string" then
            -- Simple string content (user prompt turns).
            table.insert(out, { role = msg.role, content = msg.content })
        elseif type(msg.content) == "table" then
            if msg.role == "assistant" then
                -- Assistant messages may have text + tool_use blocks.
                local a_text_parts = {}
                local tool_calls   = {}
                for _, block in ipairs(msg.content) do
                    if block.type == "text" then
                        table.insert(a_text_parts, block.text or "")
                    elseif block.type == "tool_use" then
                        table.insert(tool_calls, {
                            id   = block.id,
                            type = "function",
                            ["function"] = {
                                name      = block.name,
                                arguments = std.json.encode(block.input or {}),
                            },
                        })
                    end
                end
                local text_content = #a_text_parts > 0 and table.concat(a_text_parts, "\n") or nil
                local oai_msg = { role = "assistant" }
                if text_content then oai_msg.content = text_content end
                if #tool_calls > 0 then oai_msg.tool_calls = tool_calls end
                table.insert(out, oai_msg)
            elseif msg.role == "user" then
                -- User messages with tool_result blocks → expand to role="tool" messages.
                local has_tool_result = false
                for _, block in ipairs(msg.content) do
                    if block.type == "tool_result" then
                        has_tool_result = true
                        break
                    end
                end
                if has_tool_result then
                    for _, block in ipairs(msg.content) do
                        if block.type == "tool_result" then
                            table.insert(out, {
                                role         = "tool",
                                tool_call_id = block.tool_use_id,
                                content      = tostring(block.content or ""),
                            })
                        end
                    end
                else
                    -- Regular user message with content array (e.g. text blocks).
                    local parts = {}
                    for _, block in ipairs(msg.content) do
                        if block.type == "text" then
                            table.insert(parts, block.text or "")
                        end
                    end
                    table.insert(out, { role = "user", content = table.concat(parts, "\n") })
                end
            else
                -- Other roles: pass content as-is (fallback).
                table.insert(out, { role = msg.role, content = msg.content })
            end
        end
    end

    return out
end

-- Module-level override for test monkey-patching (set via M._test_set_llm_call).
local _llm_call_override = nil

local function llm_call(opts, messages)
    -- Allow test monkey-patch to intercept all calls.
    if _llm_call_override then
        return _llm_call_override(opts, messages)
    end

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

        -- 3. Extract system role from messages → body.system.
        --    User messages whose content is already a table (tool_result blocks) are
        --    passed through as-is; only string content needs no transformation.
        local sys_text = nil
        local body_messages = {}
        for _, msg in ipairs(messages) do
            if msg.role == "system" and sys_text == nil then
                sys_text = msg.content
            else
                -- Transparent pass-through: content may be a string or a table
                -- (e.g. [{type="tool_result", tool_use_id=..., content=...}]).
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
        -- Attach tools list when provided (multi-file lazy-load path).
        -- Omit entirely when nil to maintain backward compatibility.
        if opts.tools ~= nil then
            body.tools = opts.tools
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

        -- 9. Walk content blocks: separate text blocks and tool_use blocks.
        if type(decoded.content) ~= "table" or #decoded.content == 0 then
            return nil, "anthropic response missing content blocks"
        end
        local text_parts      = {}
        local tool_use_blocks = {}
        for _, block in ipairs(decoded.content) do
            if block.type == "text" then
                table.insert(text_parts, block.text or "")
            elseif block.type == "tool_use" then
                -- Collect tool_use blocks for run_loop dispatch.
                table.insert(tool_use_blocks, {
                    id    = block.id,
                    name  = block.name,
                    input = block.input or {},
                })
            end
        end
        local joined      = table.concat(text_parts, "\n")
        local stop_reason = decoded.stop_reason  -- "end_turn" | "tool_use" | "max_tokens"

        -- If there are no text blocks AND no tool_use blocks, the response is empty.
        if joined == "" and #tool_use_blocks == 0 then
            return nil, "anthropic response missing content blocks"
        end

        -- 10. Build return shape.
        --     tool_use_blocks field is always present when tools were requested, to
        --     allow run_loop to branch on #tool_use_blocks > 0 without checking stop_reason.
        local msg_shape = { content = joined }
        if opts.tools ~= nil then
            msg_shape.tool_use_blocks = tool_use_blocks
            msg_shape.stop_reason     = stop_reason
        end
        return { choices = { { message = msg_shape } } }

    elseif provider ~= "openai" then
        return nil, "provider " .. provider .. " not yet supported in compile_loop"
    end

    -- OpenAI-compatible path.

    local api_key = opts.api_key
    if not api_key or api_key == "" then
        api_key = std.env.get(opts.api_key_env or "OPENAI_API_KEY")
    end
    if not api_key or api_key == "" then
        return nil, "no api_key (opts.api_key or OPENAI_API_KEY env)"
    end

    -- Extract system role from messages (mirrors anthropic branch L:348-358).
    local sys_text     = nil
    local body_messages_raw = {}
    for _, msg in ipairs(messages) do
        if msg.role == "system" and sys_text == nil then
            sys_text = msg.content
        else
            table.insert(body_messages_raw, msg)
        end
    end

    -- Convert Anthropic-shaped messages to OpenAI shape.
    local oai_messages = cl_oai_convert_messages(body_messages_raw, sys_text)

    local base_url = opts.base_url or "https://api.openai.com/v1"
    local body = {
        model       = opts.model or "gpt-4o-mini",
        max_tokens  = opts.max_tokens or 4096,
        temperature = opts.temperature or 0.2,
        messages    = oai_messages,
    }
    if opts.disable_thinking then
        body.chat_template_kwargs = { enable_thinking = false }
    end

    -- tools conversion: input_schema → parameters (Crux #1, R2 guard).
    if opts.tools and #opts.tools > 0 then
        local oai_tools = {}
        for _, t in ipairs(opts.tools) do
            local fn_def = {
                name        = t.name,
                description = t.description or "",
                parameters  = t.input_schema or { type = "object", properties = {} },
            }
            table.insert(oai_tools, { type = "function", ["function"] = fn_def })
        end
        body.tools = oai_tools
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

    -- tools=nil: return raw decoded (backward compat for single-file qwen tests, R4 guard).
    -- tools~=nil: normalize to compile_loop internal shape so run_loop dispatch works (Crux #1).
    if opts.tools == nil then
        return decoded
    end
    return cl_oai_normalize(decoded)
end

-- Parse Aider-style SEARCH/REPLACE blocks from LLM output.
-- Returns (blocks, nil) on success or (nil, error_string) on failure.
-- Each block = { path = nil|string, search = string, replace = string }.
-- Marker lines are excluded; inner text is preserved verbatim (no strip).
--
-- multi_file (bool): when true, <<< path=... >>> headers are required before each
--   SEARCH/REPLACE block. target_files_set (table keyed by path string) is used
--   to validate that every path header names an allowed file.
--   When false (single-file mode), path headers are tolerated but ignored (path = nil).
--
-- Path header format: <<< path=<filepath> >>>  (on its own line, optionally preceded by whitespace)
local function parse_search_replace(text, multi_file, target_files_set)
    local blocks = {}
    local pos = 1
    local len = #text
    local current_path = nil  -- tracks the most recently seen path header

    while pos <= len do
        -- Before looking for SEARCH marker, check if the text at pos is a path header.
        -- Path header pattern: <<< path=<anything> >>> followed by newline (or end).
        -- We scan forward to find either a path header or a SEARCH marker.

        -- Try to find a path header at or after pos (before the next SEARCH marker).
        local ph_start, ph_end, ph_path = text:find("<<<%s*path=([^>]+)%s*>>>", pos)
        local s_start, s_end = text:find("<<<<<<< SEARCH\n", pos, true)

        -- If both exist, pick whichever comes first.
        if ph_start and (not s_start or ph_start < s_start) then
            -- Path header comes before next SEARCH (or there is no SEARCH yet).
            local raw_path = ph_path:match("^%s*(.-)%s*$")  -- trim whitespace
            if multi_file then
                -- Validate against allowlist.
                if not target_files_set[raw_path] then
                    return nil, "path '" .. raw_path .. "' not in target_files allowlist"
                end
            end
            -- In single-file mode, we accept but ignore path headers (current_path stays nil).
            if multi_file then
                current_path = raw_path
            end
            -- Advance past the path header line.
            pos = ph_end + 1
            -- Skip optional newline after path header.
            if pos <= len and text:sub(pos, pos) == "\n" then
                pos = pos + 1
            end
        elseif s_start then
            -- Next thing is a SEARCH marker.
            -- In multi-file mode, a SEARCH without a preceding path header is an error.
            if multi_file and current_path == nil then
                return nil, "missing path header for multi-file mode at offset " .. tostring(s_start)
            end

            -- Find ======= separator after SEARCH marker
            local sep_start, sep_end = text:find("\n=======\n", s_end + 1, true)
            if not sep_start then
                return nil, "malformed SEARCH/REPLACE block: missing ======= separator"
            end

            -- Find >>>>>>> REPLACE marker after separator
            local rep_start, rep_end = text:find("\n>>>>>>> REPLACE", sep_end + 1, true)
            if not rep_start then
                return nil, "malformed SEARCH/REPLACE block: missing >>>>>>> REPLACE marker"
            end

            local search_text  = text:sub(s_end + 1, sep_start - 1)
            local replace_text = text:sub(sep_end + 1, rep_start - 1)

            -- path is current_path (nil for single-file mode, string for multi-file mode).
            table.insert(blocks, { path = current_path, search = search_text, replace = replace_text })
            pos = rep_end + 1
        else
            -- No more path headers or SEARCH markers.
            break
        end
    end

    if #blocks == 0 then
        return nil, "no SEARCH/REPLACE blocks found"
    end
    return blocks, nil
end

-- Whitespace-normalize a string: collapse runs of whitespace to a single space
-- and strip leading/trailing whitespace. Used for the fallback ws-normalized match.
local function ws_normalize(s)
    return (s:gsub("%s+", " "):match("^%s*(.-)%s*$"))
end

-- Apply parsed SEARCH/REPLACE blocks to content.
-- Returns (new_content, failed_indices).
-- Two-stage match:
--   1. exact: content:find(search, 1, true)
--   2. ws-normalized: collapse whitespace in both search and content scan window
-- Blocks that fail both stages are appended to failed_indices and skipped.
-- Successful blocks are applied in order; applied content is updated after each success.
local function apply_blocks(content, blocks)
    local failed_indices = {}
    local current = content

    for i, block in ipairs(blocks) do
        local search  = block.search
        local replace = block.replace

        -- Stage 1: exact match
        local found_s, found_e = current:find(search, 1, true)
        if found_s then
            current = current:sub(1, found_s - 1) .. replace .. current:sub(found_e + 1)
        else
            -- Stage 2: whitespace-normalized match
            -- Scan current content line by line to find a region that ws-normalizes to the same
            -- normalized form as the search text.
            local norm_search = ws_normalize(search)
            local matched = false
            -- We slide a window over content to find a matching substring.
            -- For simplicity, we scan each possible start position in current.
            local cur_len = #current
            local search_len = #search
            -- Heuristic: limit scan to a window that's at most 3× the search length
            -- to avoid O(n²) for large files. We still check all positions.
            local cpos = 1
            while cpos <= cur_len do
                -- Try windows of varying sizes (search_len ± 50% for ws variance)
                local min_win = math.max(1, search_len - math.floor(search_len / 2))
                local max_win = search_len + math.floor(search_len / 2) + 10
                if max_win > cur_len - cpos + 1 then
                    max_win = cur_len - cpos + 1
                end
                local found_window = false
                for wlen = min_win, max_win do
                    local window = current:sub(cpos, cpos + wlen - 1)
                    if ws_normalize(window) == norm_search then
                        current = current:sub(1, cpos - 1) .. replace .. current:sub(cpos + wlen)
                        matched = true
                        found_window = true
                        break
                    end
                end
                if found_window then break end
                cpos = cpos + 1
            end

            if not matched then
                table.insert(failed_indices, i)
            end
        end
    end

    return current, failed_indices
end

-- Build the failure-feedback user message for SEARCH/REPLACE apply failures.
-- Called when one or more blocks could not be applied (SEARCH text not found).
local function build_edit_failure_msg(failed_indices, blocks, current_content)
    local parts = {}
    for _, idx in ipairs(failed_indices) do
        local blk = blocks[idx]
        table.insert(parts, string.format(
            "Edit FAILED: block %d could not be applied. The SEARCH text did not match.\n=== SEARCH (block %d) ===\n%s",
            idx, idx, blk and blk.search or "(nil)"
        ))
    end
    table.insert(parts, "=== Current file content ===\n" .. (current_content or ""))
    table.insert(parts, "Re-emit ALL blocks from scratch with corrected SEARCH text.")
    return table.concat(parts, "\n\n")
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
-- For single-file mode: artifact_path is the absolute path, modified_files is nil.
-- For multi-file mode: artifact_path is nil, modified_files is list<path>.
local function filter_for_tool_output(res)
    return {
        ok             = res.ok,
        artifact_path  = res.artifact_path,   -- single-file: abs path; multi-file: nil
        modified_files = res.modified_files,  -- multi-file: list<path>; single-file: nil
        iters          = res.iters,
        summary        = res.summary,
        failure_reason = res.failure_reason,
        last_error     = res.last_error,
        -- code:    excluded (Counter WF-A defence)
        -- history: excluded (circular-ref risk + context contamination)
    }
end

-- ============================================================
-- Multi-file lazy-load: tool spec + handler
-- ============================================================

-- Maximum number of read_file tool calls allowed within a single iteration.
-- Prevents infinite tool-use loops when the child LLM re-requests the same file.
local MAX_TOOL_CALLS_PER_ITER = 8

-- Tool spec for the child LLM (multi-file branch only).
-- Passed as opts.tools in llm_call; never exposed to the parent agent layer.
local READ_FILE_TOOL = {
    name        = "read_file",
    description = "Read the current content of a target file. Returns the file content as a string. Use this to fetch file content lazily before emitting SEARCH/REPLACE blocks.",
    input_schema = {
        type     = "object",
        required = { "path" },
        properties = {
            path = {
                type        = "string",
                description = "Absolute path. Must be one of the target_files paths provided in the spec.",
            },
        },
    },
}

-- Handle a read_file tool call from the child LLM.
-- Returns {ok=true, content=string} or {ok=false, error=string}.
-- Never raises; errors are propagated as tool_result content so the child LLM
-- can recover (per-iter reset keeps the loop safe).
local function read_file_tool_handler(path, target_files_set)
    if not target_files_set[path] then
        return { ok = false, error = "path '" .. tostring(path) .. "' not in target_files allowlist" }
    end
    local f, err = io.open(path, "r")
    if not f then
        return { ok = false, error = "cannot open: " .. tostring(err) }
    end
    local content = f:read("*a")
    f:close()
    return { ok = true, content = content or "" }
end

-- ============================================================
-- Multi-file helper
-- ============================================================

-- Group parsed blocks by their path field.
-- Returns a table: { [path_string] = {block, ...}, ... }
-- Blocks with path == nil (single-file mode) all map to the key false.
local function group_blocks_by_path(blocks)
    local grouped = {}
    for _, block in ipairs(blocks) do
        local key = block.path or false
        if not grouped[key] then
            grouped[key] = {}
        end
        table.insert(grouped[key], block)
    end
    return grouped
end

-- Apply parsed blocks to each file in target_files and write results.
-- target_files: list of absolute paths (strings).
-- grouped: output of group_blocks_by_path (keyed by path string matching target_files entries).
-- existing_map: { [abs_path] = content_string|nil } — pre-read content.
--
-- Returns:
--   new_contents_map: { [abs_path] = new_content_string }   — only files that had blocks applied
--   all_failed:       list of { path, indices }             — failed blocks per file
--   write_err:        nil or "path: error_string"            — first write failure
local function iterate_files(target_files, grouped, existing_map)
    local new_contents_map = {}
    local all_failed = {}
    local write_err = nil

    for _, abs_path in ipairs(target_files) do
        local file_blocks = grouped[abs_path]
        if file_blocks and #file_blocks > 0 then
            local current = existing_map[abs_path] or ""
            local new_content, failed_indices = apply_blocks(current, file_blocks)
            if #failed_indices > 0 then
                table.insert(all_failed, { path = abs_path, indices = failed_indices, blocks = file_blocks, current_content = current })
            else
                -- Write the new content.
                local f, werr = io.open(abs_path, "w")
                if not f then
                    write_err = abs_path .. ": " .. tostring(werr)
                    break
                end
                f:write(new_content)
                f:close()
                new_contents_map[abs_path] = new_content
            end
        end
    end

    return new_contents_map, all_failed, write_err
end

-- Build a failure-feedback message for multi-file apply failures.
local function build_multifile_edit_failure_msg(all_failed, existing_map)
    local parts = {}
    for _, entry in ipairs(all_failed) do
        for _, idx in ipairs(entry.indices) do
            local blk = entry.blocks[idx]
            table.insert(parts, string.format(
                "Edit FAILED in %s: block %d could not be applied. The SEARCH text did not match.\n=== SEARCH (block %d) ===\n%s",
                entry.path, idx, idx, blk and blk.search or "(nil)"
            ))
        end
        table.insert(parts, "=== Current file content (" .. entry.path .. ") ===\n" .. (existing_map[entry.path] or ""))
    end
    table.insert(parts, "Re-emit ALL blocks from scratch with corrected SEARCH text.")
    return table.concat(parts, "\n\n")
end

-- ============================================================
-- Internal loop body (non-public; called only via make().handler)
-- ============================================================

-- run_loop(conf) executes the structural compile-and-fix loop.
-- conf fields (K-96 full set, all resolved before entry):
--   runner, lang, target_files (list<abs_path>), multi_file (bool), spec,
--   max_iters, system, edit_mode,
--   provider, base_url, api_key, api_key_env, model,
--   max_tokens, temperature, disable_thinking, timeout,
--   on_iter (optional callback)
--
-- For backward compatibility, single-file callers pass conf.target_files = {abs_path}
-- and conf.multi_file = false. The handler normalizes before calling run_loop.
local function run_loop(conf)
    assert(type(conf) == "table", "conf table required")
    assert(conf.target_files and #conf.target_files > 0, "conf.target_files (non-empty list) required")
    assert(conf.spec,        "conf.spec required")
    assert(type(conf.runner) == "function", "conf.runner (function) required")

    local lang       = conf.lang or "lua"
    local max_iters  = conf.max_iters or 5
    local multi_file = conf.multi_file or false
    local mode       = resolve_dump_mode()

    -- In single-file mode, artifact_path is the single absolute path (backward compat).
    -- In multi-file mode, artifact_path is nil; modified_files carries the list.
    local artifact_path = (not multi_file) and conf.target_files[1] or nil

    -- Build a set for fast path-header validation in parse_search_replace.
    local target_files_set = {}
    for _, p in ipairs(conf.target_files) do
        target_files_set[p] = true
    end

    -- Resolve edit_mode.
    -- For single-file: "diff" requires a non-empty target file; fallback to "full".
    -- For multi-file: edit_mode="diff" is required (enforced in handler, but guard here too).
    local edit_mode = conf.edit_mode or "full"

    -- For multi-file lazy-load, do NOT pre-read file contents into initial message.
    -- existing_map starts empty; it is populated on-demand per-iter before apply.
    -- For single-file mode, pre-read as before (existing_map used for initial message + apply base).
    local existing_map = {}
    if not multi_file then
        for _, p in ipairs(conf.target_files) do
            existing_map[p] = read_target_if_exists(p)
        end
    end

    -- Single-file edit_mode fallback (multi-file must use diff — already asserted in handler).
    if not multi_file and edit_mode == "diff" and not existing_map[conf.target_files[1]] then
        log.warn("compile_loop: edit_mode=diff requires an existing non-empty target_file; falling back to full")
        edit_mode = "full"
    end

    -- Select system prompt based on edit_mode and multi_file flag.
    local system
    if edit_mode == "diff" then
        if multi_file then
            system = conf.system or DIFF_SYSTEM_MULTI
        else
            system = conf.system or DIFF_SYSTEM
        end
    else
        system = conf.system or DEFAULT_SYSTEM
    end

    -- ── Multi-file: build lazy-load initial user_content (path list only) ──────
    -- File content is NOT embedded. The child LLM fetches files via read_file tool.
    local multi_initial_user_content
    if multi_file then
        local path_lines = {}
        for _, p in ipairs(conf.target_files) do
            table.insert(path_lines, "  " .. p)
        end
        multi_initial_user_content = conf.spec
            .. "\n\nFiles:\n"
            .. table.concat(path_lines, "\n")
            .. "\n\nUse the read_file tool to fetch file content when needed."
    end

    -- ── Single-file: build initial user_content (original behaviour) ───────────
    local single_initial_user_content
    if not multi_file then
        if edit_mode == "diff" then
            -- Single-file diff mode: embed current content.
            -- existing is guaranteed non-nil here (fallback already applied above).
            single_initial_user_content = conf.spec
                .. "\n\n=== Current file content ===\n"
                .. (existing_map[conf.target_files[1]] or "")
        else
            -- full mode: embed content if present.
            local existing = existing_map[conf.target_files[1]]
            if existing then
                single_initial_user_content = conf.spec
                    .. "\n\n=== Current file content ===\n```" .. lang .. "\n"
                    .. existing
                    .. "\n```"
            else
                single_initial_user_content = conf.spec
            end
        end
    end

    -- ── Per-iter state for multi-file lazy-load ─────────────────────────────────
    -- messages[] is rebuilt each iter from state; not accumulated across iters.
    -- sr_history is reserved for subtask 2 (stagnation_v2); initialized empty here.
    local mf_state = {
        iter           = 0,
        last_err       = nil,   -- most recent verify failure stderr (≤2,000 chars)
        sr_digest_prev = nil,   -- digest of last SR block (≤500 chars)
        sr_history     = {},    -- populated in subtask 2
    }
    assert(type(mf_state.sr_history) == "table", "mf_state.sr_history must be initialized")

    -- For single-file mode, messages accumulate across iters (original behaviour).
    local messages
    if not multi_file then
        messages = {
            { role = "system", content = system },
            { role = "user",   content = single_initial_user_content },
        }
    end

    local history = {}

    for iter = 1, max_iters do
        local obs_target = artifact_path or table.concat(conf.target_files, ",")
        obs_event(mode, "iter_start", { { "iter", iter }, { "target_file", obs_target } })

        -- ── Multi-file: per-iter messages rebuild ───────────────────────────────
        -- messages[] is constructed fresh each iter from system + per-iter user content.
        -- tool_use/tool_result pairs are appended within the iter and dropped at iter end.
        if multi_file then
            mf_state.iter = iter
            -- Build per-iter user content: base + optional last_err + optional sr_digest_prev.
            local user_parts = { multi_initial_user_content }
            if mf_state.last_err and mf_state.last_err ~= "" then
                table.insert(user_parts, "\n=== Last verify error (trimmed) ===\n" .. mf_state.last_err)
            end
            if mf_state.sr_digest_prev and mf_state.sr_digest_prev ~= "" then
                table.insert(user_parts, "\n=== Previous SR digest ===\n" .. mf_state.sr_digest_prev)
            end
            local iter_user_content = table.concat(user_parts, "")
            messages = {
                { role = "system", content = system },
                { role = "user",   content = iter_user_content },
            }
            obs_event(mode, "iter_messages_size", {
                { "iter",         iter },
                { "messages_len", #messages },
                { "user_len",     #iter_user_content },
            })
        end

        -- ── LLM call 1 (multi-file: may return tool_use; single-file: returns SR/code) ──
        local call_opts = conf
        if multi_file then
            -- Attach tool spec so the child LLM can call read_file.
            -- We build a shallow copy of conf with tools added to avoid mutating conf.
            call_opts = {}
            for k, v in pairs(conf) do call_opts[k] = v end
            call_opts.tools = { READ_FILE_TOOL }
        end

        local resp, err = llm_call(call_opts, messages)
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

        -- ── Multi-file: tool_use dispatch loop ──────────────────────────────────
        -- The child LLM may issue read_file calls before emitting SR blocks.
        -- We resolve up to MAX_TOOL_CALLS_PER_ITER calls within this iter,
        -- then do a final LLM call to obtain SR blocks (or accept the SR directly
        -- if no tool_use was requested).
        --
        -- existing_map also serves as a cache (R2 fallback): if the LLM requests the
        -- same path twice, return the cached content instead of re-reading.
        -- The cache is scoped to this iter (existing_map reset per-iter below).
        if multi_file then
            -- Reset per-iter read cache before tool dispatch.
            existing_map = {}

            local tool_call_count = 0
            local cur_resp        = resp

            while true do
                local cur_choice        = (cur_resp.choices or {})[1] or {}
                local cur_msg           = cur_choice.message or {}
                local cur_tool_blocks   = cur_msg.tool_use_blocks or {}

                if #cur_tool_blocks == 0 then
                    -- No tool_use requested; fall through to SR parse below.
                    resp = cur_resp
                    break
                end

                -- Hard cap: give up if too many tool calls in one iter.
                if tool_call_count + #cur_tool_blocks > MAX_TOOL_CALLS_PER_ITER then
                    obs_event(mode, "tool_loop_giveup", { { "iter", iter }, { "count", tool_call_count } })
                    local giveup_err = "exceeded MAX_TOOL_CALLS_PER_ITER=" .. MAX_TOOL_CALLS_PER_ITER .. " within a single iter"
                    return {
                        ok             = false,
                        failure_reason = "tool_loop",
                        last_error     = giveup_err,
                        iters          = iter,
                        summary        = make_summary(false, iter, max_iters, "tool_loop"),
                        artifact_path  = nil,
                        history        = history,
                    }
                end

                -- Build assistant message carrying the tool_use blocks.
                -- content field: text portion (may be empty string).
                local assistant_content = {}
                -- Include text blocks if present.
                if cur_msg.content and cur_msg.content ~= "" then
                    table.insert(assistant_content, { type = "text", text = cur_msg.content })
                end
                -- Include tool_use blocks (raw form: id, name, input).
                for _, tb in ipairs(cur_tool_blocks) do
                    table.insert(assistant_content, {
                        type  = "tool_use",
                        id    = tb.id,
                        name  = tb.name,
                        input = tb.input,
                    })
                end
                table.insert(messages, { role = "assistant", content = assistant_content })

                -- Dispatch each tool_use block and collect tool_result blocks.
                local tool_result_content = {}
                for _, tb in ipairs(cur_tool_blocks) do
                    tool_call_count = tool_call_count + 1
                    if tb.name == "read_file" then
                        local path = (tb.input or {}).path or ""
                        -- Use cached result if available (R2 fallback: dedup repeated reads).
                        local cached = existing_map[path]
                        local dispatch_result
                        if cached ~= nil then
                            dispatch_result = { ok = true, content = cached }
                            obs_event(mode, "tool_use", {
                                { "iter",   iter },
                                { "path",   path },
                                { "ok",     true },
                                { "cached", true },
                            })
                        else
                            dispatch_result = read_file_tool_handler(path, target_files_set)
                            if dispatch_result.ok then
                                -- Cache the result for this iter.
                                existing_map[path] = dispatch_result.content
                                obs_event(mode, "tool_use", {
                                    { "iter", iter },
                                    { "path", path },
                                    { "ok",   true },
                                })
                            else
                                obs_event(mode, "tool_use_fail", {
                                    { "iter", iter },
                                    { "path", path },
                                    { "err",  dispatch_result.error },
                                })
                            end
                        end

                        -- Build tool_result block (error string propagated to child LLM).
                        local result_text
                        if dispatch_result.ok then
                            result_text = dispatch_result.content
                        else
                            result_text = "ERROR: " .. tostring(dispatch_result.error)
                        end
                        table.insert(tool_result_content, {
                            type        = "tool_result",
                            tool_use_id = tb.id,
                            content     = result_text,
                        })
                    else
                        -- Unknown tool name; return error to child LLM.
                        obs_event(mode, "tool_use_fail", {
                            { "iter", iter },
                            { "path", tostring((tb.input or {}).path or "") },
                            { "err",  "unknown tool: " .. tostring(tb.name) },
                        })
                        table.insert(tool_result_content, {
                            type        = "tool_result",
                            tool_use_id = tb.id,
                            content     = "ERROR: unknown tool '" .. tostring(tb.name) .. "'",
                        })
                    end
                end

                -- Append user message containing all tool_result blocks.
                table.insert(messages, { role = "user", content = tool_result_content })

                -- Second LLM call: provide tool results so the child LLM can emit SR blocks.
                local resp2, err2 = llm_call(call_opts, messages)
                if not resp2 then
                    local err_str = tostring(err2)
                    return {
                        ok             = false,
                        failure_reason = "llm_call",
                        last_error     = err_str:sub(-800),
                        iters          = iter,
                        summary        = make_summary(false, iter, max_iters, "llm_call"),
                        artifact_path  = nil,
                        history        = history,
                    }
                end
                cur_resp = resp2
                -- Loop: if the child LLM issues more tool_use calls, repeat.
            end
            -- resp now holds the final response (no more tool_use blocks).
        end
        -- ── end of multi-file tool dispatch loop ────────────────────────────────

        local choice  = (resp.choices or {})[1] or {}
        local msg_obj = choice.message or {}

        -- Extract text-only content for SR parse (tool_use blocks must NOT be passed
        -- to parse_search_replace — only text content is valid SR source).
        local content = msg_obj.content or ""

        -- ── diff mode ──────────────────────────────────────────────────────────
        if edit_mode == "diff" then
            -- Parse SEARCH/REPLACE blocks from the LLM text response.
            -- Pass multi_file flag and allowlist set for path validation.
            local blocks, parse_err = parse_search_replace(content, multi_file, target_files_set)
            if not blocks then
                -- Parse failure: tell the child LLM to re-emit valid blocks.
                local fmt_msg = "Output format invalid: " .. tostring(parse_err)
                    .. "\nRe-emit blocks correctly."
                local entry = { iter = iter, code = nil, result = { ok = false, stderr = fmt_msg, stdout = "", exit_code = -1 }, raw = content }
                table.insert(history, entry)
                obs_event(mode, "iter_result", {
                    { "iter",       iter },
                    { "ok",         false },
                    { "exit_code",  -1 },
                    { "stderr_len", #fmt_msg },
                })
                if conf.on_iter then
                    local cb_ok, cb_err = pcall(conf.on_iter, entry)
                    if not cb_ok then
                        log.warn("compile_loop: on_iter callback error: " .. tostring(cb_err))
                    end
                end
                -- For multi-file: update state; messages[] will be rebuilt next iter.
                if multi_file then
                    -- Compute sr_hash for parse-error case: hash the raw content (LLM output).
                    -- Using a tagged prefix to distinguish parse errors from valid SR blocks.
                    local parse_sr_hash = compute_sr_hash("<parse_err:" .. compute_sr_hash(fmt_msg) .. ">")
                    update_state(mf_state, {
                        last_err       = fmt_msg,
                        sr_hash_append = parse_sr_hash,
                    })
                    -- Stagnation check using sr_history (messages[] independent).
                    if is_stagnant_v2(mf_state, true) then
                        obs_event(mode, "stagnation_v2", {
                            { "iter",           iter },
                            { "sr_hash_recent", parse_sr_hash:sub(1, 8) },
                            { "reason",         "sr_history_repeat" },
                        })
                        return {
                            ok             = false,
                            failure_reason = "stagnation",
                            last_error     = mf_state.last_err or "",
                            iters          = iter,
                            summary        = make_summary(false, iter, max_iters, "stagnation"),
                            artifact_path  = nil,
                            history        = history,
                        }
                    end
                    -- messages[] for next iter is rebuilt from state; drop current iter messages.
                else
                    -- Stagnation check uses history entry result.stderr.
                    if is_stagnant(history) then
                        obs_event(mode, "stagnation", { { "iters", iter } })
                        return {
                            ok             = false,
                            failure_reason = "stagnation",
                            last_error     = fmt_msg:sub(-800),
                            iters          = iter,
                            summary        = make_summary(false, iter, max_iters, "stagnation"),
                            artifact_path  = artifact_path,
                            history        = history,
                        }
                    end
                    table.insert(messages, { role = "assistant", content = content })
                    table.insert(messages, { role = "user",      content = fmt_msg })
                end

            elseif multi_file then
                -- ── multi-file diff apply (per-iter rebuild path) ────────────────
                -- existing_map was populated by the tool dispatch loop above.
                -- Apply blocks using the on-demand-populated existing_map.
                local grouped = group_blocks_by_path(blocks)
                local _new_contents, all_failed, write_err = iterate_files(conf.target_files, grouped, existing_map)

                if write_err then
                    local werr_str = tostring(write_err)
                    return {
                        ok             = false,
                        failure_reason = "open_target_file",
                        last_error     = werr_str,
                        iters          = iter,
                        summary        = make_summary(false, iter, max_iters, "open_target_file"),
                        artifact_path  = nil,
                        history        = history,
                    }
                end

                if #all_failed > 0 then
                    local fail_msg = build_multifile_edit_failure_msg(all_failed, existing_map)
                    local entry = { iter = iter, code = nil, result = { ok = false, stderr = fail_msg, stdout = "", exit_code = -1 }, raw = content }
                    table.insert(history, entry)
                    obs_event(mode, "iter_result", {
                        { "iter",       iter },
                        { "ok",         false },
                        { "exit_code",  -1 },
                        { "stderr_len", #fail_msg },
                    })
                    if conf.on_iter then
                        local cb_ok, cb_err = pcall(conf.on_iter, entry)
                        if not cb_ok then
                            log.warn("compile_loop: on_iter callback error: " .. tostring(cb_err))
                        end
                    end
                    -- Update state via update_state (DRY trim policy).
                    local apply_sr_hash = compute_sr_hash(content)
                    update_state(mf_state, {
                        last_err       = fail_msg,
                        sr_digest_prev = content,
                        sr_hash_append = apply_sr_hash,
                    })
                    -- Stagnation check using sr_history (messages[] independent).
                    if is_stagnant_v2(mf_state, true) then
                        obs_event(mode, "stagnation_v2", {
                            { "iter",           iter },
                            { "sr_hash_recent", apply_sr_hash:sub(1, 8) },
                            { "reason",         "sr_history_repeat" },
                        })
                        return {
                            ok             = false,
                            failure_reason = "stagnation",
                            last_error     = mf_state.last_err or "",
                            iters          = iter,
                            summary        = make_summary(false, iter, max_iters, "stagnation"),
                            artifact_path  = nil,
                            history        = history,
                        }
                    end
                    -- messages[] for next iter is rebuilt from state (no accumulation).
                else
                    -- All blocks applied and written. Call runner with paths list (Crux #3).
                    local rr = conf.runner(conf.target_files) or {}
                    local entry = { iter = iter, code = nil, result = rr, raw = content }
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
                            ok             = true,
                            artifact_path  = nil,
                            modified_files = conf.target_files,
                            iters          = iter,
                            summary        = make_summary(true, iter, max_iters, nil),
                            history        = history,
                        }
                    end

                    -- Runner failed: update state via update_state (DRY trim policy).
                    local rr_stderr = tostring(rr.stderr or "")
                    local runner_sr_hash = compute_sr_hash(content)
                    update_state(mf_state, {
                        last_err       = rr_stderr,
                        sr_digest_prev = content,
                        sr_hash_append = runner_sr_hash,
                    })
                    -- Stagnation check (multi-file): use sr_history, independent of messages[].
                    local runner_failed = (rr.ok == false)
                    if is_stagnant_v2(mf_state, runner_failed) then
                        obs_event(mode, "stagnation_v2", {
                            { "iter",           iter },
                            { "sr_hash_recent", runner_sr_hash:sub(1, 8) },
                            { "reason",         "sr_history_repeat" },
                        })
                        return {
                            ok             = false,
                            failure_reason = "stagnation",
                            last_error     = mf_state.last_err or "",
                            iters          = iter,
                            summary        = make_summary(false, iter, max_iters, "stagnation"),
                            artifact_path  = nil,
                            history        = history,
                        }
                    end
                    -- messages[] for next iter is rebuilt from mf_state (no accumulation).
                end

            else
                -- ── single-file diff apply (original path) ───────────────────────
                local single_path = conf.target_files[1]
                local current_content = read_target_if_exists(single_path) or existing_map[single_path]
                local new_content, failed_indices = apply_blocks(current_content, blocks)

                if #failed_indices > 0 then
                    -- Partial or total apply failure: report and ask for re-emit.
                    local fail_msg = build_edit_failure_msg(failed_indices, blocks, current_content)
                    local entry = { iter = iter, code = nil, result = { ok = false, stderr = fail_msg, stdout = "", exit_code = -1 }, raw = content }
                    table.insert(history, entry)
                    obs_event(mode, "iter_result", {
                        { "iter",       iter },
                        { "ok",         false },
                        { "exit_code",  -1 },
                        { "stderr_len", #fail_msg },
                    })
                    if conf.on_iter then
                        local cb_ok, cb_err = pcall(conf.on_iter, entry)
                        if not cb_ok then
                            log.warn("compile_loop: on_iter callback error: " .. tostring(cb_err))
                        end
                    end
                    if is_stagnant(history) then
                        obs_event(mode, "stagnation", { { "iters", iter } })
                        return {
                            ok             = false,
                            failure_reason = "stagnation",
                            last_error     = fail_msg:sub(-800),
                            iters          = iter,
                            summary        = make_summary(false, iter, max_iters, "stagnation"),
                            artifact_path  = artifact_path,
                            history        = history,
                        }
                    end
                    table.insert(messages, { role = "assistant", content = content })
                    table.insert(messages, { role = "user",      content = fail_msg })
                else
                    -- All blocks applied successfully — write new content and run.
                    local f, werr = io.open(single_path, "w")
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
                    f:write(new_content)
                    f:close()

                    -- Single-file runner call with single string path (Crux #3).
                    local rr = conf.runner(single_path) or {}
                    local entry = { iter = iter, code = new_content, result = rr, raw = content }
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
                            code          = new_content,
                            artifact_path = artifact_path,
                            iters         = iter,
                            summary       = make_summary(true, iter, max_iters, nil),
                            history       = history,
                        }
                    end

                    if is_stagnant(history) then
                        local last_stderr = tostring((rr.stderr) or ""):sub(-800)
                        obs_event(mode, "stagnation", { { "iters", iter } })
                        return {
                            ok             = false,
                            failure_reason = "stagnation",
                            last_error     = last_stderr,
                            code           = new_content,
                            iters          = iter,
                            summary        = make_summary(false, iter, max_iters, "stagnation"),
                            artifact_path  = artifact_path,
                            history        = history,
                        }
                    end

                    -- Runner failed — provide runner feedback for next iteration.
                    table.insert(messages, { role = "assistant", content = content })
                    table.insert(messages, { role = "user",      content = build_failure_msg(lang, rr) })
                end
            end

        -- ── full mode (default) ────────────────────────────────────────────────
        else
            local single_path = conf.target_files[1]
            local code = extract_code(content, lang)

            -- Write target file (full-file replace — next_full_file action)
            local f, werr = io.open(single_path, "w")
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

            -- Single-file runner call with single string path (Crux #3).
            local rr = conf.runner(single_path) or {}
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
        -- ── end of edit_mode branch ────────────────────────────────────────────
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
triggers. Returns ok/iters/summary and, on failure, failure_reason/last_error.

Single-file mode: provide target_file (string).
Multi-file mode: provide target_files (array of absolute paths). Requires edit_mode=diff.
target_file and target_files are mutually exclusive.]],
        input_schema = {
            type     = "object",
            required = { "spec" },
            properties = {
                spec = {
                    type        = "string",
                    description = "Full specification the child LLM must satisfy.",
                },
                target_file = {
                    type        = "string",
                    description = "Absolute path of the file (single-file mode). Read on entry if it already exists, then written on each iteration. Mutually exclusive with target_files.",
                },
                target_files = {
                    type        = "array",
                    items       = { type = "string" },
                    description = "Array of absolute paths (multi-file mode). Mutually exclusive with target_file. Multi-file mode requires edit_mode=diff.",
                },
                lang = {
                    type        = "string",
                    description = "Code fence language label (default: lua).",
                },
            },
        },
    }

    local function handler(input)
        -- Crux #2: target_file and target_files are mutually exclusive.
        assert(
            not (input.target_file and input.target_files),
            "target_file and target_files are mutually exclusive"
        )
        -- At least one must be provided.
        assert(
            input.target_file or input.target_files,
            "target_file (string) or target_files (array) is required"
        )

        -- Determine multi_file mode and normalize to internal list.
        local multi_file
        local files_list

        if input.target_files then
            -- Multi-file mode entry.
            multi_file = true
            assert(type(input.target_files) == "table", "target_files must be an array")
            assert(#input.target_files > 0, "target_files must not be empty")
            for i, v in ipairs(input.target_files) do
                assert(type(v) == "string", "target_files[" .. i .. "] must be a string")
            end
            -- Crux #2: normalize to internal list with abs paths applied element-wise.
            files_list = {}
            for _, p in ipairs(input.target_files) do
                table.insert(files_list, to_abs(p))
            end
        else
            -- Single-file mode entry (target_file string).
            multi_file = false
            files_list = { to_abs(input.target_file) }
        end

        -- Crux #2 / design-selection 5: multi-file mode requires edit_mode=diff.
        local effective_edit_mode = conf.edit_mode
        assert(
            not (multi_file and effective_edit_mode == "full"),
            "multi-file mode requires edit_mode=diff"
        )

        -- Resolve LLM fields at call time.
        -- Priority: conf.llm.<field> → _AGENT_LLM_CTX top → nil (env fallback in llm_call)
        local parent_ctx = agent._llm_ctx_top() or {}
        local llm_conf   = conf.llm or {}

        local resolved_conf = {
            -- runner (from factory conf, never from input)
            runner   = conf.runner,

            -- tool input fields (normalized)
            lang         = input.lang or conf.lang or "lua",
            target_files = files_list,   -- internal list (1-element for single-file)
            multi_file   = multi_file,
            spec         = input.spec,

            -- factory conf fields
            max_iters = conf.max_iters,
            system    = conf.system,
            edit_mode = effective_edit_mode,
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

-- ============================================================
-- Test helpers (internal; _ prefix signals non-public)
-- ============================================================

--- Override the internal llm_call function for test monkey-patching.
--- Call M._test_reset_llm_call() after the test to restore production behaviour.
--- Production callers must never call this.
function M._test_set_llm_call(fn)
    assert(type(fn) == "function", "_test_set_llm_call requires a function")
    _llm_call_override = fn
end

--- Reset the llm_call override installed by M._test_set_llm_call().
function M._test_reset_llm_call()
    _llm_call_override = nil
end

return M
