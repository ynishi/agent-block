--- llm_proto — provider-neutral LLM wire protocol layer.
---
--- Purpose
---   Every block that talks to an LLM endpoint used to build its request body
---   inline, and more than one did. The result was drift:
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
    -- OpenAI-compat nests the explanation under `error.message`; vLLM answers
    -- with a top-level `message`. Either is the server saying what went wrong.
    local message = err.message or decoded.message or ("HTTP " .. tostring(status))

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

--- The backoff curve, as the SDKs have it: half a second doubling to a cap
--- of eight, less a deterministic share so parallel callers do not line up.
local RETRY_BASE_S = 0.5
local RETRY_CAP_S = 8

--- The longest a `retry-after` is believed. Past this the header is a
--- statement about the provider's afternoon, not about the next attempt,
--- and the curve stands (the Anthropic SDK draws the same line at 60s).
local RETRY_AFTER_MAX_S = 60

--- Backoff delay in seconds for attempt N (1-based), honouring `retry-after`
--- up to `RETRY_AFTER_MAX_S`.
---
--- The SDKs' curve — `RETRY_BASE_S * 2^(N-1)` held to `RETRY_CAP_S`, then
--- shortened by up to a quarter — with the shortening deterministic in
--- `salt` rather than drawn at random, so parallel agents that hit the
--- same limit do not line up on the same retry instant and a spec can say
--- what the number is. Five steps: whole, and 1/16 less each step down to
--- three quarters.
---
--- @param attempt number
--- @param classified table  Result of `classify_error`
--- @param salt number|nil   Distinguishes concurrent callers (e.g. call index)
--- @return number seconds
function M.retry_delay(attempt, classified, salt)
    local after = classified and classified.retry_after
    if type(after) == "number" and after > 0 and after <= RETRY_AFTER_MAX_S then
        return after
    end
    local base = math.min(RETRY_BASE_S * 2 ^ (attempt - 1), RETRY_CAP_S)
    local share = ((salt or 0) % 5) / 16 -- 0, 1/16 .. 4/16
    return base * (1 - share)
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

