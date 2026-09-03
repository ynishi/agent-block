--- llm_proto — provider-neutral LLM wire protocol layer.
---
--- Purpose
---   `blocks/agent` and `blocks/tools/compile_loop` both talk to LLM endpoints, and
---   both used to build their request bodies inline. The result was drift:
---   `tool_choice` existed only on the agent/Anthropic path, `thinking` existed
---   nowhere, and every provider quirk had to be fixed twice. This module owns
---   the wire format so the loop blocks only own the loop.
---
--- Model
---   The canonical request vocabulary is **OpenAI Chat Completions**
---   (`tool_choice = "auto" | "none" | "required" | { type = "function", name }`),
---   because that is the shape every OpenAI-compatible server speaks. Each
---   adapter translates that vocabulary into its own dialect and validates the
---   combinations its API rejects.
---
---   The canonical *message* vocabulary stays Anthropic-shaped (content-block
---   arrays), matching the internal history representation the loop blocks
---   already keep. Adapters convert on the way out.
---
--- Usage
---   local proto = require("llm_proto")
---   local ad = proto.adapter("anthropic")          -- or "openai"
---   local req, err = ad.build({ model = ..., messages = ..., tools = ...,
---                               tool_choice = "required",
---                               thinking = { effort = "medium" } })
---   -- req = { url, headers, body }   (body is a table; caller encodes)
---   local decoded, perr = ad.parse(raw_response_json)
---
--- Adapters are pure: they never perform I/O. What performs it is
--- `proto.backend(conf)`, which closes build, POST-with-retries and parse into
--- one function of a provider-neutral request — the shape a kernel session
--- binds and a loop without one calls directly, so "ask the model" has a
--- single implementation and its callers hold no provider knowledge.
---
---   local backend = proto.backend({ provider = "anthropic", model = ... })
---   local res, err = backend({ messages = ..., system = ..., tools = ... })

local M = {}

--- Protocol layer revision. Bumped when the canonical spec/decoded shape
--- changes in a way callers can observe.
M.VERSION = 1

-- ============================================================
-- tool_choice
-- ============================================================

--- Canonical tool_choice forms, provider-neutral.
---   { kind = "auto" }               -- model decides (API default when tools present)
---   { kind = "none" }               -- never call a tool
---   { kind = "required" }           -- must call some tool
---   { kind = "tool", name = "..." } -- must call this specific tool
---
--- Accepted inputs (both vocabularies, so existing call sites keep working):
---   OpenAI     "auto" | "none" | "required"
---              { type = "function", name = "x" }
---              { type = "function", function = { name = "x" } }
---   Anthropic  "auto" | "none" | "any"
---              { type = "auto" | "any" | "none" }
---              { type = "tool", name = "x" }
---
--- @param tc string|table|nil
--- @return table|nil canonical  nil when tc is nil (= leave to API default)
--- @return string|nil err
function M.normalize_tool_choice(tc)
    if tc == nil then
        return nil, nil
    end

    if type(tc) == "string" then
        if tc == "auto" or tc == "none" then
            return { kind = tc }, nil
        elseif tc == "required" or tc == "any" then
            return { kind = "required" }, nil
        end
        return nil, "invalid tool_choice string: " .. tc
    end

    if type(tc) ~= "table" then
        return nil, "tool_choice must be a string or table, got " .. type(tc)
    end

    local ty = tc.type
    if ty == "auto" or ty == "none" then
        return { kind = ty }, nil
    elseif ty == "any" then
        return { kind = "required" }, nil
    elseif ty == "required" then
        -- Not a real API form on either side, but an obvious caller intent.
        return { kind = "required" }, nil
    elseif ty == "tool" or ty == "function" then
        -- Anthropic: { type = "tool", name = "x" }
        -- OpenAI:    { type = "function", function = { name = "x" } }
        local name = tc.name
        if not name and type(tc["function"]) == "table" then
            name = tc["function"].name
        end
        if not name or name == "" then
            return nil, "tool_choice type=" .. tostring(ty) .. " requires a tool name"
        end
        return { kind = "tool", name = name }, nil
    end

    return nil, "invalid tool_choice: unknown type " .. tostring(ty)
end

-- ============================================================
-- thinking / reasoning
-- ============================================================

local VALID_EFFORT = {
    none = true,
    minimal = true,
    low = true,
    medium = true,
    high = true,
}

