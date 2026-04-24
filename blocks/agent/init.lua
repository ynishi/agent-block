-- blocks/agent/init.lua — Generic Agent module (StdPkg)
--
-- Usage:
--   local agent = require("agent")
--   local result = agent.run({
--       prompt = "...",
--       system = "...",
--       model = "claude-sonnet-4-20250514",
--       max_tokens = 4096,
--       timeout = 120,
--       max_iterations = 20,
--       max_tokens_budget = nil,
--       mcp_servers = { { name = "outline", command = "outline-mcp", args = {} } },
--       on_turn = function(turn_info) end,
--       extra_tools = {},
--       -- Anthropic server-side context editing (default ON).
--       -- Set to false to opt out entirely (no beta header, no body field).
--       context_management = true,
--       -- Optional override for the default edits table (clear_tool_uses_20250919).
--       context_management_config = { edits = { ... } },
--   })
--
-- result: { ok, content, usage, num_turns, error, messages }
--
-- Notes:
--   - All MCP/HTTP bridge calls are async (coroutine yield). agent.run() must be
--     called inside isle.coroutine_eval() or equivalent async context.
--   - tool.call() is sync. tool.schema() is sync.
--   - NEVER throws. All errors returned as { ok=false, error=... }.
--   - on_turn payload keys: turn_number, content, tool_calls, usage,
--     context_management (additive; absent when the server reports no edits
--     this turn, i.e. response.context_management is nil).

local M = {}

-- ============================================================
-- Internal: LLM dump controls (safe-by-default)
-- ============================================================
--
-- AGENT_BLOCK_LLM_DUMP:
--   "off"  (default) : no dump logs
--   "meta"           : status/model/usage/tool counts
--   "full"           : request/response body dump (API key is always redacted)
--
-- RUST_LOG fallback:
--   When AGENT_BLOCK_LLM_DUMP is unset and RUST_LOG contains "debug"/"trace",
--   dump mode becomes "meta".
--
-- Production guard:
--   When AGENT_BLOCK_ENV is "prod" or "production", "full" is downgraded to
--   "meta" unless AGENT_BLOCK_LLM_DUMP_ALLOW_PROD=true.
--
-- NOTE:
--   This guards transport/auth secrets (x-api-key), not model-generated text.
--   In production, prefer AGENT_BLOCK_LLM_DUMP=off.

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
            log.warn("agent: AGENT_BLOCK_LLM_DUMP=full blocked in production env; downgraded to meta")
            mode = "meta"
        end
    end
    return mode
end

local function sanitize_headers_for_dump(headers)
    local out = {}
    for k, v in pairs(headers or {}) do
        local lk = string.lower(tostring(k))
        if lk == "x-api-key" or lk == "authorization" then
            out[k] = "***REDACTED***"
        else
            out[k] = v
        end
    end
    return out
end

local function llm_dump(mode, msg)
    if mode ~= "off" then
        log.info("agent.llm_dump " .. msg)
    end
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

local function llm_dump_event(mode, event_name, fields)
    if mode == "off" then return end
    local pairs = {
        { "prefix", LLM_DUMP_PREFIX },
        { "event", event_name },
        { "component", "llm" },
    }
    for _, f in ipairs(fields or {}) do
        table.insert(pairs, f)
    end
    llm_dump(mode, format_kv(pairs))
end

-- Build fixed-order external metadata fields for dump logs.
-- Priority: opts.log_meta.* -> environment fallback -> nil.
local function build_log_meta(opts)
    local meta = opts and opts.log_meta or {}
    local trace_id = meta.trace_id or std.env.get("AGENT_BLOCK_TRACE_ID")
    if not trace_id then
        trace_id = meta.task_id or std.env.get("AGENT_BLOCK_TASK_ID")
        if trace_id then
            log.warn("agent: log_meta.task_id / AGENT_BLOCK_TASK_ID is deprecated; use trace_id / AGENT_BLOCK_TRACE_ID")
        end
    end
    return {
        trace_id = trace_id,
        agent_id = meta.agent_id or std.env.get("AGENT_BLOCK_AGENT_ID") or std.env.agent_id(),
        agent_name = meta.agent_name or std.env.get("AGENT_BLOCK_AGENT_NAME"),
        run_id = meta.run_id or std.env.get("AGENT_BLOCK_RUN_ID"),
    }
end

local function count_tool_use_blocks(content)
    local n = 0
    for _, block in ipairs(content or {}) do
        if block.type == "tool_use" then
            n = n + 1
        end
    end
    return n
end

