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

--- Send one built request and hand back the provider's decoded JSON.
---
--- The transport step on its own: encode the body, POST it with the retry
--- policy this module owns, turn a non-200 into the classified error string,
--- and decode what came back. Everything either side of it — which wire to
--- build and how to read the decoded answer — belongs to the adapter.
---
--- It is exported because two callers need exactly this middle and differ at
--- the ends: `M.backend` below (adapter build -> transport -> adapter parse),
--- and `knl_adapter`'s LLMPort, whose `build` / `parse` are the Port's own
--- methods and whose `classify` needs the FULL parse result. Before this
--- existed the Port ran its own retry loop, its own non-200 message and its
--- own decode beside these — three copies of a policy that has to be one.
---
--- Failure is `nil, err` for anything the provider answered; a transport
--- failure RAISES, because that is what the host's `http.request` does and
--- turning it into a return here would make the two callers' error contracts
--- disagree. A caller that must not raise (the Port) pcalls this.
---
--- @param wire table  { url, headers, body } from an adapter's build
--- @param opts table|nil  { max_retries?, timeout?, dump?, on_request?,
---                          on_response? } — the two callbacks are
---                          observability only and their return is not read
--- @return table|nil raw  the decoded response JSON
--- @return string|nil err
--- @return table|nil meta  { status, latency_ms } on the success path
function M.transport(wire, opts)
    opts = opts or {}

    local body_json = std.json.encode(wire.body)
    if opts.on_request then
        pcall(opts.on_request, {
            url = wire.url,
            headers = wire.headers,
            body = wire.body,
            body_json = body_json,
        })
    end

    local started = std.time.now()
    local resp = post_with_retry(wire.url, {
        method = "POST",
        headers = wire.headers,
        body = body_json,
        timeout = opts.timeout or DEFAULT_TIMEOUT,
        dump = opts.dump,
    }, tonumber(opts.max_retries) or DEFAULT_MAX_RETRIES)
    local latency_ms = math.floor((std.time.now() - started) * 1000)

    if opts.on_response then
        pcall(opts.on_response, {
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

    return raw, nil, { status = resp.status, latency_ms = latency_ms }
end

--- Marks a table as a JSON array, for the one case Lua cannot express: an
--- empty table is an array and a mapping at once, and the host bridge reads
--- an untagged one as a mapping.
local ARRAY_TAG = { __jsontype = "array" }

--- The content blocks of a model response, as they arrived.
---
--- The one thing done to them is to say what an empty Lua table cannot say
--- for itself: no blocks is an empty *array*, not an empty mapping. An answer
--- that carried nothing is an answer providers do send, and what is recorded
--- has to be what was said — so no block is invented to stand in for it, and
--- the empty answer keeps the usage it reports.
---
--- This is about the record, not about the wire. A request that has to carry
--- an empty assistant turn back to a provider that will not take one is the
--- business of whatever builds that request; putting the fix here would put
--- a sentence in the history to satisfy a later HTTP call.
---
--- Anything that is not a table is handed on untouched: the kernel refuses it
--- and notes the call as failed, which is the honest ending for a response
--- nobody can read.
local function response_blocks(content)
    if type(content) == "table" and #content == 0 then
        return setmetatable({}, ARRAY_TAG)
    end
    return content
end

--- Build a model backend: one closure that turns a provider-neutral request
--- into an answer.
---
--- This is the whole transport in one value — wire format, retries, parse —
--- so a caller that wants a model call holds a function rather than a
--- provider. Two kinds of caller use it:
---
---   * `tool_loop` and the agent block, which call it directly
---   * `knl_adapter`, whose Port reuses the same pieces (build / parse /
---     classify_error / retry_delay) and hands the result to a knl device as
---     its `llm` — what `knl.beat(session, device)` then calls
---
--- so there is one implementation of "ask the model" and no side of it carries
--- provider knowledge.
---
--- The closure answers `result | nil, err`: `content` is an array of blocks
--- (empty when the model sent none), `usage` a table, and `stop_reason` a
--- string when the provider named one. `status` and `latency_ms` ride along
--- for callers that want them; the kernel's own boundary shape
--- (`knl.shapes.llm_result`) keeps only what a beat reads.
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

        -- The middle is `M.transport`, shared with knl_adapter's Port: POST
        -- with the retry policy, the classified non-200, the decode.
        local raw, terr, meta = M.transport(built, {
            max_retries = max_retries,
            timeout = conf.timeout,
            dump = conf.dump,
            on_request = conf.on_request,
            on_response = conf.on_response,
        })
        if not raw then
            return nil, terr
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
            -- Absent when the provider named no reason: the kernel takes it
            -- that way, and a label nobody sent would be a fact this file
            -- made up.
            stop_reason = decoded.stop_reason,
            status = meta.status,
            latency_ms = meta.latency_ms,
        }
    end,
        nil
end

M._response_blocks = response_blocks

-- ============================================================
-- MCP tools, in the neutral vocabulary
-- ============================================================
--
-- Two callers bind an MCP server's tools onto a model request — the agent
-- block's `connect_mcp_servers` and knl_adapter's `ToolPort.mcp` — and both
-- were doing the same two translations by hand: MCP's declaration into the
-- one a request carries, and an MCP call's content blocks into the text a
-- tool_result carries. Two copies of a NAMESPACE is the dangerous kind of
-- duplicate: the day they disagree, the same tool has two names and the
-- model's call finds neither.
--
-- They live here rather than in a module of their own because a new
-- `blocks/lib/*` has to be registered on the Rust side (host.rs
-- EMBEDDED_LIBS) to be require-able in the host at all, and this round does
-- not touch Rust. llm_proto is already required by both callers and already
-- owns the neutral shapes that go on the wire, so it is the honest home
-- until the registration can be made — at which point these two functions
-- move to `mcp_tools` unchanged.

--- One `tools/list` entry as the neutral tool declaration a request carries.
---
--- MCP's private vocabulary is closed here: the `<server>__<tool>` name that
--- keeps two servers' tools apart, the camelCase `inputSchema` under the
--- snake_case name every adapter build reads, an empty description rather
--- than a missing one, and an empty object schema for a server that declared
--- none (a provider will reject a tool with no schema at all).
---
--- @param server string  the connected server's name
--- @param entry table  one item of `mcp.list_tools(server).tools`
--- @return table decl  { name, description, input_schema }
function M.mcp_tool_decl(server, entry)
    return {
        name = server .. "__" .. entry.name,
        description = entry.description or "",
        input_schema = entry.inputSchema or entry.input_schema or { type = "object", properties = {} },
    }
end

--- An MCP call's content blocks as the text a tool_result carries: a single
--- text block verbatim, no blocks the empty string, anything else (several
--- blocks, or one that is not text) JSON-encoded so nothing is dropped.
---
--- @param blocks table|nil  `mcp.call(...).content`
--- @return string text
function M.mcp_result_text(blocks)
    blocks = blocks or {}
    if #blocks == 1 and blocks[1].type == "text" then
        return blocks[1].text
    elseif #blocks == 0 then
        return ""
    end
    return std.json.encode(blocks)
end

return M