--- Canonical thinking spec, provider-neutral.
---   { enabled = bool, effort = string|nil, budget_tokens = number|nil,
---     mode = "auto"|"adaptive"|"enabled" }
---
--- Accepted inputs:
---   false / { enabled = false }   -- explicitly off (compat servers get
---                                    chat_template_kwargs.enable_thinking=false)
---   true                          -- on, provider default depth
---   { effort = "medium" }         -- on, effort-based depth
---   { budget_tokens = 8000 }      -- on, Anthropic manual budget
---   mode: "enabled"  force Anthropic manual form ({type="enabled",budget_tokens})
---         "adaptive" force Anthropic adaptive form ({type="adaptive"} + effort)
---         "auto"     (default) pick by model generation
---
--- @param t boolean|table|nil
--- @return table|nil canonical  nil when t is nil (= provider default, no field)
--- @return string|nil err
function M.normalize_thinking(t)
    if t == nil then
        return nil, nil
    end

    if type(t) == "boolean" then
        return { enabled = t, mode = "auto" }, nil
    end

    if type(t) ~= "table" then
        return nil, "thinking must be a boolean or table, got " .. type(t)
    end

    -- `enabled` defaults to true: passing any thinking table means "I want it",
    -- and turning it off is spelled `thinking = false` / `{ enabled = false }`.
    local enabled = true
    if t.enabled ~= nil then
        enabled = t.enabled and true or false
    end

    local effort = t.effort
    if effort ~= nil then
        if type(effort) ~= "string" or not VALID_EFFORT[effort] then
            return nil, "invalid thinking.effort: " .. tostring(effort)
        end
    end

    local budget = t.budget_tokens
    if budget ~= nil then
        budget = tonumber(budget)
        if not budget or budget <= 0 then
            return nil, "invalid thinking.budget_tokens: " .. tostring(t.budget_tokens)
        end
    end

    local mode = t.mode or "auto"
    if mode ~= "auto" and mode ~= "adaptive" and mode ~= "enabled" then
        return nil, "invalid thinking.mode: " .. tostring(mode)
    end

    return {
        enabled = enabled,
        effort = effort,
        budget_tokens = budget,
        mode = mode,
    },
        nil
end

-- ============================================================
-- Errors
-- ============================================================

--- Error codes that mean "you are out of budget", not "you are going too fast".
--- Both providers report these as 429, but retrying one of them succeeds in a
--- moment and retrying the other cannot succeed until the billing period
--- rolls over.
local QUOTA_CODES = {
    enforced_spend_limit_reached = true, -- Anthropic
    credit_balance_exhausted = true, -- OpenAI
    organization_spend_limit_exceeded = true,
    project_spend_limit_exceeded = true,
    organization_usage_limit_exceeded = true,
    insufficient_quota = true,
}

--- Classify an HTTP failure so callers can decide whether to retry.
---
--- @param status number       HTTP status
--- @param body string|table|nil  Response body (JSON text or decoded table)
--- @param headers table|nil   Response headers
--- @return table  { kind, retryable, retry_after (seconds|nil), code, message }
function M.classify_error(status, body, headers)
    local decoded = body
    if type(body) == "string" and body ~= "" then
        local ok, parsed = pcall(std.json.decode, body)
        decoded = ok and parsed or nil
    end
    if type(decoded) ~= "table" then
        decoded = {}
    end

    local err = decoded.error or {}
    local code = err.code
    if not code and type(err.details) == "table" then
        code = err.details.error_code
    end
    local message = err.message or ("HTTP " .. tostring(status))

    local retry_after
    for k, v in pairs(headers or {}) do
        if tostring(k):lower() == "retry-after" then
            retry_after = tonumber(v)
        end
    end

    local kind, retryable
    if status == 429 then
        if code and QUOTA_CODES[code] then
            kind, retryable = "quota", false
        else
            kind, retryable = "rate_limit", true
        end
    elseif status == 408 or status == 504 then
        kind, retryable = "timeout", true
    elseif status == 529 or status == 503 or status == 502 then
        kind, retryable = "overloaded", true
    elseif status >= 500 then
        kind, retryable = "server", true
    elseif status == 401 or status == 403 then
        kind, retryable = "auth", false
    elseif status == 404 then
        kind, retryable = "not_found", false
    elseif status >= 400 then
        kind, retryable = "invalid_request", false
    else
        kind, retryable = "unknown", false
    end

    return {
        kind = kind,
        retryable = retryable,
        retry_after = retry_after,
        code = code,
        message = message,
    }
