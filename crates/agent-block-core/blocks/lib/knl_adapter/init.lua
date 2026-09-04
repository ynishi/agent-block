--- knl_adapter — the Device IF backend for the Lua kernel, as a Port.
---
--- The interface, not a shim of literals
---   `knl.turn` calls `conf.llm(request)` and reads a *status string* off the
---   answer: "ok" | "refused" | "error". Turning an llm_proto answer into that
---   contract needs three provider-specific steps — build the wire, parse the
---   raw response, and judge the status — and only the last one carries a
---   provider's private vocabulary (how *this* provider says "I refused").
---
---   An earlier POC put that vocabulary (`stop_reason == "refusal"`) straight
---   into the shim as a literal. That leaks provider knowledge into the piece
---   that is meant to be provider-agnostic. The fix is a Port (hexagonal
---   Port/Adapter, realized here with metatable polymorphism): the interface
---   `LLMPort` fixes the three methods `build` / `parse` / `status`, the shared
---   shim `LLMPort:open` depends on the Port alone (no `== "refusal"`, no
---   status of its own), and each concrete provider closes its refusal
---   vocabulary inside its own `status()`. Provider knowledge lives in the
---   adapter by construction, not by convention.
---
--- What stays in llm_proto
---   The wire format, request building, response parsing, error classification
---   and retry policy all live in `llm_proto` and are reused, not reimplemented.
---   `build` / `parse` here are thin delegations to `llm_proto.adapter(...)`;
---   the retry loop reuses `llm_proto`'s exported `classify_error` /
---   `retry_delay`. llm_proto is untouched.
---
--- Why the shim does not call llm_proto's own backend closure
---   `llm_proto.backend` bundles build + POST + parse and hands back a reduced
---   result: it drops `stop_details` and reports the HTTP integer as `status`.
---   A Port's `status(result)` must see the *full* parse result to judge a
---   refusal, so the shim runs build -> http -> parse itself and passes the
---   whole parse result to `status`.
---
--- Usage
---   local knl_adapter = require("knl_adapter")
---   local llm = knl_adapter.anthropic:open({ model = ..., max_tokens = ... })
---   local ctx = knl.open(...)
---   local outcome = knl.turn({ ctx = ctx, llm = llm })
---
---   `open`'s conf is forwarded to llm_proto verbatim: model / api_key /
---   max_tokens / thinking / tool_choice / ... are llm_proto's vocabulary and
---   this module does not reinterpret any of them.

local proto = require("llm_proto")

local M = {}

--- Retries for transient API failures — mirrors llm_proto's own default,
--- because `llm_proto.post_with_retry` is module-local (not exported) so the
--- shim drives its own loop with the exported classify_error / retry_delay.
local DEFAULT_MAX_RETRIES = 2

--- Seconds a request may take when the conf does not say.
local DEFAULT_TIMEOUT = 120

--- Output cap when neither the request nor the conf names one.
local DEFAULT_MAX_TOKENS = 4096

-- ============================================================
-- LLMPort — the provider Port (interface)
-- ============================================================

--- The Port. `build` / `parse` / `status` are the per-provider methods; the
--- shared shim `open` is inherited by every instance through `__index`.
local LLMPort = {}
LLMPort.__index = LLMPort
M.LLMPort = LLMPort

--- Construct a Port instance from a provider impl.
---
--- @param impl table  { build, parse, status } — all three are required
---                     functions. `build(request, conf) -> wire | nil, err`,
---                     `parse(raw) -> result | nil, err`,
---                     `status(result) -> "ok" | "refused"`.
--- @return table port  a Port instance (impl with the LLMPort metatable)
function LLMPort.new(impl)
    if type(impl) ~= "table" then
        error("LLMPort.new: impl must be a table")
    end
    for _, method in ipairs({ "build", "parse", "status" }) do
        if type(impl[method]) ~= "function" then
            error("LLMPort.new: missing method: " .. method)
        end
    end
    return setmetatable(impl, LLMPort)
end