local function count_text_chars(content)
    local n = 0
    for _, block in ipairs(content or {}) do
        if block.type == "text" and block.text then
            n = n + #tostring(block.text)
        end
    end
    return n
end

-- ============================================================
-- Default context management config (Anthropic server-side
-- rolling history via clear_tool_uses_20250919).
-- Trigger at 80K input_tokens, keep last 3 tool_uses,
-- clear at least 10K input_tokens worth.
-- Opt-out via opts.context_management = false.
-- Override via opts.context_management_config = { ... }.
-- ============================================================

local DEFAULT_CONTEXT_MANAGEMENT = {
    edits = {
        {
            type = "clear_tool_uses_20250919",
            trigger = { type = "input_tokens", value = 80000 },
            keep   = { type = "tool_uses",    value = 3 },
            clear_at_least = { type = "input_tokens", value = 10000 },
        },
    },
}

-- ============================================================
-- Internal: LLM API call (Anthropic Messages API)
-- ============================================================

--- Call Anthropic Messages API via http.request.
--- @param messages table  Messages array
--- @param opts table      Options: system, model, max_tokens, tools, timeout,
---                        context_management (table|nil — table enables the
---                        context-management beta header and body field; nil
---                        means opt-out, no header and no body field).
--- @param trace table|nil Optional call metadata for dump logs.
--- @return table|nil      Parsed response JSON on success, nil on error
--- @return string|nil     Error string on failure
local function llm_call(messages, opts, trace)
    local api_key = std.env.get("ANTHROPIC_API_KEY")
    if not api_key then
        return nil, "ANTHROPIC_API_KEY not set"
    end

    local model = opts.model or std.env.get_or("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")

    -- ============================================================
    -- Prompt caching (Anthropic Messages API)
    -- ------------------------------------------------------------
    -- Default: ON. `cache_control: {type=ephemeral}` markers are placed on
    -- the stable prefix so turn-2+ reads the cached prefix at ~10% input-
    -- token unit cost. Disable with `opts.cache_control = false` for A/B.
    --
    -- Breakpoint placement (2 of 4 budget used):
    --   1. system block (content-array form)         → caches [tools + system]
    --   2. tools[#opts.tools] (= last tool entry)    → caches [tools]
    -- Messages-level marker (3rd slot) is a planned follow-up to cache the
    -- conversation tail as well; leaving the last 2 slots free lets callers
    -- add their own markers if needed.
    --
    -- API ordering contract (docs.claude.com/en/docs/build-with-claude/prompt-caching):
    --   API processes prefix as  tools → system → messages
    --   cache_control on block X caches the prefix up to AND INCLUDING X.
    --   So marker on system caches [tools + system] regardless of how the
    --   messages array grows. Messages appended across ReAct turns do not
    --   invalidate the tools+system cache.
    --
    -- Minimum cacheable prefix size (Anthropic official):
    --   Sonnet / Opus: 1024 tokens
    --   Haiku:         2048 tokens
    -- Below the minimum the marker is silently ignored (no cache_creation,
    -- no cache_read, standard input-token billing).
    --
    -- PRACTICAL MARGIN (empirical, not documented):
    --   At exactly ~1264 tokens on Sonnet (well above the 1024 line) we
    --   observed stochastic cache misses — turn 1 cache_create = 0 AND
    --   cache_read = 0 despite the prefix exceeding the documented minimum.
    --   At ~1679 tokens the cache fires deterministically (turn 1 creates,
    --   turn 2+ reads). The effective threshold appears to include an
    --   undocumented safety margin.
    --   Recommendation: aim for ≥1.5× the published minimum
    --     (~1500 tokens for Sonnet/Opus, ~3000 tokens for Haiku)
    --   before relying on cache hits in production.
    --
    -- Byte-exact keying:
    --   The cache key is a hash of the prefix BYTES up to the marker.
    --   Any whitespace / key-ordering / extra-field drift in tools or
    --   system invalidates the key. `std.json.encode` orders keys
    --   alphabetically which keeps serialization stable across runs;
    --   avoid injecting per-turn timestamps / UUIDs / counters into
    --   system or tool schemas.
    --
    -- Non-standard fields in messages:
    --   Anthropic returns a non-spec `caller` field in tool_use blocks
    --   (`{"type":"direct"}`) which agent-block echoes back in turn 2+
    --   assistant messages. This does NOT affect cache matching because
    --   cache_control is placed before the messages array (tools + system
    --   prefix only); messages content is outside the cache scope.
    --
    -- Observability:
    --   Response `usage.cache_creation_input_tokens` → `cache_create`
    --   Response `usage.cache_read_input_tokens`     → `cache_read`
    --   Both are emitted to the "summary" dump event and the `on_turn`
    --   callback receives them via `info.usage`.
    --   Hit rate ≈ cache_read / (cache_read + input_tokens).
    --
    -- Disabling (`opts.cache_control = false`) is useful when:
    --   - A/B comparing with/without caching
    --   - system + tools is known to be < minimum (marker would be wasted)
    --   - caller wants strict byte-exact requests (no cache_control drift)
    -- ============================================================
    local cache_on = opts.cache_control ~= false

    local body = {
        model = model,
        max_tokens = opts.max_tokens or 4096,
        messages = messages,
    }
    if opts.system and opts.system ~= "" then
        if cache_on then
            body.system = {
                {
                    type = "text",
                    text = opts.system,
                    cache_control = { type = "ephemeral" },
                },
            }
        else
            body.system = opts.system
        end
    end
    if opts.tools and #opts.tools > 0 then
        if cache_on then
            -- Shallow-clone list + last entry so we don't mutate opts.tools
            -- across calls (caller's reference is preserved intact).
            local tools = {}
            for i = 1, #opts.tools - 1 do
                tools[i] = opts.tools[i]
            end
            local last = {}
            for k, v in pairs(opts.tools[#opts.tools]) do
                last[k] = v
            end
            last.cache_control = { type = "ephemeral" }
            tools[#opts.tools] = last
            body.tools = tools
        else
            body.tools = opts.tools
        end
    end

    local headers = {
        ["x-api-key"] = api_key,
        ["anthropic-version"] = "2023-06-01",
        ["content-type"] = "application/json",
    }

    -- Anthropic context-management (beta): add header + body only when enabled.
    -- call_opts normalization in M.run() makes opts.context_management either
    -- nil (opt-out) or a table (enabled: default or user-provided override).
    if opts.context_management ~= nil then
        headers["anthropic-beta"] = "context-management-2025-06-27"
        body.context_management = opts.context_management
    end

    local dump_mode = resolve_dump_mode()
    local call_index = trace and trace.call_index or "?"
    local turn = trace and trace.turn or "?"
    local iteration = trace and trace.iteration or "?"
    llm_dump_event(dump_mode, "request", {
        { "call", call_index },
        { "turn", turn },
        { "iter", iteration },
        { "trace_id", trace and trace.trace_id or nil },
        { "run_id", trace and trace.run_id or nil },
        { "agent_id", trace and trace.agent_id or nil },
        { "agent_name", trace and trace.agent_name or nil },
        { "model", body.model },
        { "messages", #messages },
        { "tools", #(body.tools or {}) },
        { "max_tokens", tonumber(body.max_tokens) or 0 },
        { "timeout", tonumber(opts.timeout or 120) or 120 },
        { "context_mgmt", opts.context_management ~= nil },
    })
    if dump_mode == "full" then
        llm_dump_event(dump_mode, "request_headers", {
            { "call", call_index },
            { "turn", turn },
            { "iter", iteration },
            { "payload", std.json.encode(sanitize_headers_for_dump(headers)) },
        })
        llm_dump_event(dump_mode, "request_body", {
            { "call", call_index },
            { "turn", turn },
            { "iter", iteration },
            { "payload", std.json.encode(body) },
        })
    end

    local start_ts = std.time.now()
    local resp = http.request("https://api.anthropic.com/v1/messages", {
        method = "POST",
        headers = headers,
        body = std.json.encode(body),
        timeout = opts.timeout or 120,
    })
    local elapsed_ms = math.floor((std.time.now() - start_ts) * 1000)

    llm_dump_event(dump_mode, "response", {
        { "call", call_index },
        { "turn", turn },
        { "iter", iteration },
        { "trace_id", trace and trace.trace_id or nil },
        { "run_id", trace and trace.run_id or nil },
        { "agent_id", trace and trace.agent_id or nil },
        { "agent_name", trace and trace.agent_name or nil },
        { "status", resp.status },
        { "latency_ms", elapsed_ms },
    })
    if dump_mode == "full" then
        llm_dump_event(dump_mode, "response_headers", {
            { "call", call_index },
            { "turn", turn },
            { "iter", iteration },
            { "payload", std.json.encode(resp.headers or {}) },
        })
        llm_dump_event(dump_mode, "response_body", {
            { "call", call_index },
            { "turn", turn },
            { "iter", iteration },
            { "payload", tostring(resp.body or "") },
        })
    end

    if resp.status ~= 200 then
        -- Do not include raw body in the returned error string; caller-side
        -- logs often propagate this message verbatim.
        return nil, "API error " .. resp.status
    end

    local decoded = std.json.decode(resp.body)
    if dump_mode ~= "off" then
        local usage = decoded.usage or {}
        local in_tok = tonumber(usage.input_tokens) or 0
        local out_tok = tonumber(usage.output_tokens) or 0
        -- Prompt-cache accounting (Anthropic: cache_* are disjoint from input_tokens).
        --   cache_create = bytes written to the cache on this call (~1.25x input price)
        --   cache_read   = bytes read from the cache on this call (~0.1x input price)
        -- hit_rate ≈ cache_read / (cache_read + in_tok).
        local cache_create = tonumber(usage.cache_creation_input_tokens) or 0
        local cache_read = tonumber(usage.cache_read_input_tokens) or 0
        local stop_reason = tostring(decoded.stop_reason or "unknown")
        local content_blocks = #(decoded.content or {})
        local tool_uses = count_tool_use_blocks(decoded.content)
        local text_chars = count_text_chars(decoded.content)
        local cm_applied = 0
        if decoded.context_management and decoded.context_management.applied_edits then
            cm_applied = #decoded.context_management.applied_edits
        end
        llm_dump_event(dump_mode, "summary", {
            { "call", call_index },
            { "turn", turn },
            { "iter", iteration },
            { "trace_id", trace and trace.trace_id or nil },
            { "run_id", trace and trace.run_id or nil },
            { "agent_id", trace and trace.agent_id or nil },
            { "agent_name", trace and trace.agent_name or nil },
            { "stop_reason", stop_reason },
            { "blocks", content_blocks },
            { "tool_uses", tool_uses },
            { "text_chars", text_chars },
            { "usage_in", in_tok },
            { "usage_out", out_tok },
            { "usage_total", in_tok + out_tok },
            { "cache_create", cache_create },
            { "cache_read", cache_read },
            { "context_edits", cm_applied },
        })
    end
    return decoded, nil
end

-- ============================================================
-- Internal: Budget tracking
-- ============================================================

--- Create a new budget tracker.
--- @param max_tokens_budget number|nil  Total token limit (nil = unlimited)
--- @return table  Tracker with :add(usage), :exceeded(), :summary() methods
local function new_budget_tracker(max_tokens_budget)
    local tracker = {
        input_tokens = 0,
        output_tokens = 0,
        total_tokens = 0,
        limit = max_tokens_budget,
    }

    function tracker:add(usage)
        if usage then
            self.input_tokens = self.input_tokens + (usage.input_tokens or 0)
            self.output_tokens = self.output_tokens + (usage.output_tokens or 0)
            self.total_tokens = self.input_tokens + self.output_tokens
        end
    end

    function tracker:exceeded()
        if not self.limit then return false end
        return self.total_tokens >= self.limit
    end

    function tracker:summary()
        return {
            input_tokens = self.input_tokens,
            output_tokens = self.output_tokens,
            total_tokens = self.total_tokens,
        }
    end

    return tracker
end

-- ============================================================
-- Internal: MCP server integration
-- ============================================================

--- Connect to MCP servers and collect tool definitions.
--- Returns mcp_tool_map and list of connected server names.
--- On failure, returns nil + error string (with already-connected servers in third return).
---
--- @param servers table  Array of { name, command, args? }
--- @return table|nil     mcp_tool_map { ["server__tool"] = { server, tool, def } }
--- @return string|nil    Error string on failure
--- @return table         List of connected server names (for cleanup on failure)
local function connect_mcp_servers(servers, opts)
    local mcp_tool_map = {}
    local connected = {}
    opts = opts or {}

    for _, srv in ipairs(servers) do
        local name = srv.name

        -- Connect to MCP server: use HTTP transport when srv.url is set,
        -- otherwise fall back to stdio (srv.command / srv.args).
        local ok, err
        if srv.url then
            -- Merge server-level trace_context into transport_opts when not already set.
            local transport_opts = {}
            for k, v in pairs(srv.transport_opts or {}) do transport_opts[k] = v end
            if transport_opts.trace_context == nil then
                transport_opts.trace_context = not not srv.trace_context
            end
            ok, err = pcall(mcp.connect_http, name, srv.url, transport_opts)
        else
            local command = srv.command
            local args = srv.args or {}
            local connect_opts = { trace_context = not not srv.trace_context }
            ok, err = pcall(mcp.connect, name, command, args, connect_opts)
        end
        if not ok then
            return nil, "mcp connect failed for '" .. name .. "': " .. tostring(err), connected
        end
        table.insert(connected, name)

        -- Auto-register sampling handler if opts.sampling is set.
        if opts.sampling then
            local sampling_ok, sampling_err = pcall(mcp.set_sampling_handler, name, opts.sampling)
            if not sampling_ok then
                log.warn("agent: mcp set_sampling_handler failed for '" .. name .. "': " .. tostring(sampling_err))
            end
        end

        -- List tools (async)
        local list_result = mcp.list_tools(name)
        if not list_result.ok then
            return nil, "mcp list_tools failed for '" .. name .. "': " .. tostring(list_result.error), connected
        end

        local tools = list_result.tools or {}
        for _, t in ipairs(tools) do
            local ns_name = name .. "__" .. t.name
            -- Convert inputSchema (camelCase) -> input_schema (snake_case) for Anthropic API
            local input_schema = t.inputSchema or t.input_schema or { type = "object", properties = {} }
            mcp_tool_map[ns_name] = {
                server = name,
                tool = t.name,
                def = {
                    name = ns_name,
                    description = t.description or "",
                    input_schema = input_schema,
                },
            }
        end

        -- Register on_progress / progress_to_log (no capability gate; all servers).
        -- Callback priority: opts.on_progress wins over progress_to_log bool.
        if opts.on_progress then
            local sn = name
            local user_cb = opts.on_progress
            -- Register user_cb directly on the main Isle (no bytecode dump needed).
            -- The callback runs with upvalues intact because main Isle is never
            -- crossed; only the event table `ev` is constructed on the Rust side.
            mcp.on_progress(sn, function(ev)
                local ok, cb_err = pcall(user_cb, ev)
                if not ok then
                    log.warn("agent: on_progress callback error: " .. tostring(cb_err))
                end
            end)
        elseif opts.progress_to_log then
            local sn = name
            mcp.on_progress(sn, function(ev)
                local msg = "mcp progress: server=" .. tostring(ev.server)
                    .. " token=" .. tostring(ev.token)
                    .. " p=" .. tostring(ev.progress) .. "/" .. tostring(ev.total or "")
                if ev.message and ev.message ~= "" then
                    msg = msg .. " msg=" .. ev.message
                end
                log.info(msg)
            end)
        end

        -- Opt-in: register resources / prompts meta-tools + on_log/log_to_stderr
        -- if capability present (server_info call shared for all capability-gated opts).
        if opts.enable_resources or opts.enable_prompts or opts.on_log or opts.log_to_stderr then
            local si_result = mcp.server_info(name)
            if si_result.ok then
                local caps = (si_result.server_info and si_result.server_info.capabilities) or {}

                if opts.enable_resources then
                    if caps.resources ~= nil then
                        local sn = name
                        tool.register(sn .. "__mcp_list_resources", {
                            description = "List available resources on MCP server '" .. sn .. "'",
                            input_schema = { type = "object", properties = {} },
                        }, function(_input)
                            local r = mcp.list_resources(sn)
                            if not r.ok then return std.json.encode({ error = r.error }) end
                            return std.json.encode(r.resources)
                        end)
                        tool.register(sn .. "__mcp_read_resource", {
                            description = "Read a resource by URI from MCP server '" .. sn .. "'",
                            input_schema = {
                                type = "object",
                                properties = { uri = { type = "string" } },
                                required = { "uri" },
                            },
                        }, function(input)
                            local r = mcp.read_resource(sn, input.uri)
                            if not r.ok then return std.json.encode({ error = r.error }) end
                            return std.json.encode(r.contents)
                        end)
                    else
                        log.info("agent: server '" .. name .. "' has no resources capability; skipping register")
                    end
                end

                if opts.enable_prompts then
                    if caps.prompts ~= nil then
                        local sn = name
                        tool.register(sn .. "__mcp_list_prompts", {
                            description = "List available prompts on MCP server '" .. sn .. "'",
                            input_schema = { type = "object", properties = {} },
                        }, function(_input)
                            local r = mcp.list_prompts(sn)
                            if not r.ok then return std.json.encode({ error = r.error }) end
                            return std.json.encode(r.prompts)
                        end)
                        tool.register(sn .. "__mcp_get_prompt", {
                            description = "Get a prompt by name from MCP server '" .. sn .. "'",
                            input_schema = {
                                type = "object",
                                properties = {
                                    name = { type = "string" },
                                    args = { type = "object" },
                                },
                                required = { "name" },
                            },
                        }, function(input)
                            local r = mcp.get_prompt(sn, input.name, input.args or {})
                            if not r.ok then return std.json.encode({ error = r.error }) end
                            return std.json.encode(r.messages)
                        end)
                    else
                        log.info("agent: server '" .. name .. "' has no prompts capability; skipping register")
                    end
                end

                -- Register on_log / log_to_stderr (logging capability gate).
                -- Callback priority: opts.on_log wins over log_to_stderr bool.
                if opts.on_log or opts.log_to_stderr then
                    if caps.logging ~= nil then
                        local sn = name
                        if opts.on_log then
                            local user_cb = opts.on_log
                            -- Register user_cb directly on the main Isle (upvalue-safe).
                            mcp.on_log(sn, function(ev)
                                local ok, cb_err = pcall(user_cb, ev)
                                if not ok then
                                    log.warn("agent: on_log callback error: " .. tostring(cb_err))
                                end
                            end)
                        else
                            -- log_to_stderr=true: bridge to log.* by level
                            mcp.on_log(sn, function(ev)
                                local msg = "mcp log: server=" .. tostring(ev.server)
                                    .. " logger=" .. tostring(ev.logger)
                                    .. " data=" .. tostring(ev.data)
                                if ev.level == "debug" then
                                    log.debug(msg)
                                elseif ev.level == "warning" then
                                    log.warn(msg)
                                elseif ev.level == "error" then
                                    log.error(msg)
                                else
                                    log.info(msg)
                                end
                            end)
                        end
                    else
                        log.info("agent: server '" .. name .. "' has no logging capability; on_log/log_to_stderr skipped")
                    end
                end
            else
                log.warn("agent: mcp.server_info failed for '" .. name .. "': " .. tostring(si_result.error))
            end
        end
    end

    return mcp_tool_map, nil, connected
end

--- Gracefully disconnect from MCP servers.
--- Logs errors but does not throw.
--- @param server_names table  Array of server name strings
local function disconnect_mcp_servers(server_names)
    for _, name in ipairs(server_names) do
        local ok, err = pcall(mcp.disconnect, name)
        if not ok then
            log.warn("agent: mcp disconnect error for '" .. name .. "': " .. tostring(err))
        end
    end
end

-- ============================================================
-- Internal: Build unified tools array
-- ============================================================

--- Build the unified tools array for the Anthropic API.
--- Merges tool.schema() (registered Lua tools) + MCP tools + extra_tools.
--- @param mcp_tool_map table  MCP namespace map (may be empty)
--- @param extra_tools table   Additional Anthropic tool definitions (may be nil/empty)
--- @return table              Unified tools array in Anthropic format
local function build_tools(mcp_tool_map, extra_tools)
    local tools = {}

    -- Add registered Lua tools from tool.schema()
    local lua_tools = tool.schema()
    for _, t in ipairs(lua_tools) do
        table.insert(tools, t)
    end

    -- Add MCP tools (already in Anthropic format from connect_mcp_servers)
    for _, entry in pairs(mcp_tool_map) do
        table.insert(tools, entry.def)
    end

    -- Add extra_tools if provided
    if extra_tools then
        for _, t in ipairs(extra_tools) do
            table.insert(tools, t)
        end
    end

    return tools
end

-- ============================================================
-- Internal: Tool dispatch (unified)
-- ============================================================

--- Dispatch a tool call to either MCP or the local Lua registry.
--- Errors are returned as (content, is_error=true) instead of throwing.
--- @param name string         Tool name (possibly namespaced as "server__tool")
--- @param input table         Tool input from LLM
--- @param mcp_tool_map table  MCP namespace map
--- @return string             Result content string
--- @return boolean            is_error flag
local function dispatch_tool(name, input, mcp_tool_map)
    -- Check if this is an MCP-namespaced tool
    if mcp_tool_map[name] then
        local entry = mcp_tool_map[name]
        local call_result = mcp.call(entry.server, entry.tool, input)
        -- ok=false covers transport / protocol / timeout failures only.
        if not call_result.ok then
            return tostring(call_result.error or "mcp.call failed"), true
        end

        -- Server-reported tool-execution error (MCP `isError`). Forward the
        -- content as-is so the LLM can self-correct in the ReAct loop.
        local is_error = call_result.is_error == true
        if is_error then
            log.warn(string.format("mcp tool '%s.%s' returned isError=true", entry.server, entry.tool))
        end

        -- Extract content from MCP result
        local content_blocks = call_result.content or {}
        if #content_blocks == 1 and content_blocks[1].type == "text" then
            return content_blocks[1].text, is_error
        elseif #content_blocks == 0 then
            return "", is_error
        else
            -- Multiple blocks or non-text: encode as JSON
            return std.json.encode(content_blocks), is_error
        end
    end

    -- Fall back to registered Lua tool
    local ok, res = pcall(tool.call, name, input)
    if not ok then
        return "tool error: " .. tostring(res), true
    end
    if type(res) == "table" then
        return std.json.encode(res), false
    end
    return tostring(res), false
end

-- ============================================================
-- Internal: Extract text content from Anthropic response
-- ============================================================

--- Collect all text blocks from the Anthropic content array and concatenate.
--- @param content table  Array of content blocks from Anthropic response
--- @return string        Concatenated text, or empty string
local function extract_text(content)
    local parts = {}
    for _, block in ipairs(content or {}) do
        if block.type == "text" and block.text then
            table.insert(parts, block.text)
        end
    end
    return table.concat(parts, "\n")
end

-- ============================================================
-- Public: agent.run(opts)
-- ============================================================

--- Run a ReAct agent loop.
---
--- @param opts table  {
---   prompt          (required) Initial user prompt string
---   system          (optional) System prompt string
---   model           (optional) LLM model identifier
---   max_tokens      (optional) Per-request token limit (default: 4096)
---   timeout         (optional) HTTP timeout in seconds (default: 120)
---   max_iterations  (optional) Max tool-use loop iterations (default: 20)
---   max_tokens_budget (optional) Total token budget across all iterations (default: nil = unlimited)
---   mcp_servers     (optional) Array of { name, command, args? }
---   on_turn         (optional) Callback function(turn_info). turn_info has
---                   keys: turn_number, content, tool_calls, usage, and
---                   context_management (passed through from the Anthropic
---                   response; absent when no edits fired this turn).
---   log_meta        (optional) External metadata for structured dump logs.
---                   Keys: `trace_id`, `agent_id`, `agent_name`, `run_id`.
---                   Values are attached to `ab.llm` request/response/summary lines.
---                   Fallback env vars: AGENT_BLOCK_TRACE_ID / AGENT_BLOCK_AGENT_ID
---                   / AGENT_BLOCK_AGENT_NAME / AGENT_BLOCK_RUN_ID.
---                   Deprecated fallback: `task_id` / AGENT_BLOCK_TASK_ID maps to `trace_id`.
---   extra_tools     (optional) Extra Anthropic tool definitions to include
---   context_management        (optional, default true) When false, opt out of
---                   Anthropic server-side context editing entirely (no beta
---                   header, no body field). Any non-false value (nil, true,
---                   table) keeps it enabled.
---   context_management_config (optional) Full override table passed as
---                   body.context_management. Defaults to DEFAULT_CONTEXT_MANAGEMENT
---                   (clear_tool_uses_20250919 with 80K/keep=3/clear>=10K).
---                   Ignored when context_management == false.
--- }
---
--- @return table  {
---   ok         boolean
---   content    string  (final text response)
---   usage      { input_tokens, output_tokens, total_tokens }
---   num_turns  number
---   error      string  (when ok=false)
---   messages   table   (full conversation history)
--- }
function M.run(opts)
    opts = opts or {}

    -- Validate required fields
    if not opts.prompt or opts.prompt == "" then
        return { ok = false, error = "prompt is required", usage = { input_tokens = 0, output_tokens = 0, total_tokens = 0 }, num_turns = 0, messages = {} }
    end

    -- Budget tracker
    local budget = new_budget_tracker(opts.max_tokens_budget)
    local max_iter = opts.max_iterations or 20

    -- Connect MCP servers if specified
    local mcp_tool_map = {}
    local connected_servers = {}

    if opts.mcp_servers and #opts.mcp_servers > 0 then
        local tool_map, err, partial_connected = connect_mcp_servers(opts.mcp_servers, opts)
        if err then
            -- Disconnect any servers that did connect before the failure
            disconnect_mcp_servers(partial_connected)
            return {
                ok = false,
                error = err,
                usage = budget:summary(),
                num_turns = 0,
                messages = {},
            }
        end
        mcp_tool_map = tool_map
        connected_servers = partial_connected
    end

    -- Build unified tools array
    local tools = build_tools(mcp_tool_map, opts.extra_tools)

    -- Normalize context_management opts once:
    --   opts.context_management == false                   → cm_final = nil (opt-out)
    --   opts.context_management_config = { ... } (or nil)  → cm_final = override or DEFAULT
    -- Strict equality (~= false) is used so nil (unset) is treated as default-on.
    local cm_final
    if opts.context_management == false then
        cm_final = nil
    else
        cm_final = opts.context_management_config or DEFAULT_CONTEXT_MANAGEMENT
    end

    -- Build call options for llm_call
    local call_opts = {
        model = opts.model,
        max_tokens = opts.max_tokens or 4096,
        timeout = opts.timeout or 120,
        system = opts.system,
        tools = tools,
        context_management = cm_final,  -- nil = opt-out, table = enabled
    }
    local log_meta = build_log_meta(opts)

    -- Initialize message history
    local messages = {
        { role = "user", content = opts.prompt },
    }

    -- ReAct loop state
    local num_turns = 0
    local llm_call_index = 0
    local final_content = ""
    local loop_error = nil

    -- pcall wrapper for guaranteed MCP cleanup
    local loop_ok, loop_err = pcall(function()
        local iter = 0

        while true do
            -- Call LLM
            llm_call_index = llm_call_index + 1
            local response, api_err = llm_call(messages, call_opts, {
                call_index = llm_call_index,
                -- num_turns increments after a successful assistant response append.
                -- For request-side correlation, report the upcoming turn number.
                turn = num_turns + 1,
                iteration = iter + 1,
                trace_id = log_meta.trace_id,
                agent_id = log_meta.agent_id,
                agent_name = log_meta.agent_name,
                run_id = log_meta.run_id,
            })
            if not response then
                loop_error = api_err
                return
            end

            -- Append assistant message
            table.insert(messages, {
                role = "assistant",
                content = response.content,
            })

            -- Track usage BEFORE budget check
            budget:add(response.usage)
            num_turns = num_turns + 1

            -- Collect tool calls from response
            local tool_calls = {}
            for _, block in ipairs(response.content or {}) do
                if block.type == "tool_use" then
                    table.insert(tool_calls, block)
                end
            end

            -- Extract current text content
            final_content = extract_text(response.content)

            -- Fire on_turn callback (errors are logged, not propagated)
            if opts.on_turn then
                local cb_ok, cb_err = pcall(opts.on_turn, {
                    turn_number = num_turns,
                    content = response.content,
                    tool_calls = tool_calls,
                    usage = response.usage,
                    -- Pass-through of Anthropic response.context_management.
                    -- When the server didn't apply any edits this turn the
                    -- field is nil and Lua removes the key from the payload,
                    -- preserving the historical 4-key shape for existing callbacks.
                    context_management = response.context_management,
                })
                if not cb_ok then
                    log.warn("agent: on_turn callback error: " .. tostring(cb_err))
                end
            end

            -- No tool calls → done (end_turn or max_tokens)
            if #tool_calls == 0 then
                break
            end

            -- Check stop reason
            local stop_reason = response.stop_reason
            if stop_reason == "end_turn" or stop_reason == "max_tokens" then
                break
            end

            -- Budget checks
            iter = iter + 1
            if iter >= max_iter then
                log.warn("agent: max iterations (" .. max_iter .. ") reached")
                break
            end

            if budget:exceeded() then
                log.warn("agent: token budget exceeded (" .. budget.total_tokens .. "/" .. budget.limit .. ")")
                break
            end

            -- Dispatch tool calls and collect results
            local tool_results = {}
            for _, tc in ipairs(tool_calls) do
                local content_str, is_error = dispatch_tool(tc.name, tc.input, mcp_tool_map)
                table.insert(tool_results, {
                    type = "tool_result",
                    tool_use_id = tc.id,
                    content = content_str,
                    is_error = is_error or nil,
                })
            end

            -- Append tool results as user message
            table.insert(messages, {
                role = "user",
                content = tool_results,
            })
        end
    end)

    -- Always disconnect MCP servers, regardless of loop outcome
    disconnect_mcp_servers(connected_servers)

    -- Propagate unexpected pcall error
    if not loop_ok then
        return {
            ok = false,
            error = tostring(loop_err),
            usage = budget:summary(),
            num_turns = num_turns,
            messages = messages,
        }
    end

    -- Propagate structured API error
    if loop_error then
        return {
            ok = false,
            error = loop_error,
            usage = budget:summary(),
            num_turns = num_turns,
            messages = messages,
        }
    end

    return {
        ok = true,
        content = final_content,
        usage = budget:summary(),
        num_turns = num_turns,
        messages = messages,
    }
end

return M