end

--- Backoff delay in seconds for attempt N (1-based), honouring `retry-after`.
--- Exponential with a small deterministic spread so parallel agents that hit
--- the same limit do not line up on the same retry instant.
---
--- @param attempt number
--- @param classified table  Result of `classify_error`
--- @param salt number|nil   Distinguishes concurrent callers (e.g. call index)
--- @return number seconds
function M.retry_delay(attempt, classified, salt)
    if classified and classified.retry_after then
        return classified.retry_after
    end
    local base = math.min(2 ^ (attempt - 1), 30)
    local spread = ((salt or 0) % 5) / 10 -- 0.0 .. 0.4
    return base + spread
end

-- ============================================================
-- Headers
-- ============================================================

--- Merge caller-supplied headers into the ones an adapter built.
---
--- A request sometimes has to carry something the protocol does not model: the
--- browser `User-Agent` a RunPod proxy or a Cloudflare gate wants to see, a
--- gateway's routing header. Without a way through, the caller has to rebuild
--- the request by hand — which is how a second copy of the wire format starts.
---
--- The caller's value wins on a name collision, including the auth headers:
--- passing a header explicitly is a statement about the wire, and honouring
--- half of them would be the worse surprise.
---
--- @param headers table  The adapter's headers (mutated in place)
--- @param extra table|nil  Caller headers, name -> value
--- @return table headers
function M.merge_headers(headers, extra)
    if type(extra) ~= "table" then
        return headers
    end
    for name, value in pairs(extra) do
        headers[name] = tostring(value)
    end
    return headers
end

-- ============================================================
-- Adapter registry
-- ============================================================

local ADAPTERS = {
    openai = "llm_proto.openai",
    anthropic = "llm_proto.anthropic",
}

local loaded = {}

--- Resolve a provider name to its adapter module.
---
--- @param provider string|nil  "anthropic" (default) | "openai"
--- @return table|nil adapter  { name, build, parse }
--- @return string|nil err
function M.adapter(provider)
    local key = provider or "anthropic"
    local modname = ADAPTERS[key]
    if not modname then
        return nil, "unsupported provider: " .. tostring(provider)
    end
    if not loaded[key] then
        loaded[key] = require(modname)
    end
    return loaded[key], nil
end

--- List the provider names this build understands.
--- @return table  Array of provider name strings
function M.providers()
    local out = {}
    for name, _ in pairs(ADAPTERS) do
        table.insert(out, name)
    end
    table.sort(out)
    return out
end

-- ============================================================
-- Backend
-- ============================================================

--- Retries for transient API failures (rate limit / overload / 5xx).
local DEFAULT_MAX_RETRIES = 2

--- Output cap when neither the request nor the conf names one.
local DEFAULT_MAX_TOKENS = 4096

--- Seconds a request may take when the conf does not say.
local DEFAULT_TIMEOUT = 120

--- Conf keys that configure the closure rather than the request. They are
--- kept back when the adapter spec is assembled: an adapter ignores what it
--- does not know, but forwarding a callback as if it were a wire field is the
--- kind of thing that stops being harmless the day an adapter grows a field of
--- the same name.
local BACKEND_CONF = {
    max_retries = true,
    on_request = true,
    on_response = true,
    on_decoded = true,
}

--- POST with retries for the failures worth retrying.
---
--- Rate limits, overload and 5xx come back on their own; auth failures,
--- malformed requests and exhausted spend never will, so the classification
--- decides rather than the status class.
local function post_with_retry(url, request_opts, max_retries)
    local attempt = 0
    while true do
        local resp = http.request(url, request_opts)
        if resp.status == 200 or attempt >= max_retries then
            return resp
        end
        local classified = M.classify_error(resp.status, resp.body, resp.headers)
        if not classified.retryable then
            return resp
        end
        attempt = attempt + 1
        local delay = M.retry_delay(attempt, classified, attempt)
        log.warn(
            "llm_proto: "
                .. classified.kind
                .. " (HTTP "
                .. tostring(resp.status)
                .. "); retry "
                .. attempt
                .. "/"
                .. max_retries
        )
        std.task.sleep(delay * 1000)
    end
end

--- The content blocks to report for a model response.
---
--- An answer carries a non-empty array of blocks. An answer with no blocks at
--- all is reported as the one empty text block it amounts to, because an empty
--- Lua table crosses into the kernel as an empty mapping rather than an empty
--- array — and losing the response, with the usage it reports, would be the
--- worse trade.
local function response_blocks(content)
    if type(content) ~= "table" or #content == 0 then
        return { { type = "text", text = "" } }
    end
    return content