--- Open the Port into a knl backend: the closure `knl.turn` binds as
--- `conf.llm`. Shared by every Port instance — it knows no provider dialect,
--- it only calls `self:build` / `self:parse` / `self:status`. There is no
--- `== "refusal"` here and no status of its own; the one judgement it makes is
--- delegated to `self:status`.
---
--- @param conf table  Forwarded verbatim to the Port's build (model, api_key,
---                     max_tokens, thinking, tool_choice, ...). Shim-level keys:
---                     max_retries (default 2), timeout (default 120), dump.
--- @return function llm  function(request) -> resp | nil, err, where resp is
---                       { status, content, usage, stop_reason }
function LLMPort:open(conf)
    conf = conf or {}
    local port = self
    local max_retries = tonumber(conf.max_retries) or DEFAULT_MAX_RETRIES
    local timeout = conf.timeout or DEFAULT_TIMEOUT

    --- @param request table  knl.fold output: { messages, system?, tools? }.
    return function(request)
        -- build: neutral request -> provider wire { url, headers, body }.
        local wire, berr = port:build(request, conf)
        if not wire then
            return nil, berr
        end

        -- POST with retry. The loop is the shim's, but the policy is
        -- llm_proto's: classify_error decides whether a non-200 is worth
        -- retrying, retry_delay decides how long to wait. Nothing about the
        -- wire format or the parse is reimplemented here.
        local request_opts = {
            method = "POST",
            headers = wire.headers,
            body = std.json.encode(wire.body),
            timeout = timeout,
            dump = conf.dump,
        }
        local attempt = 0
        local resp
        while true do
            resp = http.request(wire.url, request_opts)
            if resp.status == 200 or attempt >= max_retries then
                break
            end
            local classified = proto.classify_error(resp.status, resp.body, resp.headers)
            if not classified.retryable then
                break
            end
            attempt = attempt + 1
            std.task.sleep(proto.retry_delay(attempt, classified, attempt) * 1000)
        end

        -- Non-200 after retries: the beat did not come off. turn maps the
        -- (nil, err) onto Outcome.err("call", err).
        if resp.status ~= 200 then
            local classified = proto.classify_error(resp.status, resp.body, resp.headers)
            return nil, "API error " .. tostring(resp.status) .. " (" .. classified.kind .. ")"
        end

        local ok_decode, raw = pcall(std.json.decode, resp.body)
        if not ok_decode then
            return nil, "response JSON decode failed"
        end

        -- parse: provider raw -> full neutral result (content, usage,
        -- stop_reason, and whatever else the parse carries, e.g. stop_details).
        -- The full result is what status() needs; that is why proto.backend is
        -- not used (it drops stop_details).
        local result, perr = port:parse(raw)
        if not result then
            return nil, perr
        end

        -- The status is the Port's judgement; content / usage / stop_reason
        -- ride verbatim from the parse. The shim adds no classification.
        return {
            status = port:status(result),
            content = result.content,
            usage = result.usage,
            stop_reason = result.stop_reason,
        }
    end
end

-- ============================================================
-- anthropic — a concrete LLMPort
-- ============================================================

--- The llm_proto anthropic adapter, captured once. build / parse below delegate
--- to it; llm_proto is not modified.
local proto_anthropic = proto.adapter("anthropic")

--- Merge conf and request into the one spec `llm_proto.adapter.build` wants —
--- the request wins field by field, mirroring `llm_proto.backend` — then
--- delegate. Thin: the merge is the only work; the wire format is llm_proto's.
---
--- @param request table  { messages, system?, tools? } from knl.fold
--- @param conf table  provider conf from `open`
--- @return table|nil wire  { url, headers, body }
--- @return string|nil err
local function anthropic_build(_, request, conf)
    local spec = {}
    for key, value in pairs(conf or {}) do
        spec[key] = value
    end
    for key, value in pairs(request or {}) do
        spec[key] = value
    end
    spec.max_tokens = (request and request.max_tokens) or (conf and conf.max_tokens) or DEFAULT_MAX_TOKENS
    return proto_anthropic.build(spec)
end

--- Delegate to llm_proto's anthropic parse. It already returns the full neutral
--- result (content / usage / stop_reason / stop_details / ...), so there is
--- nothing to add.
---
--- @param raw table  parsed response JSON
--- @return table|nil result
--- @return string|nil err
local function anthropic_parse(_, raw)
    return proto_anthropic.parse(raw)
end

--- Anthropic's refusal vocabulary, closed inside anthropic's status() — this is
--- the provider knowledge the shim must not hold. Anthropic signals a refusal
--- on the Messages API `stop_reason` enum (the value "refusal") and/or a
--- `stop_details` object of type "refusal". Read both, so the judgement is
--- robust to whichever the parse carried. Everything else — end_turn /
--- tool_use / max_tokens / stop_sequence — is "ok".
---
--- @param result table  a parse result
--- @return string  "refused" | "ok"
local function anthropic_status(_, result)
    if result.stop_reason == "refusal" then
        return "refused"
    end
    if type(result.stop_details) == "table" and result.stop_details.type == "refusal" then
        return "refused"
    end
    return "ok"
end

--- The anthropic Port. Provider knowledge (its refusal vocabulary) lives in
--- `status`; adding a second provider is one more `LLMPort.new{...}` and the
--- shim never changes.
M.anthropic = LLMPort.new({
    build = anthropic_build,
    parse = anthropic_parse,
    status = anthropic_status,
})

return M
