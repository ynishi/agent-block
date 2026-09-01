--- llm_proto.anthropic — Anthropic Messages API dialect.
---
--- Translates the canonical (OpenAI-vocabulary) request spec into a
--- `POST /v1/messages` body, and normalizes the response back into the
--- canonical decoded shape.
---
--- Anthropic-specific concerns owned here:
---   * prompt caching (`cache_control` breakpoints on system + last tool)
---   * context-management beta header/body
---   * tool_choice object form + `disable_parallel_tool_use`
---   * extended thinking, including the manual/adaptive generation split
---   * thinking/redacted_thinking round-trip requirements during tool use
---
--- Reference: platform.claude.com/docs/en/build-with-claude/extended-thinking
---            platform.claude.com/docs/en/agents-and-tools/tool-use

local proto = require("llm_proto")

local M = { name = "anthropic" }

local DEFAULT_MODEL = "claude-haiku-4-5-20251001"
local API_URL = "https://api.anthropic.com/v1/messages"
local API_VERSION = "2023-06-01"

--- Anthropic's documented floor for a manual thinking budget.
local MIN_BUDGET_TOKENS = 1024

-- ============================================================
-- Model generation
-- ============================================================

--- Extract a (major, minor) generation from a Claude model id.
---
--- `claude-haiku-4-5-20251001` -> 4, 5
--- `claude-sonnet-4-6`         -> 4, 6
--- `claude-opus-5`             -> 5, 0
---
--- The trailing date stamp is ignored: only the first two numeric segments
--- are read, and a segment of 5+ digits is treated as a date, not a minor.
---
--- @param model string
--- @return number major  0 when the id carries no version
--- @return number minor
local function model_generation(model)
    local major, minor = nil, 0
    for seg in tostring(model or ""):gmatch("[^-]+") do
        if seg:match("^%d+$") then
            if #seg >= 5 then
                break -- date stamp (e.g. 20251001)
            end
            if major == nil then
                major = tonumber(seg)
            else
                minor = tonumber(seg)
                break
            end
        end
    end
    return major or 0, minor
end

--- Decide which extended-thinking request form the model accepts.
---
--- `{ type = "enabled", budget_tokens = N }` is the Claude 4.5-and-earlier
--- form; 4.7+ rejects it with a 400 and takes `{ type = "adaptive" }` plus
--- `output_config.effort` instead. Unversioned / unknown ids fall back to
--- adaptive, which is the forward-looking default.
---
--- @param model string
--- @return string  "enabled" | "adaptive"
local function thinking_form_for(model)
    local major, minor = model_generation(model)
    if major == 0 then
        return "adaptive"
    end
    if major > 4 then
        return "adaptive"
    end
    if major == 4 and minor >= 7 then
        return "adaptive"
    end
    return "enabled"
end

M._model_generation = model_generation
M._thinking_form_for = thinking_form_for

-- ============================================================
-- build
-- ============================================================

-- Prompt caching (on unless `cache_control = false`)
--
-- Two of the four available breakpoints are used, both on the stable prefix:
-- the system block (content-array form) and the last tool entry. The API
-- processes the prefix as tools -> system -> messages, and a marker caches
-- everything up to and including its own block, so the marker on system
-- covers [tools + system] no matter how the messages array grows across
-- turns. The remaining two slots are left for callers.
--
-- Minimum cacheable prefix is 1024 tokens (Sonnet / Opus) or 2048 (Haiku);
-- below that the marker is silently ignored — no cache_creation, no
-- cache_read, ordinary input billing. The documented floor is optimistic:
-- at ~1264 tokens on Sonnet we measured stochastic misses (turn 1 created
-- nothing and turn 2 read nothing), while ~1679 tokens fired deterministically.
-- Aim for =>1.5x the published minimum before depending on hits.
--
-- The cache key hashes the prefix bytes, so any whitespace, key-order, or
-- extra-field drift in tools or system invalidates it. `std.json.encode`
-- sorts keys, which keeps serialization stable; do not inject per-turn
-- timestamps, UUIDs, or counters into system or tool schemas.
--
-- Anthropic returns a non-spec `caller` field on tool_use blocks that gets
-- echoed back in later assistant messages. It does not affect matching:
-- cache_control sits ahead of the messages array, so message content is
-- outside the cached prefix.