--- Retries for the failures worth asking again about: rate limit, overload,
--- 5xx, and a transport failure (connect refused, name lookup, a deadline,
--- a read cut) — the same set, and the same count, as the official SDKs'
--- default (Anthropic / OpenAI: two retries, connection errors and timeouts
--- included). Two, because the retry is held here and nowhere else: a
--- loop that retried on top of this would multiply the attempts (three
--- layers of three is twenty-seven), and the SRE reading is that the layer
--- right above the one refusing is the one that asks again.
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
--- decides rather than the status class. A failure with no answer at all —
--- the host's `http.request` raising on a refused connect, a name that
--- would not resolve, a deadline, a read cut short — is retried the same
--- way, as the SDKs do: a pod that is coming up answers the second time,
--- and a run that gave up on the first refusal would be failing on
--- something a second later is not true. When the retries are spent the
--- raise is let out as it came, which keeps the contract `transport`
--- states below (a transport failure RAISES).
---
--- What is not weighed here is whether the POST had side effects on the
--- server before the read was cut. The general clients (urllib3, Go's
--- net/http) refuse to retry a non-idempotent method past that point; the
--- LLM SDKs retry it, and so does this, on the same reading: a generation
--- the client never received cost a call and nothing else.
local function post_with_retry(url, request_opts, max_retries)
    local attempt = 0
    while true do
        local sent, resp = pcall(http.request, url, request_opts)
        local classified
        if sent then
            if resp.status == 200 or attempt >= max_retries then
                return resp
            end
            classified = M.classify_error(resp.status, resp.body, resp.headers)
            if not classified.retryable then
                return resp
            end
        else
            if attempt >= max_retries then
                error(resp, 0)
            end
            classified = { kind = "transport", retryable = true, message = tostring(resp) }
        end
        attempt = attempt + 1
        local delay = M.retry_delay(attempt, classified, attempt)
        log.warn(
            "llm_proto: "
                .. classified.kind
                .. (sent and (" (HTTP " .. tostring(resp.status) .. ")") or (" (" .. classified.message .. ")"))
                .. "; retry "
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
--- @param opts table|nil  { max_retries?, timeout?, on_request?,
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
        -- The server's own explanation rides along: a 400 that only says
        -- "invalid_request" leaves the caller with no way to tell a context
        -- overflow from a malformed body.
        return nil,
            "API error " .. tostring(resp.status) .. " (" .. classified.kind .. "): " .. tostring(classified.message)
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
---   * a block that wants a model call and no loop, which calls it directly
---     (the blocks this repository ships have all moved onto the kernel, so
---     the closure form is now the SDK's rather than one of theirs)
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
---   timeout, thinking, tool_choice, ... — forwarded to the adapter,
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

--- The empty-array tagging above, as an export: an adapter that builds a
--- response outside `M.backend` — knl_adapter's Port Mapper is the one in
--- tree — has the same empty content to say, and a second copy of the
--- metatable convention is a second thing to keep in step.
M.response_blocks = response_blocks

-- ============================================================
-- Counting and the window — what the adapters share
-- ============================================================
--
-- Each provider adapter answers two more questions beside build / parse:
-- `count(spec) -> tokens | nil, err` (how many tokens the request is, as the
-- server would count it) and `profile(spec) -> { context_window, max_output }
-- | nil, err` (what the model can take). Both are answered by the server's
-- own surface where it has one — Anthropic's count_tokens, vLLM's /tokenize
-- and /v1/models, llama.cpp's /apply-template + /tokenize and /props — and
-- `nil` where it does not (OpenAI's chat completions, Ollama), so the caller
-- falls back to an estimate it knows is one. The helpers below are the plain
-- one-shot HTTP the adapters do that with: no retry, a short timeout, the
-- error text on the way out. They are not `transport`, which is the model
-- call and carries its retry policy and hooks.

--- Timeout for a counting / discovery call: local servers answer in
--- milliseconds and a hosted one in well under this.
local PROBE_TIMEOUT = 15

--- One JSON round trip. `method` "GET" sends no body.
---
--- @param method string  "GET" | "POST"
--- @param url string
--- @param headers table
--- @param body table|nil  encoded as JSON when given
--- @param timeout number|nil
--- @return table|nil decoded
--- @return string|nil err
function M.probe(method, url, headers, body, timeout)
    local opts = {
        method = method,
        headers = headers,
        timeout = timeout or PROBE_TIMEOUT,
    }
    if body ~= nil then
        opts.body = std.json.encode(body)
    end
    local ok, resp = pcall(http.request, url, opts)
    if not ok then
        return nil, "probe " .. url .. ": " .. tostring(resp)
    end
    if resp.status ~= 200 then
        local classified = M.classify_error(resp.status, resp.body, resp.headers)
        return nil, "probe " .. url .. ": HTTP " .. tostring(resp.status) .. ": " .. tostring(classified.message)
    end
    local decoded_ok, decoded = pcall(std.json.decode, resp.body)
    if not decoded_ok or type(decoded) ~= "table" then
        return nil, "probe " .. url .. ": response JSON decode failed"
    end
    return decoded, nil
end

--- One round trip, read as a liveness answer rather than as data: what the
--- adapters' `health` is made of.
---
--- Unlike `probe`, nothing is decoded — a `/health` answers plain text or
--- nothing — and a non-200 is an answer, not an error: the status is what
--- says whether the server is up and not serving (503, loading or an engine
--- that died) or up and not this (404 on a route it does not have, 401 on
--- a key it does not take). Only a raise from the host's http device —
--- refused connect, no such name, deadline — is `nil, err`: nothing
--- answered.
---
--- @param url string
--- @param headers table|nil
--- @param timeout number|nil
--- @return table|nil answer  { status, body, headers }
--- @return string|nil err  when nothing answered
function M.ping(url, headers, timeout)
    local ok, resp = pcall(http.request, url, {
        method = "GET",
        headers = headers or {},
        timeout = timeout or PROBE_TIMEOUT,
    })
    if not ok then
        return nil, "ping " .. url .. ": " .. tostring(resp)
    end
    return { status = resp.status, body = resp.body, headers = resp.headers }, nil
end

--- What a `ping` answer says about the server, in the vocabulary every
--- adapter's `health` answers in:
---
---   ok           answered 200: up, and serving this
---   unavailable  answered 503: up, and not serving — a model still loading
---                (llama.cpp, TGI), an engine that died (vLLM)
---   down         answered anything else: reachable, and not usable as
---                configured — no such route, a key it will not take, a 5xx
---   unreachable  nothing answered: no process, no name, no route to it
---
--- `alive` is `kind == "ok"` and nothing subtler: a preflight asks one
--- question. The status and the first of the body ride along for the
--- record that says why a run was not started.
---
--- @param answer table|nil  from `ping`
--- @param err string|nil  from `ping`
--- @return table  { alive, kind, status?, message? }
function M.health_of(answer, err)
    if not answer then
        return { alive = false, kind = "unreachable", message = tostring(err) }
    end
    local kind
    if answer.status == 200 then
        kind = "ok"
    elseif answer.status == 503 then
        kind = "unavailable"
    else
        kind = "down"
    end
    local body = type(answer.body) == "string" and answer.body:sub(1, 200) or nil
    return { alive = kind == "ok", kind = kind, status = answer.status, message = body }
end

--- The server root a compatible server hangs its non-OpenAI endpoints off:
--- `/tokenize`, `/props`, `/api/show` live beside `/v1`, not under it.
---
--- @param base_url string  e.g. "http://localhost:8000/v1"
--- @return string root  e.g. "http://localhost:8000"
function M.server_root(base_url)
    local root = base_url:gsub("/+$", "")
    root = root:gsub("/v1$", "")
    return root
end

--- The estimate for a request no server will count: bytes over every string
--- in it, at `bytes_per_token`. The default of 3.2 is the safe side of what
--- was measured — 4 bytes to the token was 11-21% low against a vLLM-served
--- Qwen on code and tool schemas, and this margin covers the worst of those
--- calls — which is the only correctness an estimate can offer: a request it
--- passes fits. A caller that knows its model's ratio passes a larger value.
---
--- @param request table  knl.fold output: { messages, system?, tools? }
--- @param bytes_per_token number|nil  default 3.2
--- @return integer tokens
function M.estimate_tokens(request, bytes_per_token)
    local per = bytes_per_token or 3.2
    local bytes = 0
    local function walk(v)
        local t = type(v)
        if t == "string" then
            bytes = bytes + #v
        elseif t == "number" or t == "boolean" then
            bytes = bytes + 4
        elseif t == "table" then
            for k, child in pairs(v) do
                if type(k) == "string" then
                    bytes = bytes + #k
                end
                walk(child)
            end
        end
    end
    walk(request)
    return math.ceil(bytes / per)
end

return M
