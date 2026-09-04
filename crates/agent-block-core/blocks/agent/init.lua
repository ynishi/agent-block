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
--       -- Provider selection (default "anthropic"). Use "openai" for OpenAI-compatible
--       -- endpoints (vLLM, llama.cpp, OpenRouter, RunPod, etc.).
--       provider = "anthropic",  -- "anthropic" | "openai"
--       -- Base URL override for OpenAI-compatible endpoints.
--       -- Default for openai: "https://api.openai.com/v1"
--       base_url = "http://localhost:8080/v1",
--       -- Per-call API key override (avoids env var conflicts with multiple providers).
--       api_key = "sk-...",
--       -- Custom env var name for the API key (default: ANTHROPIC_API_KEY / OPENAI_API_KEY).
--       api_key_env = "MY_OPENAI_KEY",
--       -- Anthropic server-side context editing (default ON).
--       -- Set to false to opt out entirely (no beta header, no body field).
--       -- Anthropic-only: warn+ignored when provider="openai".
--       context_management = true,
--       -- Optional override for the default edits table (clear_tool_uses_20250919).
--       context_management_config = { edits = { ... } },
--       -- Protocol options, translated per provider by the llm_proto package:
--       -- tool_choice / parallel_tool_calls / thinking / response_format /
--       -- dialect / betas / temperature / top_p / top_k / stop / seed / n /
--       -- logit_bias / logprobs / top_logprobs / frequency_penalty /
--       -- presence_penalty / metadata / service_tier / store /
--       -- prompt_cache_key / safety_identifier / verbosity / extra_body.
--       tool_choice = "required",
--       thinking    = { effort = "medium" },
--       max_retries = 2,   -- transient failures only (rate limit / overload / 5xx)
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

-- Provider wire format (request building, tool_choice / thinking translation,
-- response normalization) lives in `llm_proto`; the call-dispatch-repeat loop
-- lives in `tool_loop`. What this module owns is the agent part: assembling the
-- tool set from the registry and MCP, the token budget, and the dump.
local tool_loop = require("tool_loop")

-- The run path reaches llm_proto's wire format through tool_loop; what this
-- module calls directly is the MCP vocabulary it shares with knl_adapter
-- (`mcp_tool_decl` / `mcp_result_text`).
local proto = require("llm_proto")

-- Re-exported through `M._test_helpers` only.
local proto_openai = proto.adapter("openai")

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

--- The four ab.obs correlation fields.
---
--- Closed: this stopped being an internal detail when compile_loop began
--- resolving its own obs ids through `_log_meta`. Every field is optional
--- because an unset environment is the ordinary case, but an unexpected key
--- means the two components have drifted apart on what the fields are — which
--- is the failure the convention exists to prevent, and it would otherwise show
--- up as a run that cannot be selected rather than as an error.
---
--- Checked only in dev mode (LSHAPE_CHECK=1).
local LOG_META = T.shape({
    trace_id = T.string:is_optional(),
    run_id = T.string:is_optional(),
    agent_id = T.string:is_optional(),
    agent_name = T.string:is_optional(),
}, { open = false })

--- Token accounting. `thinking_tokens` is optional because the two
--- fail-before-we-started paths return a literal zeroed table rather than a
--- tracker summary.
local USAGE = T.shape({
    input_tokens = T.number,
    output_tokens = T.number,
    total_tokens = T.number,
    thinking_tokens = T.number:is_optional(),
})

--- What `run` returns, as the two things it can be.
---
--- `run` never throws; every failure comes back as `ok = false` with an
--- `error`. Two closed alternatives say that in a way that can be checked:
--- success carries `content` and no `error`, failure carries `error` and no
--- `content`. A single shape with both optional would accept the result that
--- has neither, which is the shape a caller cannot do anything with and the one
--- a half-finished error path produces.
local RUN_OK = T.shape({
    ok = T.literal(true),
    content = T.string,
    usage = USAGE,
    num_turns = T.number,
    messages = T.array_of(T.table),
}, { open = false })

