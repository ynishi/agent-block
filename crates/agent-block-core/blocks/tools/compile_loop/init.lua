-- blocks/tools/compile_loop/init.lua — Tool factory for the autonomous compile-and-fix loop.
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
--     edit_mode = "full"|"diff"?, -- default "full"; "diff" edits through tools
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

-- Provider wire format (request building, tool_choice / thinking translation,
-- response normalization) is shared with blocks/agent via the `llm_proto`
-- package. This module owns the compile-and-fix loop, not the protocol.
local proto = require("llm_proto")
local proto_anthropic = proto.adapter("anthropic")
local proto_openai = proto.adapter("openai")

-- The ReAct mechanics of one iteration: call, dispatch the tools we handed
-- it, repeat. The build stays out here.
local tool_loop = require("tool_loop")

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
    if not v then
        return false
    end
    v = string.lower(tostring(v))
    return v == "1" or v == "true" or v == "yes" or v == "on"
end

-- Module-level override for test monkey-patching of std.env.get (set via M._test_set_env_get).
-- Declared here so resolve_temperature() can close over it as an upvalue.
local _env_get_override = nil

--- resolve_temperature() — infallible, returns a number.
--- Priority: caller (opts.temperature) > COMPILE_LOOP_LLM_TEMPERATURE env > 0.0 default.
--- This function returns only the env/default tier; caller tier is applied at the call site.
local function resolve_temperature()
    local s
    if _env_get_override then
        s = _env_get_override("COMPILE_LOOP_LLM_TEMPERATURE")
    else
        s = std.env.get("COMPILE_LOOP_LLM_TEMPERATURE")
    end
    if s == nil then
        return 0.0
    end
    local n = tonumber(s)
    if n == nil then
        log.warn(
            "compile_loop: COMPILE_LOOP_LLM_TEMPERATURE="
                .. tostring(s)
                .. " is not a valid number; falling back to 0.0"
        )
        return 0.0
    end
    return n
end

local function normalize_dump_mode(v)
    if not v or v == "" then
        return nil
    end
    v = string.lower(tostring(v))
    if v == "off" or v == "none" then
        return "off"
    end
    if v == "meta" then
        return "meta"
    end
    if v == "full" then
        return "full"
    end
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

-- Process-lifetime cache for the dump mode. llm_call fires per iteration, per
-- tool-loop turn and per distill chunk; env vars do not change mid-process, so
-- resolving once avoids re-reading env and repeating the prod-downgrade warn.
local _dump_mode_cache = nil

local function resolve_dump_mode_cached()
    if _dump_mode_cache == nil then
        _dump_mode_cache = resolve_dump_mode()
    end
    return _dump_mode_cache
end

-- Redact credential-bearing headers before they are emitted in full mode.
-- Applied to both request headers (api key / bearer token) and response
-- headers (proxy stacks can return Set-Cookie session tokens).
-- Keep this list in sync with the other two copies: blocks/agent/init.lua
-- sanitize_headers_for_dump and REDACTED_HEADERS in src/bridge/http.rs. The Rust
-- site is a superset: these exact names plus the ab.obs substring policy
-- (token / secret / password / api_key / access_key / private_key / ...).
local function sanitize_headers_for_dump(headers)
    local out = {}
    for k, v in pairs(headers or {}) do
        local lk = string.lower(tostring(k))
        if
            lk == "x-api-key"
            or lk == "authorization"
            or lk == "set-cookie"
            or lk == "cookie"
            or lk == "proxy-authorization"
        then
            out[k] = "***REDACTED***"
        else
            out[k] = v
        end
    end
    return out
end

local LLM_DUMP_PREFIX = "ab.obs"

local function kv_escape(v)
    if v == nil then
        return "nil"
    end
    if type(v) == "boolean" or type(v) == "number" then
        return tostring(v)
    end
    local s = tostring(v)
    if s == "" then
        return '""'
    end
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
    if mode == "off" then
        return
    end
    local entries = {
        { "prefix", LLM_DUMP_PREFIX },
        { "event", event_name },
        { "component", "compile_loop" },
    }
    for _, f in ipairs(fields or {}) do
        table.insert(entries, f)
    end
    log.info(format_kv(entries))
end

-- Summarize an fs_edit `edits` array for the observability line.
--
-- Where an edit landed exists only in the call that made it: `modified_set`
-- keeps which files changed, not which lines. Dropping that leaves the two
-- questions worth asking about a fix — did it touch the lines it was pointed
-- at, and how far did it move beyond them — answerable only by argument.
-- Recording it costs nothing here because edits are addressed by line: a
-- search-and-replace protocol would have to reconstruct the position after the
-- fact, if it could at all.
--
-- Positions and magnitudes only. `expect` and `replace` are source text and do
-- not belong in a log line.
--
-- @param edits table|nil  Array of { start_line, end_line, expect, replace }
-- @return string  Addressed ranges, "12-15,40-40" ("" when there are none)
-- @return integer Lines the edits replace
-- @return integer Lines they put back
local function summarize_edits(edits)
    local ranges, removed, added = {}, 0, 0
    for _, e in ipairs(edits or {}) do
        local first, last = tonumber(e.start_line), tonumber(e.end_line)
        -- An entry without an address is not an edit, so it contributes
        -- nothing rather than inflating the magnitudes with a phantom range.
        if first and last then
            table.insert(ranges, first .. "-" .. last)
            removed = removed + (last - first + 1)
            -- An empty replacement deletes the range, so it adds no lines.
            if type(e.replace) == "string" and e.replace ~= "" then
                local n = 1
                for _ in e.replace:gmatch("\n") do
                    n = n + 1
                end
                added = added + n
            end
        end
    end
    return table.concat(ranges, ","), removed, added
end

local DEFAULT_SYSTEM = [[You are an expert programmer.
You will be given a spec and asked to write code that runs and passes its self-checks.
Output ONLY the complete file contents in a single fenced code block (e.g. ```lua\n...\n```).
No prose before or after the block.
On retry, output the WHOLE corrected file (not a diff). Keep changes minimal.]]

local DIFF_SYSTEM = [[You are an expert programmer editing an existing file.

Edit through the tools, not by printing code:
- read_file / read_file_range to see the current content. Their output is
  line-numbered, and those line numbers are what fs_edit addresses.
- fs_edit to change it: give start_line, end_line and the expected current text
  of those lines. expect must be exact. A rejected edit tells you what is
  actually at those lines — correct it from that.
- Make the SMALLEST changes that satisfy the spec.

When every edit has been applied, reply with the single word DONE.]]