end

--- Build a model backend: one closure that turns a provider-neutral request
--- into an answer.
---
--- This is the whole transport in one value — wire format, retries, parse —
--- so a caller that wants a model call holds a function rather than a
--- provider. Two of them use it:
---
---   * the kernel, when a session is opened with `backend = ...`: `s:call(req)`
---     runs it and records what it returns
---   * a loop with no session, which calls it directly
---
--- so there is one implementation of "ask the model" and neither side carries
--- provider knowledge.
---
--- The closure answers `result | nil, err`, which is the contract `knl.call`
--- checks: `content` is a non-empty array of blocks, `usage` a table and
--- `stop_reason` a string. `status` and `latency_ms` ride along for callers
--- that want them; the kernel drops anything beyond the three.
---
--- @param conf table {
---   provider, model, api_key, api_key_env, base_url, headers, max_tokens,
---   timeout, dump, thinking, tool_choice, ... — forwarded to the adapter,
---   max_retries  (default 2) transient API failures only
---   on_request   function({ url, headers, body, body_json }) before the POST
---   on_response  function({ status, headers, body, latency_ms }) after it
---   on_decoded   function(decoded) with the adapter's parse, which carries
---                what the neutral answer does not (stop_details, provider
---                extras). Observability only: what they return is not read.
--- }
--- @return function|nil backend  function(req) -> result | nil, err
--- @return string|nil err  when the provider is not one this build speaks
function M.backend(conf)
    conf = conf or {}

    local adapter, aerr = M.adapter(conf.provider)
    if not adapter then
        return nil, aerr
    end

    -- Resolved once: the conf is fixed for the life of the closure, and only
    -- the request changes per call.
    local base = {}
    for key, value in pairs(conf) do
        if not BACKEND_CONF[key] then
            base[key] = value
        end
    end
    local max_retries = tonumber(conf.max_retries) or DEFAULT_MAX_RETRIES

    --- @param req table  { messages, system, tools, ... } — provider-neutral
    return function(req)
        req = req or {}

        -- The request wins over the conf, field by field: the conf says how to
        -- reach the provider, the request says what to ask it, and a caller
        -- that wants to override a knob for one call can.
        local spec = {}
        for key, value in pairs(base) do
            spec[key] = value
        end
        for key, value in pairs(req) do
            spec[key] = value
        end
        spec.max_tokens = req.max_tokens or conf.max_tokens or DEFAULT_MAX_TOKENS

        local built, berr = adapter.build(spec)
        if not built then
            return nil, berr
        end

        local body_json = std.json.encode(built.body)
        if conf.on_request then
            pcall(conf.on_request, {
                url = built.url,
                headers = built.headers,
                body = built.body,
                body_json = body_json,
            })
        end

        local started = std.time.now()
        local resp = post_with_retry(built.url, {
            method = "POST",
            headers = built.headers,
            body = body_json,
            timeout = conf.timeout or DEFAULT_TIMEOUT,
            dump = conf.dump,
        }, max_retries)
        local latency_ms = math.floor((std.time.now() - started) * 1000)

        if conf.on_response then
            pcall(conf.on_response, {
                status = resp.status,
                headers = resp.headers,
                body = resp.body,
                latency_ms = latency_ms,
            })
        end

        if resp.status ~= 200 then
            local classified = M.classify_error(resp.status, resp.body, resp.headers)
            return nil, "API error " .. tostring(resp.status) .. " (" .. classified.kind .. ")"
        end

        local ok_decode, raw = pcall(std.json.decode, resp.body)
        if not ok_decode then
            return nil, "response JSON decode failed"
        end

        local decoded, perr = adapter.parse(raw)
        if not decoded then
            return nil, perr
        end

        if conf.on_decoded then
            pcall(conf.on_decoded, decoded)
        end

        return {
            content = response_blocks(decoded.content),
            usage = decoded.usage or {},
            -- A string because the contract asks for one; a provider that
            -- names no reason still produced an answer, and refusing to
            -- record it over a missing label would be the worse failure.
            stop_reason = decoded.stop_reason or "",
            status = resp.status,
            latency_ms = latency_ms,
        }
    end,
        nil
end

M._response_blocks = response_blocks

return M