local RUN_ERR = T.shape({
    ok = T.literal(false),
    error = T.string,
    usage = USAGE,
    num_turns = T.number,
    messages = T.array_of(T.table),
}, { open = false })

local RUN_RESULT = T.any_of({ RUN_OK, RUN_ERR })

--- What `mcp.call` hands back across the host boundary.
---
--- The bridge builds this table field by field in Rust and nothing on this side
--- described it, so the two languages agreed by coincidence and review. Closed:
--- a field added on the Rust side that this side does not know about is exactly
--- the drift worth catching, and the e2e tests run real servers through this
--- path, so the check has somewhere to fire.
---
--- `ok = false` covers transport, protocol and timeout failures and carries
--- `error`. A tool that ran and reported failure comes back `ok = true` with
--- `is_error = true`, which is why the two are separate fields rather than one.
local MCP_CALL_RESULT = T.shape({
    ok = T.boolean,
    error = T.string:is_optional(),
    content = T.table:is_optional(),
    is_error = T.boolean:is_optional(),
    structured_content = T.any:is_optional(),
}, { open = false })

-- ============================================================
-- Internal: parent LLM context stack (_AGENT_LLM_CTX)
-- ============================================================
--
-- Allows child tools (e.g. compile_loop) to inherit the calling agent's
-- provider/model/api_key at handler call time without hard-coding provider
-- defaults or env vars in the factory (Crux #2).
--
-- Stack entries: { provider, base_url, api_key, api_key_env, model }
-- push: M.run() entry (after opts validation)
-- pop:  M.run() exit — both success and pcall-error branches
--
-- Never exposed as a Lua global. Accessed via M._llm_ctx_top().
local _AGENT_LLM_CTX = {}

--- M._llm_ctx_top() → table|nil
--- Return the topmost LLM context pushed by the innermost active agent.run(),
--- or nil when called outside any agent.run() (no parent context).
function M._llm_ctx_top()
    return _AGENT_LLM_CTX[#_AGENT_LLM_CTX]
end

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
    if not v then
        return false
    end
    v = string.lower(tostring(v))
    return v == "1" or v == "true" or v == "yes" or v == "on"
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
            log.warn("agent: AGENT_BLOCK_LLM_DUMP=full blocked in production env; downgraded to meta")
            mode = "meta"
        end
    end
    return mode
end

-- Process-lifetime cache for the dump mode. llm_call fires per turn and per
-- tool-loop turn; env vars do not change mid-process, so resolving once avoids
-- re-reading env and repeating the prod-downgrade warn.
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
-- Keep this list in sync with the other two copies: blocks/tools/compile_loop/init.lua
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

local function llm_dump(mode, msg)
    if mode ~= "off" then
        log.info("agent.llm_dump " .. msg)
    end
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

local function llm_dump_event(mode, event_name, fields)
    if mode == "off" then
        return
    end
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
    return shape.assert_dev({
        trace_id = trace_id,
        agent_id = meta.agent_id or std.env.get("AGENT_BLOCK_AGENT_ID") or std.env.agent_id(),
        agent_name = meta.agent_name or std.env.get("AGENT_BLOCK_AGENT_NAME"),
        run_id = meta.run_id or std.env.get("AGENT_BLOCK_RUN_ID"),
    }, LOG_META, "agent log_meta")
end

--- The four ab.obs correlation fields, resolved from `opts.log_meta` and the
--- environment.
---
--- Exposed because a sibling block emitting its own ab.obs lines has to resolve
--- the ids the same way this one does — `compile_loop` runs its own loop, and a
--- convention where each component reaches for the environment slightly
--- differently is one that cannot be relied on to select a run. Underscore
--- prefix marks it as cross-block reach, like `_llm_ctx_top`, not agent API.
---
--- @param opts table|nil  May carry `log_meta` with any of the four fields.
--- @return table  { trace_id, run_id, agent_id, agent_name }, any of them nil.
function M._log_meta(opts)
    return build_log_meta(opts)
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
            keep = { type = "tool_uses", value = 3 },
            clear_at_least = { type = "input_tokens", value = 10000 },
        },
    },
}