-- Multi-file differs only in that a path is named on every call; the editing
-- contract is the same, so the two prompts stay parallel.
local DIFF_SYSTEM_MULTI = [[You are an expert programmer editing several existing files.

Edit through the tools, not by printing code:
- read_file / read_file_range to see a file's current content. Their output is
  line-numbered, and those line numbers are what fs_edit addresses.
- fs_edit to change it: pass the path, plus edits giving start_line, end_line
  and the expected current text of those lines. expect must be exact. A
  rejected edit tells you what is actually at those lines — correct it from
  that.
- Every path must be one of the target files you were given.
- Make the SMALLEST changes that satisfy the spec.

When every edit has been applied, reply with the single word DONE.]]

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
            iters,
            max_iters,
            STAGNATION_WINDOW
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
    local ref = (history[#history].result or {}).stderr or ""
    for i = #history - STAGNATION_WINDOW + 1, #history do
        if ((history[i].result or {}).stderr or "") ~= ref then
            return false
        end
    end
    return true
end

-- FNV-1a 32-bit hash (inline fallback; no external dependency required).
-- Returns a decimal string representation of the 32-bit hash value.
local function fnv1a_hash(s)
    s = s or ""
    local hash = 2166136261 -- FNV offset basis (32-bit)
    for i = 1, #s do
        local byte = string.byte(s, i)
        -- XOR with byte then multiply by FNV prime (16777619), truncated to 32-bit.
        hash = (hash ~ byte) * 16777619
        -- Keep only lower 32 bits to prevent integer overflow accumulation.
        hash = hash & 0xFFFFFFFF
    end
    return tostring(hash)
end

-- Compute a stable hash for an iteration's edit signature.
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
--   (2) Among the last STAGNATION_WINDOW entries, all STAGNATION_WINDOW share the same sr_hash
--       (full-window identical-hash requirement; partial matches do not trigger stagnation)
--   (3) The most recent verify outcome is failure (caller passes last_verify_failed = true)
--
-- Returns: boolean
local function is_stagnant_v2(state, last_verify_failed)
    assert(type(state) == "table", "state required")
    assert(type(state.sr_history) == "table", "state.sr_history must be initialized as table")

    if #state.sr_history < STAGNATION_WINDOW then
        return false
    end
    if not last_verify_failed then
        return false
    end

    -- Collect the last STAGNATION_WINDOW entries.
    local recent = {}
    for i = #state.sr_history - STAGNATION_WINDOW + 1, #state.sr_history do
        recent[#recent + 1] = state.sr_history[i]
    end

    -- Count occurrences of each hash within the recent window.
    -- Stagnation requires ALL window entries to share the same hash (c >= STAGNATION_WINDOW).
    -- A 2-of-3 partial match is not sufficient; LLM must have fully converged to one output.
    local counts = {}
    for _, h in ipairs(recent) do
        counts[h] = (counts[h] or 0) + 1
    end
    for _, c in pairs(counts) do
        if c >= STAGNATION_WINDOW then
            return true
        end
    end
    return false
end

-- Convert a modified_set (path → true map) to a sorted list of path strings.
-- Used to populate the modified_files field on every return path in the SR block.
-- pure function, no errors.
local function collect_modified_paths(set)
    local paths = {}
    for path in pairs(set) do
        paths[#paths + 1] = path
    end
    table.sort(paths)
    return paths
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
    if m then
        return m
    end
    -- Fallback: any fence
    m = text:match("```%w*%s*\n(.-)\n```")
    if m then
        return m
    end
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
-- Internal: OpenAI tool-use helpers
-- ============================================================

--- Normalize a raw OpenAI chat completion response into compile_loop internal shape.
--- Internal shape (tools path):
---   { choices = { { message = { content = joined_text,
---                               tool_use_blocks = [{id, name, input}],
---                               stop_reason = string } } } }
--- @param raw table  Parsed OpenAI JSON response
--- @return table|nil  compile_loop-shape table on success
--- @return string|nil Error string on failure
local function cl_oai_normalize(raw)
    local decoded, perr = proto_openai.parse(raw)
    if not decoded then
        return nil, perr
    end

    local text_parts = {}
    local tool_use_blocks = {}
    for _, block in ipairs(decoded.content) do
        if block.type == "text" then
            table.insert(text_parts, block.text or "")
        elseif block.type == "tool_use" then
            -- `thinking` blocks are intentionally skipped: reasoning text must
            -- not leak into the content that becomes a patch / answer.
            table.insert(tool_use_blocks, {
                id = block.id,
                name = block.name or "",
                input = block.input or {},
                is_error_hint = block.is_error_hint,
            })
        end
    end

    return {
        choices = {
            {
                message = {
                    content = table.concat(text_parts, "\n"),
                    tool_use_blocks = tool_use_blocks,
                    stop_reason = decoded.stop_reason,
                },
            },
        },
    },
        nil
end

-- Module-level override for test monkey-patching (set via M._test_set_llm_call).
local _llm_call_override = nil

local function llm_call(opts, messages)
    -- Allow test monkey-patch to intercept all calls.
    if _llm_call_override then
        return _llm_call_override(opts, messages)
    end

    -- Resolved once per process; only "full" emits prompt/response bodies below.
    local mode = resolve_dump_mode_cached()

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

        -- 4-5. Body + headers via the shared protocol layer (llm_proto.anthropic).
        --      cache_control defaults OFF here: compile_loop sends a fresh
        --      prompt per iteration, so the markers would only add bytes.
        local req, build_err = proto_anthropic.build({
            model = model,
            messages = body_messages,
            system = sys_text,
            tools = opts.tools,
            max_tokens = opts.max_tokens or 4096,
            tool_choice = opts.tool_choice,
            parallel_tool_calls = opts.parallel_tool_calls,
            thinking = opts.thinking,
            cache_control = opts.cache_control or false,
            api_key = api_key,
            base_url = opts.base_url,
        })
        if not req then
            return nil, build_err
        end
        local headers = req.headers

        -- 6. HTTP call
        -- Encoded once so the dumped payload is byte-identical to the wire body.
        local body_json = std.json.encode(req.body)
        if mode == "full" then
            obs_event(mode, "request_headers", { { "payload", std.json.encode(sanitize_headers_for_dump(headers)) } })
            obs_event(mode, "request_body", { { "payload", body_json } })
        end
        local resp = http.request(req.url, {
            method = "POST",
            headers = headers,
            body = body_json,
            timeout = opts.timeout or 120,
            -- Policy flag for the host JSONL dump sink (AGENT_BLOCK_LLM_DUMP_DIR).
            dump = (mode == "full") and "full" or nil,
        })
        if mode == "full" then
            local resp_headers = sanitize_headers_for_dump(resp.headers)
            obs_event(mode, "response_headers", { { "payload", std.json.encode(resp_headers) } })
            obs_event(mode, "response_body", { { "payload", tostring(resp.body or "") } })
        end

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
        local parsed, perr = proto_anthropic.parse(decoded)
        if not parsed then
            return nil, perr
        end
        local text_parts = {}
        local tool_use_blocks = {}
        -- `thinking` / `redacted_thinking` blocks fall through both branches:
        -- reasoning text must not become part of a patch, and compile_loop
        -- rebuilds its prompt each iteration so nothing needs echoing back.
        for _, block in ipairs(parsed.content) do
            if block.type == "text" then
                table.insert(text_parts, block.text or "")
            elseif block.type == "tool_use" then
                -- Collect tool_use blocks for run_loop dispatch.
                table.insert(tool_use_blocks, {
                    id = block.id,
                    name = block.name,
                    input = block.input or {},
                })
            end
        end
        local joined = table.concat(text_parts, "\n")
        local stop_reason = parsed.stop_reason -- "end_turn" | "tool_use" | "max_tokens"

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
            msg_shape.stop_reason = stop_reason
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
    local sys_text = nil
    local body_messages_raw = {}
    for _, msg in ipairs(messages) do
        if msg.role == "system" and sys_text == nil then
            sys_text = msg.content
        else
            table.insert(body_messages_raw, msg)
        end
    end

    -- Message / tool conversion and the reasoning dialect split live in
    -- llm_proto.openai. `disable_thinking` (Qwen) maps onto the shared
    -- `thinking` spec; an explicit opts.thinking wins when both are set.
    local thinking_spec = opts.thinking
    if thinking_spec == nil and opts.disable_thinking then
        thinking_spec = { enabled = false }
    end

    -- Model default stays literal (no OPENAI_MODEL env fallback) to keep the
    -- resolution order compile_loop callers already rely on.
    local req, build_err = proto_openai.build({
        model = opts.model or "gpt-4o-mini",
        messages = body_messages_raw,
        system = sys_text,
        tools = opts.tools,
        max_tokens = opts.max_tokens or 4096,
        temperature = opts.temperature or resolve_temperature(),
        tool_choice = opts.tool_choice,
        parallel_tool_calls = opts.parallel_tool_calls,
        thinking = thinking_spec,
        dialect = opts.dialect,
        api_key = api_key,
        base_url = opts.base_url,
    })
    if not req then
        return nil, build_err
    end
    local headers = req.headers
    headers["User-Agent"] = "Mozilla/5.0" -- RunPod proxy / Cloudflare gate

    -- Encoded once so the dumped payload is byte-identical to the wire body.
    local body_json = std.json.encode(req.body)
    if mode == "full" then
        obs_event(mode, "request_headers", { { "payload", std.json.encode(sanitize_headers_for_dump(headers)) } })
        obs_event(mode, "request_body", { { "payload", body_json } })
    end
    local resp = http.request(req.url, {
        method = "POST",
        headers = headers,
        body = body_json,
        timeout = opts.timeout or 120,
        -- Policy flag for the host JSONL dump sink (AGENT_BLOCK_LLM_DUMP_DIR).
        dump = (mode == "full") and "full" or nil,
    })
    if mode == "full" then
        local resp_headers = sanitize_headers_for_dump(resp.headers)
        obs_event(mode, "response_headers", { { "payload", std.json.encode(resp_headers) } })
        obs_event(mode, "response_body", { { "payload", tostring(resp.body or "") } })
    end
    if resp.status ~= 200 then
        return nil, "API error " .. tostring(resp.status) .. " body=" .. tostring(resp.body or "")
    end
    local ok, decoded = pcall(std.json.decode, resp.body)
    if not ok or type(decoded) ~= "table" then
        return nil, "decode failed: " .. tostring(decoded)
    end

    return cl_oai_normalize(decoded)
end

-- Read target file if it already exists and is non-empty.
-- Returns file content as a string, or nil when the file is absent, empty, or unreadable.
-- Uses to_abs so that relative paths are resolved before io.open.
local function read_target_if_exists(path)
    local abs_path = to_abs(path)
    local f, _ = io.open(abs_path, "r")
    if not f then
        return nil
    end
    local content = f:read("*a")
    f:close()
    if not content or content == "" then
        return nil
    end
    return content
end

-- Write content to path, checking both the open and the write result.
-- Shared by the single-file write paths, iterate_files, and the
-- fs_edit tool handler so error shaping stays consistent.
-- Returns true on success, or (false, err_string) on failure.
local function write_file(path, content)
    local f, oerr = io.open(path, "w")
    if not f then
        return false, tostring(oerr)
    end
    local wok, werr = f:write(content)
    f:close()
    if not wok then
        return false, tostring(werr or "write failed")
    end
    return true
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
        ok = res.ok,
        artifact_path = res.artifact_path, -- single-file: abs path; multi-file: nil
        modified_files = res.modified_files, -- multi-file: list<path>; single-file: nil
        iters = res.iters,
        summary = res.summary,
        failure_reason = res.failure_reason,
        last_error = res.last_error,
        -- code:    excluded (Counter WF-A defence)
        -- history: excluded (circular-ref risk + context contamination)
    }
end

-- ============================================================
-- Multi-file lazy-load: tool spec + handler
-- ============================================================

-- Maximum number of tool calls (read_file / read_file_range / fs_edit)
-- allowed within a single iteration.
-- Prevents infinite tool-use loops when the child LLM re-requests the same file.
-- Raised from 8 when the write tool joined the tool set: write calls consume
-- budget too (one call per edit), so an agentic model needs headroom for
-- read → edit × N → done within one iter.
local MAX_TOOL_CALLS_PER_ITER = 16

-- ── Distill / cache constants (added ST1) ───────────────────────────────────
-- Files with content length >= this threshold trigger the distill subloop (ST2-3).
-- Below threshold: full content returned verbatim (unchanged behaviour).
local READ_FILE_FULL_THRESHOLD = 10000 -- chars

-- Lines per chunk fed to the distill LLM in one call.
local DISTILL_CHUNK_LINES = 200

-- Maximum chars for the aggregate digest returned by read_file after distillation.
local DISTILL_DIGEST_MAX_CHARS = 4000

-- Maximum chars for a single chunk's contribution to the aggregate digest.
local DISTILL_CHUNK_DIGEST_MAX_CHARS = 400

-- TTL seconds for file_digest cache entries in "auto" refresh mode.
local CACHE_AUTO_TTL_SEC = 10

-- Maximum line span allowed in a single read_file_range call.
local READ_FILE_RANGE_MAX_LINES = 500
-- ── end distill / cache constants ───────────────────────────────────────────

-- Tool spec for the child LLM (multi-file branch only).
-- Passed as opts.tools in llm_call; never exposed to the parent agent layer.
local READ_FILE_TOOL = {
    name = "read_file",
    description = "Read the current content of a target file. "
        .. "For files <= READ_FILE_FULL_THRESHOLD bytes, returns full content. "
        .. "For larger files, returns a distilled digest with line index hints; "
        .. "use read_file_range to fetch verbatim ranges as needed.",
    input_schema = {
        type = "object",
        required = { "path" },
        properties = {
            path = {
                type = "string",
                description = "Absolute path. Must be one of the target_files paths provided in the spec.",
            },
        },
    },
}

-- Tool spec for verbatim line-range retrieval (multi-file branch only).
-- Allows the child LLM to fetch exact source lines after read_file returns a digest.
local READ_FILE_RANGE_TOOL = {
    name = "read_file_range",
    description = "Read a verbatim line range of a target file. "
        .. "Use this after read_file returned a distilled digest, to fetch a specific section. "
        .. "1-indexed, inclusive; line_end - line_start + 1 must be <= READ_FILE_RANGE_MAX_LINES.",
    input_schema = {
        type = "object",
        required = { "path", "line_start", "line_end" },
        properties = {
            path = {
                type = "string",
                description = "Absolute path. Must be in target_files.",
            },
            line_start = {
                type = "integer",
                description = "1-indexed start line, inclusive.",
            },
            line_end = {
                type = "integer",
                description = "1-indexed end line, inclusive.",
            },
        },
    },
}

-- ── ST2: cache lifecycle helpers ────────────────────────────────────────────

-- Return the mtime of a file as a number.
-- Tries std.fs.metadata(path).modified first (mlua-batteries fs feature).
-- Falls back to os.time() if the metadata call is unavailable or returns nil.
-- Fallback behaviour: within the same iter every call returns os.time() which is
-- nearly identical (same-second), so cache will hit for repeated reads within an
-- iter; across iter boundaries the TTL-based "auto" mode governs expiry.
local function file_mtime(path)
    local ok, meta = pcall(function()
        return std.fs.metadata(path)
    end)
    if ok and meta and meta.modified then
        return meta.modified
    end
    -- Fallback: os.time() — same-iter reads get near-identical timestamps,
    -- so auto-mode cache will hit within a single iter. Across iters the TTL
    -- (CACHE_AUTO_TTL_SEC) governs whether the cache is considered fresh.
    return os.time()
end

-- Determine whether the cached digest for a path is still valid.
-- cached:        mf_state.file_digest[path] entry (or nil if not yet cached)
-- cur_mtime:     current file mtime (number from file_mtime)
-- refresh_mode:  "auto" | "always" | "files" | "manual"
--
-- Returns true  → use cache (no distill call needed)
--         false → cache miss or forced refresh (call distill_subloop)
local function should_use_cache(cached, cur_mtime, refresh_mode)
    if cached == nil then
        return false
    end
    if refresh_mode == "always" then
        return false
    end
    if refresh_mode == "manual" then
        return true
    end -- mtime ignored
    local mtime_match = (cached.mtime == cur_mtime)
    if refresh_mode == "files" then
        return mtime_match
    end
    -- "auto": mtime match AND within TTL window
    return mtime_match and (os.time() - cached.cached_at) < CACHE_AUTO_TTL_SEC
end

-- Format a cached digest entry into an LLM-readable text block.
-- cached: { digest=string, line_index=string, mtime=number, cached_at=number }
-- Returns a formatted string combining the digest and the line index.
local function format_digest_response(cached)
    local parts = {}
    table.insert(parts, "[Distilled digest]\n" .. tostring(cached.digest or ""))
    if cached.line_index and cached.line_index ~= "" then
        table.insert(parts, "\n[Line index]\n" .. tostring(cached.line_index))
    end
    table.insert(parts, "\n[Use read_file_range to fetch verbatim line ranges.]")
    return table.concat(parts, "")
end

-- Return the first READ_FILE_FULL_THRESHOLD chars of content with a warning suffix.
-- Used as a fallback when distill_subloop fails — compile_loop continues rather than
-- aborting (Phase 3b error design: handler returns content, never {ok=false}).
-- err: optional error string from distill_subloop (may be nil)
local function truncate_with_warning(content, err)
    local head = content:sub(1, READ_FILE_FULL_THRESHOLD)
    local warn = "\n\n[WARNING: file exceeded size threshold; content truncated"
    if err and err ~= "" then
        warn = warn .. " (distill error: " .. tostring(err) .. ")"
    end
    warn = warn .. "]"
    return head .. warn
end

-- ── ST3: distill subloop helpers ────────────────────────────────────────────

-- Prompt template for the distill LLM call.
-- Placeholder order (8 args, AC#8): path, chunk_start, chunk_end, total_lines,
--   last_err, target_func, chunk_text, DISTILL_CHUNK_DIGEST_MAX_CHARS
local DISTILL_CHUNK_PROMPT = "You are summarizing a chunk of a source code file for a coding assistant.\n"
    .. "Your summary will be used as a digest that lets the assistant understand the code\n"
    .. "without seeing the full file.\n\n"
    .. "File: %s\n"
    .. "Chunk: lines %d-%d (of %d total)\n"
    .. "Recent build error (if any): %s\n"
    .. "Target function (if any): %s\n\n"
    .. "Code chunk:\n"
    .. "```\n%s\n```\n\n"
    .. "Instructions:\n"
    .. "- Write a concise technical summary of what this chunk defines and does.\n"
    .. "- Emphasize any definitions, exports, or logic relevant to the build error or target function.\n"
    .. "- Include key function/class/variable names so the assistant can ask for specific lines.\n"
    .. "- Keep the summary under %d characters.\n"
    .. "- Output ONLY the summary text, no preamble."

-- Split a string into a list of lines (no trailing newline in each entry).
-- Returns a table of strings, 1-indexed.
local function split_lines(content)
    local lines = {}
    for line in (content .. "\n"):gmatch("([^\n]*)\n") do
        table.insert(lines, line)
    end
    return lines
end

-- Split lines into chunks of at most chunk_size lines.
-- Applies boundary adjustment: after computing the natural chunk end, scans up to
-- +20 lines ahead for a line matching a function-definition prefix
-- (^function / ^local function / ^def / ^fn ).  If found at index i, the chunk
-- is extended to i-1 (just before the definition) to avoid mid-function splits.
--
-- Returns: { {start=N, end_=M, total_lines=T, text="..."}, ... }
--   - start / end_ are 1-indexed, inclusive
--   - total_lines is #lines (same for every chunk; used as prompt context)
local function chunk_by_lines(lines, chunk_size)
    local chunks = {}
    local total = #lines
    local i = 1
    while i <= total do
        local natural_end = math.min(i + chunk_size - 1, total)
        local adjusted_end = natural_end
        -- Boundary adjustment: scan up to +20 lines ahead for a function start.
        if natural_end < total then
            local scan_limit = math.min(natural_end + 20, total)
            for j = natural_end + 1, scan_limit do
                local line = lines[j]
                if
                    line:match("^function ")
                    or line:match("^local function ")
                    or line:match("^def ")
                    or line:match("^fn ")
                then
                    -- Extend chunk to end just before this definition line.
                    adjusted_end = j - 1
                    break
                end
            end
        end
        -- Build chunk text.
        local chunk_lines = {}
        for k = i, adjusted_end do
            table.insert(chunk_lines, lines[k])
        end
        table.insert(chunks, {
            start = i,
            end_ = adjusted_end,
            total_lines = total,
            text = table.concat(chunk_lines, "\n"),
        })
        i = adjusted_end + 1
    end
    return chunks
end

-- Extract the text content from an llm_call response.
-- Both providers land on the same internal shape, so this reads one place.
-- Returns the content string or nil on any access failure.
local function extract_text(resp)
    if not resp then
        return nil
    end
    local choices = resp.choices
    if not choices or not choices[1] then
        return nil
    end
    local msg = choices[1].message
    if not msg then
        return nil
    end
    return msg.content -- string for both providers when tools=nil
end

-- Call the distill LLM for a single chunk.
-- Uses conf.provider (provider-agnostic — crux-card §2 must_not_simplify).
-- Never passes tools → raw text response (no tool_use schema in distill path).
-- Returns digest_string on success, nil on any failure (caller handles fallback).
local function call_distill_llm(path, chunk, mf_state, conf)
    -- Build distill conf — inherit provider/model/base_url/api_key from outer conf.
    -- No 'tools' key → llm_call treats tools as nil → raw text response.
    local distill_conf = {
        provider = conf.provider,
        model = conf.model,
        base_url = conf.base_url,
        api_key = conf.api_key,
        api_key_env = conf.api_key_env,
        -- Same endpoint settings as the main loop. Without these the distill
        -- call silently ran at the provider defaults — thinking back on, the
        -- env temperature, and the 120s timeout.
        max_tokens = conf.max_tokens,
        temperature = conf.temperature,
        timeout = conf.timeout,
        disable_thinking = conf.disable_thinking,
        thinking = conf.thinking,
        dialect = conf.dialect,
    }

    -- Resolve target_func with type guard (subtask-3.md Constraint / Risk).
    local target_func_str = "(none)"
    if conf.target_func and type(conf.target_func) == "string" then
        target_func_str = conf.target_func
    end

    local prompt = string.format(
        DISTILL_CHUNK_PROMPT,
        path,
        chunk.start,
        chunk.end_,
        chunk.total_lines,
        mf_state.last_err or "(none)",
        target_func_str,
        chunk.text,
        DISTILL_CHUNK_DIGEST_MAX_CHARS
    )

    local messages = {
        { role = "user", content = prompt },
    }

    local resp, call_err = llm_call(distill_conf, messages) -- luacheck: ignore call_err
    if not resp then
        return nil
    end

    local text = extract_text(resp)
    return text -- may be nil if response shape is unexpected
end

-- Pack chunk digests into a single string that fits within max_chars.
-- chunk_digests: list of { start=N, end_=M, digest=string }  (already priority-sorted)
-- max_chars:     upper bound (DISTILL_DIGEST_MAX_CHARS)
-- tolerance:     allowed undershoot fraction (Aider repomap.py:568-591, default 0.15)
--
-- Algorithm:
--   1. If total length ≤ max_chars → include all (no binary search needed).
--   2. Otherwise binary-search for the largest K such that
--      sum(digests[1..K]) ≤ max_chars.
--      (tolerance is used to check whether we are in the acceptable window
--       max_chars*(1-tolerance) ≤ sum ≤ max_chars; if the best K already
--       satisfies this we stop early.)
--   3. Restore original order (sort by .start) before concatenating.
-- Returns: concatenated digest string (may be "" if every individual chunk
--          exceeds max_chars — caller's truncate_with_warning handles this).
local function binary_search_pack(chunk_digests, max_chars, tolerance)
    tolerance = tolerance or 0.15
    if #chunk_digests == 0 then
        return ""
    end

    -- Compute cumulative lengths.
    local total_len = 0
    for _, cd in ipairs(chunk_digests) do
        total_len = total_len + #(cd.digest or "")
    end

    local selected
    if total_len <= max_chars then
        -- All fit — take everything.
        selected = {}
        for _, cd in ipairs(chunk_digests) do
            table.insert(selected, cd)
        end
    else
        -- Binary search for largest K that fits.
        local lo, hi = 0, #chunk_digests
        local best_k = 0
        local lower_bound = max_chars * (1 - tolerance)
        while lo <= hi do
            local mid = math.floor((lo + hi) / 2)
            local sum = 0
            for k = 1, mid do
                sum = sum + #(chunk_digests[k].digest or "")
            end
            if sum <= max_chars then
                best_k = mid
                if sum >= lower_bound then
                    -- Within acceptable window — stop early (Aider tolerance logic).
                    break
                end
                lo = mid + 1
            else
                hi = mid - 1
            end
        end
        -- Collect top-K.
        selected = {}
        for k = 1, best_k do
            table.insert(selected, chunk_digests[k])
        end
    end

    -- Restore original order (sort by .start ascending).
    table.sort(selected, function(a, b)
        return a.start < b.start
    end)

    -- Concatenate digests.
    local parts = {}
    for _, cd in ipairs(selected) do
        table.insert(parts, cd.digest or "")
    end
    return table.concat(parts, "\n")
end

-- Build a line-index string from a list of chunk digest entries.
-- Each entry: { start=N, end_=M, digest=string }
-- Format: "L1-50: <first non-empty line of digest, max 80 chars>\n..."
local function build_line_index(chunk_digests)
    local lines = {}
    for _, cd in ipairs(chunk_digests) do
        -- First non-empty line of the digest.
        local first_line = ""
        for line in (tostring(cd.digest or "") .. "\n"):gmatch("([^\n]*)\n") do
            if line ~= "" then
                first_line = line
                break
            end
        end
        if #first_line > 80 then
            first_line = first_line:sub(1, 80)
        end
        table.insert(lines, "L" .. cd.start .. "-" .. cd.end_ .. ": " .. first_line)
    end
    return table.concat(lines, "\n")
end

-- Distill subloop — real implementation (ST3).
-- Replaces the ST2 stub.
--
-- Signature: path, content, mf_state, conf → digest, line_index, err_string
--   err_string non-nil means failure; caller should invoke truncate_with_warning.
--
-- Steps:
--   1. Split content into chunks (chunk_by_lines).
--   2. For each chunk, call call_distill_llm → collect {start, end_, digest}.
--      Chunk with no digest (LLM failure) is skipped; if ALL fail → err_string.
--   3. Priority-sort chunk_digests for binary_search_pack:
--      (1) chunks whose range overlaps last_err line (if any)
--      (2) chunks containing conf.target_func string (if non-nil string)
--      (3) original order
--   4. binary_search_pack → digest string.
--   5. build_line_index → line_index string.
--
-- Module-level override for test injection (M._test_set_distill_subloop).
local _distill_subloop_override = nil

local function distill_subloop(path, content, mf_state, conf)
    if _distill_subloop_override then
        return _distill_subloop_override(path, content, mf_state, conf)
    end

    -- 1. Split and chunk.
    local lines = split_lines(content)
    local chunks = chunk_by_lines(lines, DISTILL_CHUNK_LINES)

    -- 2. Distill each chunk via LLM.
    local chunk_digests = {}
    for _, chunk in ipairs(chunks) do
        local digest = call_distill_llm(path, chunk, mf_state, conf)
        if digest then
            table.insert(chunk_digests, {
                start = chunk.start,
                end_ = chunk.end_,
                digest = digest,
            })
        end
        -- Chunks with nil digest are silently skipped; if all fail we handle below.
    end

    if #chunk_digests == 0 then
        return nil, nil, "distill_subloop: all chunks failed (no LLM response)"
    end

    -- 3. Priority-sort for binary_search_pack.
    -- Extract the error line number from mf_state.last_err (path:line or path:line:col).
    local err_line = nil
    if mf_state.last_err then
        local m = mf_state.last_err:match(":(%d+)")
        if m then
            err_line = tonumber(m)
        end
    end

    local target_func = nil
    if conf and conf.target_func and type(conf.target_func) == "string" then
        target_func = conf.target_func
    end

    -- Assign priority to each chunk digest.
    local function chunk_priority(cd)
        -- Priority 1: overlaps err_line.
        if err_line and cd.start <= err_line and err_line <= cd.end_ then
            return 1
        end
        -- Priority 2: contains target_func string.
        if target_func and cd.digest:find(target_func, 1, true) then
            return 2
        end
        -- Priority 3: original order (handled by stable secondary sort below).
        return 3
    end

    -- Stable sort: primary = priority, secondary = original index (position in table).
    local indexed = {}
    for idx, cd in ipairs(chunk_digests) do
        table.insert(indexed, { cd = cd, prio = chunk_priority(cd), orig = idx })
    end
    table.sort(indexed, function(a, b)
        if a.prio ~= b.prio then
            return a.prio < b.prio
        end
        return a.orig < b.orig
    end)

    -- Rebuild sorted list for binary_search_pack.
    local sorted_digests = {}
    for _, entry in ipairs(indexed) do
        table.insert(sorted_digests, entry.cd)
    end

    -- 4. Pack into budget.
    local digest = binary_search_pack(sorted_digests, DISTILL_DIGEST_MAX_CHARS, 0.15)

    -- 5. Build line index (using original order for readability).
    local line_index = build_line_index(chunk_digests)

    return digest, line_index, nil
end

-- Handle a read_file_range tool call from the child LLM.
-- Returns verbatim lines [line_start, line_end] (1-indexed, inclusive) from path.
-- NEVER passes through distillation — verbatim access is guaranteed regardless of
-- file size (crux-card §3 must_not_simplify: verbatim range access after distill).
-- Returns {ok=true, content=string} or {ok=false, error=string}.
local function read_file_range_tool_handler(path, line_start, line_end, target_files_set)
    -- Allowlist check
    if not target_files_set[path] then
        return { ok = false, error = "path '" .. tostring(path) .. "' not in target_files allowlist" }
    end
    -- Type and range validation
    if
        type(line_start) ~= "number"
        or type(line_end) ~= "number"
        or math.floor(line_start) ~= line_start
        or math.floor(line_end) ~= line_end
    then
        return { ok = false, error = "line_start and line_end must be integers" }
    end
    line_start = math.floor(line_start)
    line_end = math.floor(line_end)
    if line_start < 1 or line_end < line_start then
        return { ok = false, error = "invalid range: require 1 <= line_start <= line_end" }
    end
    if (line_end - line_start + 1) > READ_FILE_RANGE_MAX_LINES then
        return {
            ok = false,
            error = string.format(
                "range %d-%d exceeds READ_FILE_RANGE_MAX_LINES=%d",
                line_start,
                line_end,
                READ_FILE_RANGE_MAX_LINES
            ),
        }
    end
    -- Verbatim line read (no distillation)
    local f, open_err = io.open(path, "r")
    if not f then
        return { ok = false, error = "cannot open: " .. tostring(open_err) }
    end
    local lines = {}
    local cur = 0
    for line in f:lines() do
        cur = cur + 1
        if cur >= line_start then
            table.insert(lines, line)
        end
        if cur >= line_end then
            break
        end
    end
    f:close()
    if cur < line_start then
        return {
            ok = false,
            error = string.format("file has %d lines; line_start=%d out of range", cur, line_start),
        }
    end
    return { ok = true, content = table.concat(lines, "\n"), first_line = line_start }
end

-- Prefix each line with its 1-based number.
--
-- fs_edit addresses lines, so the reads that feed it have to say which line
-- is which. Only ever applied to verbatim content on its way to the child LLM
-- (never to the cached copy, and never to a distilled digest, where the
-- numbers would not correspond to the file).
local function with_line_numbers(text, first_line)
    local out = {}
    local n = (first_line or 1) - 1
    for line in (text .. "\n"):gmatch("(.-)\n") do
        n = n + 1
        table.insert(out, string.format("%d\t%s", n, line))
    end
    -- gmatch on text .. "\n" yields one trailing empty element for text that
    -- already ended in a newline; drop it so no phantom line is numbered.
    if #out > 0 and out[#out]:match("^%d+\t$") and text:sub(-1) == "\n" then
        table.remove(out)
    end
    return table.concat(out, "\n")
end
-- ── end ST2: cache lifecycle helpers ────────────────────────────────────────

-- Handle a read_file tool call from the child LLM.
-- Returns {ok=true, content=string} or {ok=false, error=string}.
-- Never raises; errors are propagated as tool_result content so the child LLM
-- can recover (per-iter reset keeps the loop safe).
--
-- ST2: signature extended to (path, target_files_set, mf_state, conf).
-- mf_state and conf may be nil when called from paths that do not yet pass them
-- (guards below ensure backward-safe behaviour).
-- Size branch:
--   content <= READ_FILE_FULL_THRESHOLD → return full content (unchanged behaviour)
--   content >  READ_FILE_FULL_THRESHOLD → run distill_subloop (stub in ST2)
--     cache hit  → return format_digest_response(cached)  [no LLM call]
--     cache miss → call distill_subloop → cache result → format_digest_response
--     distill failure → truncate_with_warning (loop continues)
local function read_file_tool_handler(path, target_files_set, mf_state, conf)
    -- Error messages below are kept verbatim from the original (BC2 regression guard).
    if not target_files_set[path] then
        return { ok = false, error = "path '" .. tostring(path) .. "' not in target_files allowlist" }
    end
    local f, err = io.open(path, "r")
    if not f then
        return { ok = false, error = "cannot open: " .. tostring(err) }
    end
    local content = f:read("*a")
    f:close()
    content = content or ""

    -- Below-threshold: return full content unchanged (AC #2, backward-compat).
    if #content <= READ_FILE_FULL_THRESHOLD then
        return { ok = true, content = content, verbatim = true }
    end

    -- Above-threshold: use distill / cache path.
    -- mf_state guard: if caller did not supply mf_state (legacy path), fall back to truncate.
    if not mf_state or type(mf_state.file_digest) ~= "table" then
        return { ok = true, content = truncate_with_warning(content, nil) }
    end

    local refresh_mode = mf_state.file_digest_refresh or "auto"
    local cur_mtime = file_mtime(path)
    local cached = mf_state.file_digest[path]

    if should_use_cache(cached, cur_mtime, refresh_mode) then
        -- Cache hit: return digest without calling distill_subloop (AC #3).
        return { ok = true, content = format_digest_response(cached) }
    end

    -- Cache miss or forced refresh: call distill_subloop (stub in ST2).
    local digest, line_index, distill_err = distill_subloop(path, content, mf_state, conf)
    if distill_err then
        -- Distill failure: return truncated content with warning; do not abort loop (AC #5).
        return { ok = true, content = truncate_with_warning(content, distill_err) }
    end

    -- Store result in cache (AC #4).
    mf_state.file_digest[path] = {
        digest = digest,
        line_index = line_index,
        mtime = cur_mtime,
        cached_at = os.time(),
    }

    return { ok = true, content = format_digest_response(mf_state.file_digest[path]) }
end

-- ============================================================
-- Internal loop body (non-public; called only via make().handler)
-- ============================================================

-- run_loop(conf) executes the structural compile-and-fix loop.
-- conf fields (K-96 full set, all resolved before entry):
--   runner, lang, target_files (list<abs_path>), multi_file (bool), spec,
--   max_iters, system, edit_mode, tool_mode ("auto"|"read_only"|"none"),
--   provider, base_url, api_key, api_key_env, model,
--   max_tokens, temperature, disable_thinking, timeout,
--   on_iter (optional callback)
--
-- For backward compatibility, single-file callers pass conf.target_files = {abs_path}
-- and conf.multi_file = false. The handler normalizes before calling run_loop.
local function run_loop(conf)
    assert(type(conf) == "table", "conf table required")
    assert(conf.target_files and #conf.target_files > 0, "conf.target_files (non-empty list) required")
    assert(conf.spec, "conf.spec required")
    assert(type(conf.runner) == "function", "conf.runner (function) required")

    local lang = conf.lang or "lua"
    local max_iters = conf.max_iters or 5
    local multi_file = conf.multi_file or false
    -- tool_mode governs which tools diff mode declares:
    --   "auto" (default): read_file + read_file_range + fs_edit
    --   "read_only":      reads only — can inspect, cannot converge
    --
    -- "none" and "adaptive" are gone with the SEARCH/REPLACE text channel they
    -- depended on: "none" meant "no tools, edit through text", and "adaptive"
    -- meant "fall back to that text channel when the tools stall". Without a
    -- text channel there is nothing to fall back to, and a diff run with no
    -- edit tool cannot change a file at all. Callers that need a no-tools path
    -- want edit_mode = "full".
    local tool_mode = conf.tool_mode or "auto"
    local active_tool_mode = tool_mode
    -- Cached: the dump mode is a single process-wide fact, and the cache also
    -- keeps the prod-downgrade warn to at most one line per process.
    local mode = resolve_dump_mode_cached()

    -- In single-file mode, artifact_path is the single absolute path (backward compat).
    -- In multi-file mode, artifact_path is nil; modified_files carries the list.
    local artifact_path = (not multi_file) and conf.target_files[1] or nil

    -- Build a set for fast path-header validation in parse_search_replace.
    local target_files_set = {}
    for _, p in ipairs(conf.target_files) do
        target_files_set[p] = true
    end

    -- Write channel: std.fs owns the edit semantics, scoped to this run's
    -- target files. `tool_specs` rather than `register_tools` — the registry is
    -- global and this lock is per-invocation.
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

    -- Path list shown to the model each iteration; diff mode assembles the rest
    -- of its prompt per iteration from the last verify error.
    local path_lines = {}
    for _, p in ipairs(conf.target_files) do
        table.insert(path_lines, "  " .. p)
    end

    -- Full mode embeds the file and asks for the whole thing back.
    local single_initial_user_content
    if edit_mode == "full" then
        local existing = existing_map[conf.target_files[1]]
        if existing then
            single_initial_user_content = conf.spec
                .. "\n\n=== Current file content ===\n```"
                .. lang
                .. "\n"
                .. existing
                .. "\n```"
        else
            single_initial_user_content = conf.spec
        end
    end

    -- ── Per-iter state for multi-file lazy-load ─────────────────────────────────
    -- messages[] is rebuilt each iter from state; not accumulated across iters.
    -- sr_history is reserved for subtask 2 (stagnation_v2); initialized empty here.
    local mf_state = {
        iter = 0,
        last_err = nil, -- most recent verify failure stderr (≤2,000 chars)
        sr_digest_prev = nil, -- digest of last SR block (≤500 chars)
        sr_history = {}, -- populated in subtask 2
        -- ST1: per-iter-reset-surviving file digest cache (crux-card §1).
        -- Keyed by absolute path; each entry: { digest, line_index, mtime, cached_at }.
        -- Must NOT be cleared or overwritten in the per-iter rebuild path (L1149-1170).
        file_digest = {},
        -- Refresh policy for file_digest cache ("auto" uses CACHE_AUTO_TTL_SEC).
        file_digest_refresh = "auto",
        -- Accumulates paths that were successfully written by iterate_files across iters.
        -- Used to populate modified_files on every return path (crux §3).
        modified_set = {},
    }
    assert(type(mf_state.sr_history) == "table", "mf_state.sr_history must be initialized")
    assert(type(mf_state.file_digest) == "table", "mf_state.file_digest must be initialized")
    assert(mf_state.file_digest_refresh == "auto", "mf_state.file_digest_refresh must default to 'auto'")
    assert(type(mf_state.modified_set) == "table", "mf_state.modified_set must be initialized")

    -- ── Tool set handed to tool_loop ────────────────────────────────────────
    --
    -- std.fs owns the edit; this wrapper owns what an edit means to the loop
    -- (which files changed, which caches are now stale, whether the iteration
    -- made progress). The build is deliberately not among these: a tool the
    -- model can decline to call cannot carry the "it compiles" guarantee.
    -- Which cached reads are the file verbatim (vs a distilled digest);
    -- only the verbatim ones may be line-numbered for fs_edit.
    local verbatim_reads = {}
    local edit_count = 0
    local iter_edit_sigs = {}
    local iter_edit_errors = {}

    -- The tool set is built once, but its calls belong to an iteration. The
    -- loop below advances this cursor so each obs line can say which one —
    -- without it, a run with max_iters > 1 logs reads and edits that cannot be
    -- attributed to the iteration that made them. Iterations are sequential,
    -- so a single cursor is exact.
    local current_iter = 0

    local fs_edit_spec = std.fs.tool_specs({ allowed = { "edit" }, path_lock = conf.target_files })[1]
    local raw_fs_edit = fs_edit_spec.handler

    local function on_edit(input)
        local path = input.path or ""
        local res = raw_fs_edit(input)
        if res.ok then
            edit_count = edit_count + 1
            mf_state.modified_set[path] = true
            -- Both views of the file predate the write.
            existing_map[path] = nil
            mf_state.file_digest[path] = nil
            -- Signature by resulting version: two different edits that land on
            -- the same content are the same lack of progress.
            table.insert(iter_edit_sigs, path .. "\1" .. tostring(res.version))
            local ranges, removed, added = summarize_edits(input.edits)
            obs_event(mode, "tool_use", {
                { "iter", current_iter },
                { "path", path },
                { "tool", "fs_edit" },
                { "ok", true },
                { "ranges", ranges },
                { "lines_removed", removed },
                { "lines_added", added },
            })
            return "applied " .. tostring(res.applied) .. " edit(s) to " .. path
        end

        local detail = res.reason or "edit rejected"
        if res.reason == "expect_mismatch" then
            detail = "expect did not match lines "
                .. tostring(res.start_line)
                .. "-"
                .. tostring(res.end_line)
                .. "; those lines currently contain:\n"
                .. tostring(res.actual)
        elseif res.reason == "stale_base" then
            detail = path .. " changed since you read it; re-read and retry"
        elseif res.reason == "out_of_range" then
            detail = "line "
                .. tostring(res.end_line)
                .. " is past the end of the file ("
                .. tostring(res.file_lines)
                .. " lines)"
        elseif res.reason == "path_not_allowed" then
            detail = "path '" .. path .. "' is not one of the target files"
        end
        table.insert(iter_edit_errors, detail)
        -- A rejected batch changes nothing, so there are no magnitudes to
        -- report — but where it aimed is what separates an edit that was
        -- refused from one that landed somewhere other than the lines meant.
        local attempted = summarize_edits(input.edits)
        obs_event(mode, "tool_use_fail", {
            { "iter", current_iter },
            { "path", path },
            { "tool", "fs_edit" },
            { "err", res.reason },
            { "ranges", attempted },
        })
        return "ERROR: " .. detail
    end

    -- read_file keeps compile_loop's own handler: it is not a plain read but
    -- the loop's context management (size branch, digest cache, distill).
    local function on_read(input)
        local path = input.path or ""
        local cached = existing_map[path]
        local res
        if cached ~= nil then
            res = { ok = true, content = cached, verbatim = verbatim_reads[path] }
        else
            res = read_file_tool_handler(path, target_files_set, mf_state, conf)
            if res.ok then
                existing_map[path] = res.content
                verbatim_reads[path] = res.verbatim
            end
        end
        if not res.ok then
            obs_event(
                mode,
                "tool_use_fail",
                { { "iter", current_iter }, { "path", path }, { "tool", READ_FILE_TOOL.name }, { "err", res.error } }
            )
            return "ERROR: " .. tostring(res.error)
        end
        obs_event(mode, "tool_use", {
            { "iter", current_iter },
            { "path", path },
            { "tool", READ_FILE_TOOL.name },
            { "ok", true },
            -- Served from this iteration's cache rather than re-read from disk.
            { "cached", cached ~= nil },
        })
        -- Numbered only when it is the file itself; a digest has no line
        -- correspondence, and numbering it would invite edits to nowhere.
        if res.verbatim then
            return with_line_numbers(res.content, 1)
        end
        return res.content
    end

    local function on_read_range(input)
        local path = input.path or ""
        local res = read_file_range_tool_handler(path, input.line_start, input.line_end, target_files_set)
        if not res.ok then
            obs_event(
                mode,
                "tool_use_fail",
                {
                    { "iter", current_iter },
                    { "path", path },
                    { "tool", READ_FILE_RANGE_TOOL.name },
                    { "err", res.error },
                }
            )
            return "ERROR: " .. tostring(res.error)
        end
        obs_event(mode, "tool_use", {
            { "iter", current_iter },
            { "path", path },
            { "tool", READ_FILE_RANGE_TOOL.name },
            { "ok", true },
        })
        return with_line_numbers(res.content, res.first_line)
    end

    local edit_tools = {
        {
            name = READ_FILE_TOOL.name,
            description = READ_FILE_TOOL.description,
            input_schema = READ_FILE_TOOL.input_schema,
            handler = on_read,
        },
        {
            name = READ_FILE_RANGE_TOOL.name,
            description = READ_FILE_RANGE_TOOL.description,
            input_schema = READ_FILE_RANGE_TOOL.input_schema,
            handler = on_read_range,
        },
        {
            name = fs_edit_spec.name,
            description = fs_edit_spec.description,
            input_schema = fs_edit_spec.input_schema,
            handler = on_edit,
        },
    }

    -- read_only withholds the write tool. It cannot converge on its own, so it
    -- is for inspecting what the model would do, not for fixing.
    local read_only_tools = { edit_tools[1], edit_tools[2] }

    -- Caller-registered tools ride along with both sets: they are declared
    -- whenever any tool is. Their calls never count as edits — by contract they
    -- are read-like, and counting them would let a loop that only queried the
    -- caller's tool look like it was making progress.
    for _, t in ipairs(conf.extra_tools or {}) do
        local schema = t.schema or {}
        local spec = {
            name = t.name,
            description = schema.description or "",
            input_schema = schema.input_schema or { type = "object", properties = {} },
            handler = function(input)
                local ok, res = pcall(t.handler, input or {})
                if not ok then
                    obs_event(
                        mode,
                        "tool_use_fail",
                        { { "iter", current_iter }, { "tool", t.name }, { "err", tostring(res) } }
                    )
                    return "ERROR: " .. tostring(res)
                end
                obs_event(
                    mode,
                    "tool_use",
                    { { "iter", current_iter }, { "tool", t.name }, { "ok", true }, { "extra", true } }
                )
                return res
            end,
        }
        table.insert(edit_tools, spec)
        table.insert(read_only_tools, spec)
    end

    -- Full mode accumulates one conversation across iters; diff mode's turns
    -- live inside tool_loop and do not survive the iteration.
    local messages
    if edit_mode == "full" then
        messages = {
            { role = "system", content = system },
            { role = "user", content = single_initial_user_content },
        }
    end

    local history = {}
    -- bad_stagnation_count: counts consecutive iters where the LLM produced zero successful edits.
    -- Reset to 0 whenever at least one edit applies (good iter). When it reaches STAGNATION_WINDOW,
    -- the loop terminates with failure_reason = "no_edits_applied".
    local bad_stagnation_count = 0

    for iter = 1, max_iters do
        current_iter = iter
        local obs_target = artifact_path or table.concat(conf.target_files, ",")
        obs_event(mode, "iter_start", { { "iter", iter }, { "target_file", obs_target } })

        -- ── diff mode ──────────────────────────────────────────────────────────
        --
        -- tool_loop does the read/edit turns with exactly the tools above; the
        -- build is this loop's own step. That split is the guarantee
        -- compile_loop sells: "it compiles" has to be something the loop
        -- verified, so the runner is never a tool the model can skip.
        if edit_mode == "diff" then
            local edits_before = edit_count
            iter_edit_sigs = {}
            iter_edit_errors = {}
            -- The per-iter read cache: files change under it between iters.
            existing_map = {}
            verbatim_reads = {}
            mf_state.iter = iter

            local prompt_parts = { conf.spec, "\n\nFiles:\n" .. table.concat(path_lines, "\n") }
            if mf_state.last_err and mf_state.last_err ~= "" then
                table.insert(prompt_parts, "\n\n=== Last verify error (trimmed) ===\n" .. mf_state.last_err)
            end

            local tl = tool_loop.run({
                prompt = table.concat(prompt_parts, ""),
                system = system,
                tools = active_tool_mode == "read_only" and read_only_tools or edit_tools,
                max_turns = MAX_TOOL_CALLS_PER_ITER,
                llm = {
                    provider = conf.provider,
                    base_url = conf.base_url,
                    api_key = conf.api_key,
                    api_key_env = conf.api_key_env,
                    model = conf.model,
                    max_tokens = conf.max_tokens,
                    temperature = conf.temperature,
                    thinking = conf.thinking,
                    dialect = conf.dialect,
                    tool_choice = conf.tool_choice,
                    timeout = conf.timeout,
                },
            })

            local iter_edits_applied = edit_count - edits_before
            local content = tl.content or ""

            -- A transport / protocol failure is not the model failing to edit;
            -- it ends the run rather than counting as a stagnant iteration.
            if
                not tl.ok
                and iter_edits_applied == 0
                and tl.error
                and not tostring(tl.error):find("max_turns", 1, true)
            then
                return {
                    ok = false,
                    failure_reason = "llm_call",
                    last_error = tostring(tl.error):sub(-800),
                    iters = iter - 1,
                    summary = make_summary(false, iter - 1, max_iters, "llm_call"),
                    artifact_path = artifact_path,
                    modified_files = collect_modified_paths(mf_state.modified_set),
                    history = history,
                }
            end

            -- Nothing reached disk: the build would report what it already
            -- reported, so feed the tool errors back instead of re-running it.
            if iter_edits_applied == 0 then
                bad_stagnation_count = bad_stagnation_count + 1
                local zero_msg = "No edits were applied."
                if #iter_edit_errors > 0 then
                    zero_msg = zero_msg .. " Every fs_edit call was rejected:\n" .. table.concat(iter_edit_errors, "\n")
                end
                zero_msg = zero_msg
                    .. "\nRe-read the file (read_file output is line-numbered) and retry with an exact expect."
                local entry = {
                    iter = iter,
                    code = nil,
                    result = { ok = false, stderr = zero_msg, stdout = "", exit_code = -1 },
                    raw = content,
                }
                table.insert(history, entry)
                obs_event(mode, "iter_result", {
                    { "iter", iter },
                    { "ok", false },
                    { "exit_code", -1 },
                    { "stderr_len", #zero_msg },
                })
                if conf.on_iter then
                    local cb_ok, cb_err = pcall(conf.on_iter, entry)
                    if not cb_ok then
                        log.warn("compile_loop: on_iter callback error: " .. tostring(cb_err))
                    end
                end
                update_state(mf_state, {
                    last_err = zero_msg,
                    sr_hash_append = compute_sr_hash("<no_edits:" .. tostring(iter) .. ">"),
                })
                if bad_stagnation_count >= STAGNATION_WINDOW then
                    obs_event(mode, "bad_stagnation_blocked", {
                        { "iter", iter },
                        { "reason", "no_edits_applied" },
                    })
                    return {
                        ok = false,
                        failure_reason = "no_edits_applied",
                        last_error = mf_state.last_err or "",
                        iters = iter,
                        summary = make_summary(false, iter, max_iters, "no_edits_applied"),
                        artifact_path = artifact_path,
                        modified_files = collect_modified_paths(mf_state.modified_set),
                        history = history,
                    }
                end
                goto iter_continue
            end

            bad_stagnation_count = 0

            -- Verify. The loop's step, not the model's.
            local rr = conf.runner(multi_file and conf.target_files or conf.target_files[1]) or {}
            local entry = { iter = iter, code = nil, result = rr, raw = content }
            table.insert(history, entry)
            obs_event(mode, "iter_result", {
                { "iter", iter },
                { "ok", rr.ok and true or false },
                { "exit_code", rr.exit_code },
                { "stderr_len", #(tostring(rr.stderr or "")) },
            })
            if conf.on_iter then
                local cb_ok, cb_err = pcall(conf.on_iter, entry)
                if not cb_ok then
                    log.warn("compile_loop: on_iter callback error: " .. tostring(cb_err))
                end
            end

            local iter_sig = table.concat(iter_edit_sigs, "\1")
            if rr.ok then
                update_state(mf_state, { sr_hash_append = compute_sr_hash(iter_sig) })
                obs_event(mode, "converged", { { "iters", iter } })
                return {
                    ok = true,
                    artifact_path = artifact_path,
                    modified_files = collect_modified_paths(mf_state.modified_set),
                    iters = iter,
                    summary = make_summary(true, iter, max_iters, nil),
                    history = history,
                }
            end

            update_state(mf_state, {
                last_err = tostring(rr.stderr or ""),
                sr_digest_prev = iter_sig,
                sr_hash_append = compute_sr_hash(iter_sig),
            })
            -- The same edits producing the same failure twice: the model is
            -- circling rather than converging.
            if is_stagnant_v2(mf_state, true) then
                obs_event(mode, "stagnation_v2", { { "iter", iter }, { "reason", "sr_history_repeat" } })
                return {
                    ok = false,
                    failure_reason = "stagnation",
                    last_error = mf_state.last_err or "",
                    iters = iter,
                    summary = make_summary(false, iter, max_iters, "stagnation"),
                    artifact_path = artifact_path,
                    modified_files = collect_modified_paths(mf_state.modified_set),
                    history = history,
                }
            end

        -- ── full mode (default) ────────────────────────────────────────────────
        --
        -- Single-file whole-file rewrite: no tools, one completion per iter.
        -- This is the path for models that cannot call tools at all, so it
        -- talks to the provider directly rather than through tool_loop.
        else
            local single_path = conf.target_files[1]
            local iter_edits_applied = 0

            local resp, call_err = llm_call(conf, messages)
            if not resp then
                return {
                    ok = false,
                    failure_reason = "llm_call",
                    last_error = tostring(call_err):sub(-800),
                    iters = iter - 1,
                    summary = make_summary(false, iter - 1, max_iters, "llm_call"),
                    artifact_path = artifact_path,
                    history = history,
                }
            end
            local content = (((resp.choices or {})[1] or {}).message or {}).content or ""
            local code = extract_code(content, lang)

            -- Full mode: empty code means the LLM produced zero usable edits (bad stagnation).
            -- Non-empty code is always an edit (full-file replace).
            if #code > 0 then
                iter_edits_applied = iter_edits_applied + 1
                bad_stagnation_count = 0
            end

            -- Write target file (full-file replace — next_full_file action)
            local wok, werr = write_file(single_path, code)
            if not wok then
                return {
                    ok = false,
                    failure_reason = "open_target_file",
                    last_error = werr,
                    iters = iter,
                    summary = make_summary(false, iter, max_iters, "open_target_file"),
                    artifact_path = artifact_path,
                    history = history,
                }
            end

            -- Single-file runner call with single string path (Crux #3).
            local rr = conf.runner(single_path) or {}
            local entry = { iter = iter, code = code, result = rr, raw = content }
            table.insert(history, entry)
            obs_event(mode, "iter_result", {
                { "iter", iter },
                { "ok", rr.ok and true or false },
                { "exit_code", rr.exit_code },
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
                    ok = true,
                    code = code,
                    artifact_path = artifact_path,
                    iters = iter,
                    summary = make_summary(true, iter, max_iters, nil),
                    history = history,
                }
            end

            -- Stagnation detection: bad stagnation (empty code = zero edits) takes priority.
            if iter_edits_applied == 0 then
                bad_stagnation_count = bad_stagnation_count + 1
                if bad_stagnation_count >= STAGNATION_WINDOW then
                    local last_stderr = tostring(rr.stderr or ""):sub(-800)
                    obs_event(mode, "bad_stagnation_blocked", {
                        { "iter", iter },
                        { "reason", "no_edits_applied" },
                    })
                    return {
                        ok = false,
                        failure_reason = "no_edits_applied",
                        last_error = last_stderr,
                        code = code,
                        iters = iter,
                        summary = make_summary(false, iter, max_iters, "no_edits_applied"),
                        artifact_path = artifact_path,
                        history = history,
                    }
                end
                -- Inject explicit retry feedback so the LLM knows it must emit edits.
                local retry_msg = "Your previous attempt produced no usable code."
                    .. " Output the whole corrected file in a single fenced code block."
                table.insert(messages, { role = "assistant", content = content })
                table.insert(messages, { role = "user", content = retry_msg })
            elseif is_stagnant(history) then
                local last_stderr = tostring(rr.stderr or ""):sub(-800)
                obs_event(mode, "stagnation", { { "iters", iter } })
                return {
                    ok = false,
                    failure_reason = "stagnation",
                    last_error = last_stderr,
                    code = code,
                    iters = iter,
                    summary = make_summary(false, iter, max_iters, "stagnation"),
                    artifact_path = artifact_path,
                    history = history,
                }
            else
                -- Append assistant + failure user message for the next turn.
                table.insert(messages, { role = "assistant", content = content })
                table.insert(messages, { role = "user", content = build_failure_msg(lang, rr) })
            end
        end
        -- ── end of edit_mode branch ────────────────────────────────────────────
        ::iter_continue::
    end

    -- max_iters reached without PASS
    local last = history[#history] or {}
    local last_stderr = tostring((last.result or {}).stderr or ""):sub(-800)
    obs_event(mode, "max_iters_reached", { { "iters", max_iters } })
    local max_iters_result = {
        ok = false,
        failure_reason = "max_iters",
        last_error = last_stderr,
        code = last.code,
        iters = max_iters,
        summary = make_summary(false, max_iters, max_iters, "max_iters"),
        artifact_path = artifact_path,
        history = history,
    }
    -- Preserve modified_files for multi-file branch (single-file uses or {} on consumer side).
    if multi_file then
        max_iters_result.modified_files = collect_modified_paths(mf_state.modified_set)
    end
    return max_iters_result
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
    -- tool_mode (diff mode only):
    --   "auto" (default) declares read_file / read_file_range / fs_edit,
    --   "read_only" declares only the read tools — it can inspect but never
    --   converge, so it is a dry run rather than a fix.
    local tool_mode = conf.tool_mode or "auto"
    assert(
        tool_mode == "auto" or tool_mode == "read_only",
        "conf.tool_mode must be 'auto' or 'read_only' (use edit_mode='full' for a no-tools run)"
    )

    -- extra_tools (optional, multi-file only): caller-registered tools in the
    -- agent-layer nested form {name, schema = {description?, input_schema}, handler}.
    -- Declared alongside the built-in tools; dispatched inside the tool loop.
    -- Built-in names are reserved.
    local RESERVED_TOOL_NAMES = { read_file = true, read_file_range = true, fs_edit = true }
    if conf.extra_tools ~= nil then
        assert(type(conf.extra_tools) == "table", "conf.extra_tools must be a list")
        for i, t in ipairs(conf.extra_tools) do
            assert(type(t) == "table", "conf.extra_tools[" .. i .. "] must be a table")
            assert(
                type(t.name) == "string" and t.name ~= "",
                "conf.extra_tools[" .. i .. "].name must be a non-empty string"
            )
            assert(
                not RESERVED_TOOL_NAMES[t.name],
                "conf.extra_tools[" .. i .. "].name '" .. t.name .. "' is reserved (built-in tool)"
            )
            assert(type(t.handler) == "function", "conf.extra_tools[" .. i .. "].handler must be a function")
            assert(
                t.schema == nil or type(t.schema) == "table",
                "conf.extra_tools[" .. i .. "].schema must be a table when present"
            )
        end
    end

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
            type = "object",
            required = { "spec" },
            properties = {
                spec = {
                    type = "string",
                    description = "Full specification the child LLM must satisfy.",
                },
                target_file = {
                    type = "string",
                    description = "Absolute path of the file (single-file mode). Read on entry if it already exists, then written on each iteration. Mutually exclusive with target_files.",
                },
                target_files = {
                    type = "array",
                    items = { type = "string" },
                    description = "Array of absolute paths (multi-file mode). Mutually exclusive with target_file. Multi-file mode requires edit_mode=diff.",
                },
                lang = {
                    type = "string",
                    description = "Code fence language label (default: lua).",
                },
            },
        },
    }

    local function handler(input)
        -- Crux #2: target_file and target_files are mutually exclusive.
        assert(not (input.target_file and input.target_files), "target_file and target_files are mutually exclusive")
        -- At least one must be provided.
        assert(input.target_file or input.target_files, "target_file (string) or target_files (array) is required")

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
        assert(not (multi_file and effective_edit_mode == "full"), "multi-file mode requires edit_mode=diff")

        -- Resolve LLM fields at call time.
        -- Priority: conf.llm.<field> → _AGENT_LLM_CTX top → nil (env fallback in llm_call)
        local parent_ctx = agent._llm_ctx_top() or {}
        local llm_conf = conf.llm or {}

        local resolved_conf = {
            -- runner (from factory conf, never from input)
            runner = conf.runner,

            -- tool input fields (normalized)
            lang = input.lang or conf.lang or "lua",
            target_files = files_list, -- internal list (1-element for single-file)
            multi_file = multi_file,
            spec = input.spec,

            -- factory conf fields
            max_iters = conf.max_iters,
            system = conf.system,
            edit_mode = effective_edit_mode,
            tool_mode = tool_mode,
            extra_tools = conf.extra_tools,
            on_iter = conf.on_iter,

            -- LLM fields (K-96 full set, all explicit):
            provider = llm_conf.provider or parent_ctx.provider,
            base_url = llm_conf.base_url or parent_ctx.base_url,
            api_key = llm_conf.api_key or parent_ctx.api_key,
            api_key_env = llm_conf.api_key_env or parent_ctx.api_key_env,
            model = llm_conf.model or parent_ctx.model,
            max_tokens = llm_conf.max_tokens,
            temperature = llm_conf.temperature,
            disable_thinking = llm_conf.disable_thinking,
            timeout = llm_conf.timeout,
            -- Everything llm_proto understands, so the loop is not narrower
            -- than the protocol layer it calls.
            thinking = llm_conf.thinking,
            dialect = llm_conf.dialect,
            tool_choice = llm_conf.tool_choice,
            parallel_tool_calls = llm_conf.parallel_tool_calls,
            cache_control = llm_conf.cache_control,
            extra_body = llm_conf.extra_body,
            top_p = llm_conf.top_p,
            top_k = llm_conf.top_k,
            stop = llm_conf.stop,
            seed = llm_conf.seed,
            response_format = llm_conf.response_format,
        }

        local res = run_loop(resolved_conf)
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

--- Override std.env.get for test monkey-patching of resolve_temperature().
--- fn signature: (name: string) → string|nil
--- Call M._test_reset_env_get() after the test to restore production behaviour.
--- Production callers must never call this.
function M._test_set_env_get(fn)
    assert(type(fn) == "function", "_test_set_env_get requires a function")
    _env_get_override = fn
end

--- Reset the env_get override installed by M._test_set_env_get().
function M._test_reset_env_get()
    _env_get_override = nil
end

--- Return a fresh mf_state table with ST1 initial field defaults.
--- Used by unit tests to assert invariants without running the full make() pipeline.
--- Production callers must never call this.
function M._test_make_mf_state()
    return {
        iter = 0,
        last_err = nil,
        sr_digest_prev = nil,
        sr_history = {},
        file_digest = {},
        file_digest_refresh = "auto",
        modified_set = {},
    }
end

--- Override the internal distill_subloop function for test monkey-patching.
--- fn signature: (path, content, mf_state, conf) → digest, line_index, err_string|nil
--- Call M._test_reset_distill_subloop() after the test to restore production behaviour.
--- Production callers must never call this.
function M._test_set_distill_subloop(fn)
    assert(type(fn) == "function", "_test_set_distill_subloop requires a function")
    _distill_subloop_override = fn
end

--- Reset the distill_subloop override installed by M._test_set_distill_subloop().
function M._test_reset_distill_subloop()
    _distill_subloop_override = nil
end

--- Expose internal helpers for unit testing (read-only access).
--- Returns a table of helper functions so tests can call them directly.
function M._test_helpers()
    return {
        should_use_cache = should_use_cache,
        format_digest_response = format_digest_response,
        truncate_with_warning = truncate_with_warning,
        read_file_range_tool_handler = read_file_range_tool_handler,
        read_file_tool_handler = read_file_tool_handler,
        write_file = write_file,
        file_mtime = file_mtime,
        -- ST3 additions
        split_lines = split_lines,
        chunk_by_lines = chunk_by_lines,
        extract_text = extract_text,
        call_distill_llm = call_distill_llm,
        binary_search_pack = binary_search_pack,
        -- Stagnation / bookkeeping helpers (for unit testing 3-fix)
        is_stagnant_v2 = is_stagnant_v2,
        compute_sr_hash = compute_sr_hash,
        collect_modified_paths = collect_modified_paths,
        summarize_edits = summarize_edits,
        update_state = update_state,
        build_line_index = build_line_index,
        -- Temperature resolution (for unit testing env override)
        resolve_temperature = resolve_temperature,
        -- run_loop (for unit testing bad stagnation / full-loop scenarios without handler)
        run_loop = run_loop,
        -- Pure SR-diff / summary / stagnation branches (unit-test only exposure)
        extract_code = extract_code,
        make_summary = make_summary,
        is_stagnant = is_stagnant,
        fnv1a_hash = fnv1a_hash,
        build_failure_msg = build_failure_msg,
        filter_for_tool_output = filter_for_tool_output,
        cl_oai_map_finish_reason = proto_openai.map_finish_reason,
        cl_oai_normalize = cl_oai_normalize,
        cl_oai_convert_messages = proto_openai.convert_messages,
    }
end

return M