--- Apply the prompt-cache breakpoint to the tools array.
---
--- Shallow-clones the list and its last entry so the caller's table is never
--- mutated across calls (the cache key is byte-exact; a leaked marker would
--- change the next request's prefix).
local function tools_with_cache_marker(tools)
    local out = {}
    for i = 1, #tools - 1 do
        out[i] = tools[i]
    end
    local last = {}
    for k, v in pairs(tools[#tools]) do
        last[k] = v
    end
    last.cache_control = { type = "ephemeral" }
    out[#tools] = last
    return out
end

--- Build an Anthropic Messages request.
---
--- @param spec table {
---   model, messages, system, tools, max_tokens, timeout,
---   tool_choice, parallel_tool_calls, thinking,
---   cache_control (default true), context_management, extra_body,
---   api_key, api_key_env, base_url
--- }
--- @return table|nil req  { url, headers, body }
--- @return string|nil err
function M.build(spec)
    spec = spec or {}

    local api_key = spec.api_key
    if not api_key then
        local key_env = spec.api_key_env or "ANTHROPIC_API_KEY"
        api_key = std.env.get(key_env)
        if not api_key then
            return nil, key_env .. " not set"
        end
    end

    local model = spec.model or std.env.get_or("ANTHROPIC_MODEL", DEFAULT_MODEL)
    local cache_on = spec.cache_control ~= false

    local body = {
        model = model,
        max_tokens = spec.max_tokens or 4096,
        messages = spec.messages or {},
    }

    if spec.system and spec.system ~= "" then
        if cache_on then
            body.system = {
                {
                    type = "text",
                    text = spec.system,
                    cache_control = { type = "ephemeral" },
                },
            }
        else
            body.system = spec.system
        end
    end

    if spec.tools and #spec.tools > 0 then
        body.tools = cache_on and tools_with_cache_marker(spec.tools) or spec.tools
    end

    local headers = {
        ["x-api-key"] = api_key,
        ["anthropic-version"] = API_VERSION,
        ["content-type"] = "application/json",
    }
    local betas = {}

    -- ── sampling ──────────────────────────────────────────────
    -- Models after Opus 4.6 accept only temperature 1.0, so a caller value is
    -- dropped there rather than turned into a 400. top_p / top_k are legacy
    -- knobs on those generations too.
    local major, minor = model_generation(model)
    local temperature_locked = major > 4 or (major == 4 and minor >= 7)
    if spec.temperature ~= nil then
        if temperature_locked and spec.temperature ~= 1 then
            log.warn("llm_proto.anthropic: model '" .. model .. "' accepts only temperature 1.0; dropped")
        else
            body.temperature = spec.temperature
        end
    end
    if spec.top_p ~= nil then
        body.top_p = spec.top_p
    end
    if spec.top_k ~= nil then
        body.top_k = spec.top_k
    end

    -- OpenAI calls this `stop`; the wire name here is stop_sequences.
    if spec.stop ~= nil then
        body.stop_sequences = type(spec.stop) == "table" and spec.stop or { spec.stop }
    end

    -- Anthropic's metadata carries a single `user_id`.
    if spec.metadata ~= nil then
        body.metadata = spec.metadata
    elseif spec.safety_identifier ~= nil then
        body.metadata = { user_id = spec.safety_identifier }
    end

    if spec.service_tier ~= nil then
        body.service_tier = spec.service_tier
    end

    -- Structured outputs. The canonical spec uses the OpenAI shape
    -- (`response_format`), which maps onto output_config.format here.
    if spec.response_format ~= nil then
        local rf = spec.response_format
        if type(rf) == "table" and rf.type == "json_schema" then
            local js = rf.json_schema or {}
            body.output_config = body.output_config or {}
            body.output_config.format = { type = "json_schema", schema = js.schema or js }
            table.insert(betas, "structured-outputs-2025-11-13")
        else
            log.warn("llm_proto.anthropic: response_format type not supported; ignored")
        end
    end

    -- ── thinking ──────────────────────────────────────────────
    local thinking, terr = proto.normalize_thinking(spec.thinking)
    if terr then
        return nil, terr
    end
    local thinking_manual = false
    if thinking and thinking.enabled then
        local form = thinking.mode
        if form == "auto" then
            form = thinking_form_for(model)
        end
        if form == "enabled" then
            local budget = thinking.budget_tokens or MIN_BUDGET_TOKENS
            if budget < MIN_BUDGET_TOKENS then
                return nil, "thinking.budget_tokens must be >= " .. MIN_BUDGET_TOKENS .. " (got " .. budget .. ")"
            end
            if budget >= body.max_tokens then
                return nil,
                    "thinking.budget_tokens (" .. budget .. ") must be less than max_tokens (" .. body.max_tokens .. ")"
            end
            body.thinking = { type = "enabled", budget_tokens = budget }
            thinking_manual = true
        else
            body.thinking = { type = "adaptive" }
            if thinking.effort then
                -- output_config may already carry a structured-output format.
                body.output_config = body.output_config or {}
                body.output_config.effort = thinking.effort
            end
        end
    end

    -- ── tool_choice ───────────────────────────────────────────
    local tc, tcerr = proto.normalize_tool_choice(spec.tool_choice)
    if tcerr then
        return nil, tcerr
    end
    if tc then
        -- Manual (non-adaptive) extended thinking rejects forced tool use.
        -- Catching it here turns a 400 round-trip into a local error.
        if thinking_manual and (tc.kind == "required" or tc.kind == "tool") then
            return nil,
                "tool_choice="
                    .. tc.kind
                    .. " is not supported while manual extended thinking is enabled "
                    .. "(model "
                    .. model
                    .. " uses thinking type=enabled; use tool_choice auto/none, "
                    .. "or thinking.mode='adaptive' on a model that supports it)"
        end
        if tc.kind == "required" then
            body.tool_choice = { type = "any" }
        elseif tc.kind == "tool" then
            body.tool_choice = { type = "tool", name = tc.name }
        else
            body.tool_choice = { type = tc.kind }
        end
    end

    -- Anthropic carries the parallel switch inside tool_choice, unlike the
    -- OpenAI top-level `parallel_tool_calls`.
    if spec.parallel_tool_calls == false then
        body.tool_choice = body.tool_choice or { type = "auto" }
        body.tool_choice.disable_parallel_tool_use = true
    end

    -- ── context management (beta) ─────────────────────────────
    if spec.context_management ~= nil then
        table.insert(betas, "context-management-2025-06-27")
        body.context_management = spec.context_management
    end

    -- Beta features share one comma-separated header.
    for _, b in ipairs(spec.betas or {}) do
        table.insert(betas, b)
    end
    if #betas > 0 then
        headers["anthropic-beta"] = table.concat(betas, ",")
    end

    if spec.extra_body and type(spec.extra_body) == "table" then
        for k, v in pairs(spec.extra_body) do
            body[k] = v
        end
    end

    return {
        url = spec.base_url and (spec.base_url .. "/v1/messages") or API_URL,
        headers = headers,
        body = body,
    },
        nil
end

-- ============================================================
-- parse
-- ============================================================

--- Normalize an Anthropic response into the canonical decoded shape.
---
--- The content array is passed through verbatim — including `thinking` and
--- `redacted_thinking` blocks. That is deliberate: during tool use Anthropic
--- requires the assistant turn to be echoed back unmodified, and dropping or
--- reordering those blocks is a 400.
---
--- @param raw table  Parsed response JSON
--- @return table|nil decoded { content, stop_reason, usage, context_management }
--- @return string|nil err
function M.parse(raw)
    if not raw then
        return nil, "invalid anthropic response: empty body"
    end
    if not raw.content then
        return nil, "anthropic response missing content blocks"
    end

    local u = raw.usage or {}
    local details = u.output_tokens_details or {}

    return {
        content = raw.content,
        stop_reason = raw.stop_reason,
        -- Present on refusals: { type = "refusal", category = ..., explanation = ... }
        stop_details = raw.stop_details,
        stop_sequence = raw.stop_sequence,
        usage = {
            input_tokens = tonumber(u.input_tokens) or 0,
            output_tokens = tonumber(u.output_tokens) or 0,
            cache_creation_input_tokens = tonumber(u.cache_creation_input_tokens) or 0,
            cache_read_input_tokens = tonumber(u.cache_read_input_tokens) or 0,
            thinking_tokens = tonumber(details.thinking_tokens) or 0,
        },
        context_management = raw.context_management,
    },
        nil
end

return M