-- ============================================================
-- Internal: dump hooks
-- ============================================================

--- Build the `ab.llm` observability hooks handed to tool_loop.
---
--- The loop owns the HTTP call now, so the dump is expressed as callbacks over
--- it: `request` / `response` from the wire, `summary` from the decoded reply.
--- Field order and names are unchanged — `AGENT_BLOCK_LLM_DUMP` consumers parse
--- these lines.
---
--- @param opts table      Agent run options (reads provider, timeout, context_management)
--- @param cm table|nil    Resolved context-management config
--- @param log_meta table  trace_id / agent_id / agent_name / run_id
--- @return function on_request, function on_response, function on_summary
local function new_dump_hooks(opts, cm, log_meta)
    local dump_mode = resolve_dump_mode_cached()
    local is_openai = (opts.provider or "anthropic") == "openai"

    -- call / turn / iter were always the same number; they stay as three keys
    -- so existing dump parsers keep matching.
    local function trace_fields(turn)
        return {
            { "call", turn },
            { "turn", turn },
            { "iter", turn },
            { "trace_id", log_meta.trace_id },
            { "run_id", log_meta.run_id },
            { "agent_id", log_meta.agent_id },
            { "agent_name", log_meta.agent_name },
        }
    end

    local function with(turn, extra)
        local fields = trace_fields(turn)
        for _, kv in ipairs(extra) do
            table.insert(fields, kv)
        end
        if is_openai then
            table.insert(fields, { "provider", "openai" })
        end
        return fields
    end

    local function payload_event(name, turn, payload)
        llm_dump_event(dump_mode, name, {
            { "call", turn },
            { "turn", turn },
            { "iter", turn },
            { "payload", payload },
        })
    end

    local function on_request(info)
        local body = info.body or {}
        llm_dump_event(
            dump_mode,
            "request",
            with(info.turn, {
                { "model", body.model },
                { "messages", #(body.messages or {}) },
                { "tools", #(body.tools or {}) },
                { "max_tokens", tonumber(body.max_tokens) or 0 },
                { "timeout", tonumber(opts.timeout or 120) or 120 },
                { "context_mgmt", (not is_openai) and cm ~= nil },
            })
        )
        if dump_mode == "full" then
            payload_event("request_headers", info.turn, std.json.encode(sanitize_headers_for_dump(info.headers)))
            payload_event("request_body", info.turn, info.body_json)
        end
    end

    local function on_response(info)
        llm_dump_event(
            dump_mode,
            "response",
            with(info.turn, {
                { "status", info.status },
                { "latency_ms", info.latency_ms },
            })
        )
        if dump_mode == "full" then
            payload_event("response_headers", info.turn, std.json.encode(sanitize_headers_for_dump(info.headers)))
            payload_event("response_body", info.turn, tostring(info.body or ""))
        end
    end

    --- Emitted from the turn callback, which is the first place the decoded
    --- reply exists.
    local function on_summary(turn, decoded)
        if dump_mode == "off" then
            return
        end
        local usage = decoded.usage or {}
        local in_tok = tonumber(usage.input_tokens) or 0
        local out_tok = tonumber(usage.output_tokens) or 0
        -- Prompt-cache accounting (Anthropic: cache_* are disjoint from
        -- input_tokens). cache_create is written this call (~1.25x input
        -- price), cache_read is served from cache (~0.1x); the hit rate is
        -- cache_read / (cache_read + usage_in). Absent on OpenAI, hence 0.
        local cm_applied = 0
        if decoded.context_management and decoded.context_management.applied_edits then
            cm_applied = #decoded.context_management.applied_edits
        end
        llm_dump_event(
            dump_mode,
            "summary",
            with(turn, {
                { "stop_reason", tostring(decoded.stop_reason or "unknown") },
                { "blocks", #(decoded.content or {}) },
                { "tool_uses", count_tool_use_blocks(decoded.content) },
                { "text_chars", count_text_chars(decoded.content) },
                { "usage_in", in_tok },
                { "usage_out", out_tok },
                { "usage_total", in_tok + out_tok },
                { "usage_thinking", tonumber(usage.thinking_tokens) or 0 },
                { "cache_create", tonumber(usage.cache_creation_input_tokens) or 0 },
                { "cache_read", tonumber(usage.cache_read_input_tokens) or 0 },
                { "context_edits", cm_applied },
            })
        )
    end

    return on_request, on_response, on_summary
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
        -- Reasoning tokens are billed as output and already counted inside
        -- output_tokens; tracked separately so callers can see how much of
        -- the spend went to thinking.
        thinking_tokens = 0,
        limit = max_tokens_budget,
    }

    function tracker:add(usage)
        if usage then
            self.input_tokens = self.input_tokens + (usage.input_tokens or 0)
            self.output_tokens = self.output_tokens + (usage.output_tokens or 0)
            self.thinking_tokens = self.thinking_tokens + (usage.thinking_tokens or 0)
            self.total_tokens = self.input_tokens + self.output_tokens
        end
    end

    function tracker:exceeded()
        if not self.limit then
            return false
        end
        return self.total_tokens >= self.limit
    end

    function tracker:summary()
        return {
            input_tokens = self.input_tokens,
            output_tokens = self.output_tokens,
            total_tokens = self.total_tokens,
            thinking_tokens = self.thinking_tokens,
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
            for k, v in pairs(srv.transport_opts or {}) do
                transport_opts[k] = v
            end
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
            -- The `<server>__<tool>` namespace and the camelCase inputSchema
            -- conversion are llm_proto's, shared with knl_adapter's ToolPort:
            -- one tool must not get two names depending on which loop bound it.
            local decl = proto.mcp_tool_decl(name, t)
            mcp_tool_map[decl.name] = {
                server = name,
                tool = t.name,
                def = {
                    name = decl.name,
                    description = decl.description,
                    input_schema = decl.input_schema,
                    group = M._resolve_mcp_group(t, name),
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
                local msg = "mcp progress: server="
                    .. tostring(ev.server)
                    .. " token="
                    .. tostring(ev.token)
                    .. " p="
                    .. tostring(ev.progress)
                    .. "/"
                    .. tostring(ev.total or "")
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
                            if not r.ok then
                                return std.json.encode({ error = r.error })
                            end
                            return std.json.encode(r.resources)
                        end, { group = sn })
                        tool.register(sn .. "__mcp_read_resource", {
                            description = "Read a resource by URI from MCP server '" .. sn .. "'",
                            input_schema = {
                                type = "object",
                                properties = { uri = { type = "string" } },
                                required = { "uri" },
                            },
                        }, function(input)
                            local r = mcp.read_resource(sn, input.uri)
                            if not r.ok then
                                return std.json.encode({ error = r.error })
                            end
                            return std.json.encode(r.contents)
                        end, { group = sn })
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
                            if not r.ok then
                                return std.json.encode({ error = r.error })
                            end
                            return std.json.encode(r.prompts)
                        end, { group = sn })
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
                            if not r.ok then
                                return std.json.encode({ error = r.error })
                            end
                            return std.json.encode(r.messages)
                        end, { group = sn })
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
                                local msg = "mcp log: server="
                                    .. tostring(ev.server)
                                    .. " logger="
                                    .. tostring(ev.logger)
                                    .. " data="
                                    .. tostring(ev.data)
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
                        log.info(
                            "agent: server '" .. name .. "' has no logging capability; on_log/log_to_stderr skipped"
                        )
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
--- When active_groups is non-nil and non-empty, only tools whose group
--- matches one of the active groups are included. Tools without a group
--- are assigned to the "default" group. nil/empty = all tools (backwards compat).
--- @param mcp_tool_map table        MCP namespace map (may be empty)
--- @param extra_tools table         Additional Anthropic tool definitions (may be nil/empty)
--- @param active_groups table|nil   Array of group names to include (nil = all)
--- @return table                    Unified tools array in Anthropic format
local function build_tools(mcp_tool_map, extra_tools, active_groups)
    local tools = {}
    local seen = {}

    -- Build group lookup set (nil = no filtering)
    local group_set = nil
    if active_groups and #active_groups > 0 then
        group_set = {}
        for _, g in ipairs(active_groups) do
            group_set[g] = true
        end
    end

    local function passes_group(t)
        if not group_set then
            return true
        end
        local g = t.group or "default"
        return group_set[g] == true
    end

    local function add_unique(t)
        if seen[t.name] then
            return
        end
        if not passes_group(t) then
            return
        end
        seen[t.name] = true
        -- Strip internal `group` field before inserting into the API payload.
        -- `group` is used only for filtering (passes_group) and must not be
        -- forwarded to the Anthropic API (causes 400 "Extra inputs are not permitted").
        local def = {}
        for k, v in pairs(t) do
            if k ~= "group" then
                def[k] = v
            end
        end
        table.insert(tools, def)
    end

    -- 1. Registered Lua tools (highest priority)
    for _, t in ipairs(tool.schema()) do
        add_unique(t)
    end

    -- 2. MCP tools (already in Anthropic format from connect_mcp_servers)
    for _, entry in pairs(mcp_tool_map) do
        add_unique(entry.def)
    end

    -- 3. extra_tools (lowest priority, first-wins dedup).
    -- compile_loop.make() returns {name, schema={description, input_schema}, handler=<fn>}.
    -- The handler function is not JSON-serialisable; flatten to Anthropic flat form.
    if extra_tools then
        for _, t in ipairs(extra_tools) do
            if t.schema and t.handler then
                -- nested-schema+handler form → Anthropic flat form (strip handler)
                add_unique({
                    name = t.name,
                    description = t.schema.description,
                    input_schema = t.schema.input_schema,
                    group = t.group,
                })
            else
                add_unique(t)
            end
        end
    end

    return tools
end

-- ============================================================
-- Internal: Tool dispatch (unified)
-- ============================================================

--- Dispatch a tool call to MCP, extra_tools direct handler, or the local Lua registry.
--- Errors are returned as (content, is_error=true) instead of throwing.
--- @param name string              Tool name (possibly namespaced as "server__tool")
--- @param input table              Tool input from LLM
--- @param mcp_tool_map table       MCP namespace map
--- @param extra_tools_map table    extra_tools keyed by name (handler-bearing entries)
--- @return string                  Result content string
--- @return boolean                 is_error flag
local function dispatch_tool(name, input, mcp_tool_map, extra_tools_map)
    -- 1. MCP path (namespaced tools)
    if mcp_tool_map[name] then
        local entry = mcp_tool_map[name]
        local call_result =
            shape.assert_dev(mcp.call(entry.server, entry.tool, input), MCP_CALL_RESULT, "mcp.call result")
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

        -- Extract content from the MCP result. The rendering (single text
        -- block verbatim / none the empty string / anything else JSON) is
        -- llm_proto's, shared with knl_adapter's ToolPort.
        return proto.mcp_result_text(call_result.content), is_error
    end

    -- 2. extra_tools direct fallback (registry-independent; honours crux dispatch_tool wiring gap constraint)
    if extra_tools_map and extra_tools_map[name] then
        local entry = extra_tools_map[name]
        local ok, res = pcall(entry.handler, input)
        if not ok then
            return "tool error: " .. tostring(res), true
        end
        if type(res) == "table" then
            return std.json.encode(res), false
        end
        return tostring(res), false
    end

    -- 3. Fall back to registered Lua tool (tool.call registry)
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
---   history         (optional) Prior messages array (e.g. from session.load).
---                   When present, prepended before the new user prompt so the
---                   LLM sees the full conversation thread. Treated as opaque —
---                   trimming / compaction is the caller's responsibility.
---   extra_tools     (optional) Extra Anthropic tool definitions to include
---   provider        (optional) LLM provider: "anthropic" (default) | "openai".
---                   When "openai", routes to the OpenAI Chat Completions API shape
---                   (compatible with vLLM, llama.cpp, OpenRouter, RunPod, etc.).
---                   Default "anthropic" preserves full backward compatibility.
---   base_url        (optional) Base URL override for OpenAI-compatible endpoints.
---                   Only used when provider="openai".
---                   Default: "https://api.openai.com/v1"
---   api_key         (optional) Per-call API key override. When set, takes precedence
---                   over env var lookup. Useful for multi-provider setups where env
---                   variable names would collide.
---   api_key_env     (optional) Custom env var name for the API key.
---                   Default: "ANTHROPIC_API_KEY" (anthropic) / "OPENAI_API_KEY" (openai).
---   context_management        (optional, default true) When false, opt out of
---                   Anthropic server-side context editing entirely (no beta
---                   header, no body field). Any non-false value (nil, true,
---                   table) keeps it enabled.
---                   Anthropic-only: warn+ignored when provider="openai".
---   context_management_config (optional) Full override table passed as
---                   body.context_management. Defaults to DEFAULT_CONTEXT_MANAGEMENT
---                   (clear_tool_uses_20250919 with 80K/keep=3/clear>=10K).
---                   Ignored when context_management == false.
---                   Anthropic-only: warn+ignored when provider="openai".
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
---
--- The contract is checked on the way out in dev mode (LSHAPE_CHECK=1); see
--- `M.shapes.run_result`. Wrapped rather than asserted at each `return` because
--- there are five of them and a sixth added later would be the one that skips
--- the check.
function M.run(opts)
    return shape.assert_dev(M._run_impl(opts), RUN_RESULT, "agent.run result")
end

function M._run_impl(opts)
    opts = opts or {}

    -- Validate required fields
    if not opts.prompt or opts.prompt == "" then
        return {
            ok = false,
            error = "prompt is required",
            usage = { input_tokens = 0, output_tokens = 0, total_tokens = 0 },
            num_turns = 0,
            messages = {},
        }
    end

    -- Push parent LLM context so child tools (e.g. compile_loop) can inherit
    -- provider/model/api_key at call time without hard-coding defaults (Crux #2).
    table.insert(_AGENT_LLM_CTX, {
        provider = opts.provider,
        base_url = opts.base_url,
        api_key = opts.api_key,
        api_key_env = opts.api_key_env,
        model = opts.model,
    })

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
            -- Pop LLM context before early return (stack must stay balanced).
            table.remove(_AGENT_LLM_CTX)
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

    -- Build extra_tools_map for registry-independent dispatch (crux dispatch_tool wiring gap).
    -- Keyed by name; contains only entries that carry a handler function.
    local extra_tools_map = {}
    if opts.extra_tools then
        for _, t in ipairs(opts.extra_tools) do
            if t.name and t.handler then
                extra_tools_map[t.name] = t
            end
        end
    end

    -- Build unified tools array (tool_groups filter applied here)
    local tools = build_tools(mcp_tool_map, opts.extra_tools, opts.tool_groups)

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

    -- Anthropic-only knobs are warned about rather than dropped in silence.
    if (opts.provider or "anthropic") == "openai" then
        for _, name in ipairs({ "cache_control", "context_management", "context_management_config" }) do
            if opts[name] ~= nil then
                log.warn("agent: " .. name .. " is anthropic-only; ignored for provider=openai")
            end
        end
    end

    -- Wire options for the loop. `system` and `tools` are not here: tool_loop
    -- owns them (the tool set is re-resolved per turn).
    local llm_opts = {
        model = opts.model,
        max_tokens = opts.max_tokens or 4096,
        timeout = opts.timeout or 120,
        tool_choice = opts.tool_choice, -- nil = API default (auto)
        parallel_tool_calls = opts.parallel_tool_calls, -- false = at most one tool per turn
        thinking = opts.thinking, -- nil = provider default; see llm_proto
        dialect = opts.dialect, -- openai path: "openai" | "compat" (auto by base_url)
        extra_body = opts.extra_body, -- raw wire-body escape hatch
        -- Sampling / request knobs. Each adapter drops or renames what its
        -- provider does not accept rather than forwarding a doomed request.
        temperature = opts.temperature,
        top_p = opts.top_p,
        top_k = opts.top_k,
        stop = opts.stop,
        seed = opts.seed,
        n = opts.n,
        logit_bias = opts.logit_bias,
        logprobs = opts.logprobs,
        top_logprobs = opts.top_logprobs,
        frequency_penalty = opts.frequency_penalty,
        presence_penalty = opts.presence_penalty,
        metadata = opts.metadata,
        service_tier = opts.service_tier,
        store = opts.store,
        prompt_cache_key = opts.prompt_cache_key,
        safety_identifier = opts.safety_identifier,
        verbosity = opts.verbosity,
        response_format = opts.response_format,
        betas = opts.betas,
        context_management = cm_final, -- nil = opt-out, table = enabled
        -- Provider routing (new — additive, default nil = anthropic path)
        provider = opts.provider,
        base_url = opts.base_url,
        api_key = opts.api_key,
        api_key_env = opts.api_key_env,
        cache_control = opts.cache_control,
        -- Policy flag for the host JSONL dump sink (AGENT_BLOCK_LLM_DUMP_DIR).
        dump = (resolve_dump_mode_cached() == "full") and "full" or nil,
    }
    local log_meta = build_log_meta(opts)

    -- Initialize message history. When opts.history is provided (typically
    -- loaded via blocks/lib/session), prepend it before the new user prompt so
    -- the LLM sees the full thread. The block treats history as opaque —
    -- trimming / compaction is the caller's responsibility.
    local messages = {}
    if opts.history then
        if type(opts.history) ~= "table" then
            table.remove(_AGENT_LLM_CTX)
            return {
                ok = false,
                error = "history must be a table (messages array)",
                usage = { input_tokens = 0, output_tokens = 0, total_tokens = 0 },
                num_turns = 0,
                messages = {},
            }
        end
        for _, m in ipairs(opts.history) do
            table.insert(messages, m)
        end
    end
    table.insert(messages, { role = "user", content = opts.prompt })

    -- The loop itself lives in blocks/lib/tool_loop: one implementation of
    -- "call, dispatch, repeat" shared with every other block that iterates.
    -- What stays here is what makes this an agent rather than a loop — the
    -- registry- and MCP-backed tool set, the token budget, the iteration cap,
    -- and the dump.
    local specs = {}
    for _, t in ipairs(tools) do
        table.insert(specs, {
            name = t.name,
            description = t.description,
            input_schema = t.input_schema,
            handler = function(input)
                return dispatch_tool(t.name, input, mcp_tool_map, extra_tools_map)
            end,
        })
    end

    local on_request, on_response, on_summary = new_dump_hooks(opts, cm_final, log_meta)

    local res = tool_loop.run({
        prompt = opts.prompt,
        system = opts.system,
        messages = messages,
        tools = specs,
        llm = llm_opts,
        -- One over the cap: the loop's own bound is a backstop, because the
        -- cap below is what stops the run and it does so with ok = true.
        max_turns = max_iter + 1,
        max_retries = opts.max_retries,
        on_request = on_request,
        on_response = on_response,
        on_turn = function(info)
            on_summary(info.turn, info.decoded)
            budget:add(info.usage)

            if opts.on_turn then
                local cb_ok, cb_err = pcall(opts.on_turn, {
                    turn_number = info.turn,
                    content = info.decoded.content,
                    tool_calls = info.tool_calls,
                    usage = info.usage,
                    -- Pass-through of Anthropic response.context_management.
                    -- Nil when the server applied no edits this turn, which
                    -- drops the key and preserves the historical 4-key shape.
                    context_management = info.decoded.context_management,
                })
                if not cb_ok then
                    log.warn("agent: on_turn callback error: " .. tostring(cb_err))
                end
            end

            -- A turn that asked for nothing and was not paused ends the run
            -- on its own; stopping it here would only relabel why.
            local unfinished = #info.tool_calls > 0 or info.decoded.stop_reason == "pause_turn"
            if not unfinished then
                return
            end
            if info.turn >= max_iter then
                log.warn("agent: max iterations (" .. max_iter .. ") reached")
                return false
            end
            if budget:exceeded() then
                log.warn("agent: token budget exceeded (" .. budget.total_tokens .. "/" .. budget.limit .. ")")
                return false
            end
        end,
    })

    local num_turns = res.turns or 0
    local final_content = res.content or ""
    local loop_error = nil
    if not res.ok then
        loop_error = res.error
        -- A refusal names its category when the provider supplies one.
        if res.stop_reason == "refusal" and res.stop_details and res.stop_details.category then
            loop_error = loop_error .. " (category=" .. tostring(res.stop_details.category) .. ")"
        end
    end
    messages = res.messages or messages

    -- Pop parent LLM context (both success and error paths — stack must stay balanced).
    table.remove(_AGENT_LLM_CTX)

    -- Always disconnect MCP servers, regardless of loop outcome
    disconnect_mcp_servers(connected_servers)

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

M._build_tools = build_tools -- internal: for tests only

--- Resolve the group label for an MCP tool definition.
-- Priority:
--   1. tool JSON `_meta.group` (string, non-empty) — server-declared group
--   2. fallback: server_name (current behaviour)
--
-- rmcp 1.4.0 serialises Tool.meta as `_meta` (via #[serde(rename = "_meta")]),
-- so the Lua-side JSON key is always `_meta`.
--
-- @param tool_json   table   Raw tool object from mcp.list_tools
-- @param server_name string  MCP server name (fallback)
-- @return string             Resolved group label
local function resolve_mcp_group(tool_json, server_name)
    local meta = tool_json._meta
    if type(meta) == "table" then
        local g = meta.group
        if type(g) == "string" and g ~= "" then
            return g
        end
    end
    return server_name
end

M._resolve_mcp_group = resolve_mcp_group -- internal: for tests only

-- Bundle of pure internal helpers exposed for unit testing only.
-- These functions have no side effects beyond the std/log globals they read;
-- run behaviour is unchanged (this is a read-only accessor).
--- The contracts this module holds itself to, as data.
---
--- Public so a sibling block consuming `_log_meta` can check against the same
--- schema rather than a doc comment.
M.shapes = {
    log_meta = LOG_META,
    usage = USAGE,
    run_result = RUN_RESULT,
    mcp_call_result = MCP_CALL_RESULT,
}

function M._test_helpers()
    return {
        map_finish_reason = proto_openai.map_finish_reason,
        normalize_openai_response = proto_openai.parse,
        convert_messages_to_openai = proto_openai.convert_messages,
        new_budget_tracker = new_budget_tracker,
        count_tool_use_blocks = count_tool_use_blocks,
        count_text_chars = count_text_chars,
        normalize_dump_mode = normalize_dump_mode,
        sanitize_headers_for_dump = sanitize_headers_for_dump,
        kv_escape = kv_escape,
        format_kv = format_kv,
        build_tools = build_tools,
        resolve_mcp_group = resolve_mcp_group,
        dispatch_tool = dispatch_tool,
    }
end

return M
