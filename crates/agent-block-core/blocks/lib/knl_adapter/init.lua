--- knl_adapter — the Device IF backend for the Lua kernel, as a Port.
---
--- The interface, not a shim of literals
---   `knl.turn` calls `conf.llm(request)` and reads a *status string* off the
---   answer: "ok" | "refused" | "error". Turning an llm_proto answer into that
---   contract needs three provider-specific steps — build the wire, parse the
---   raw response, and classify the result — and only the last one carries a
---   provider's private vocabulary (how *this* provider says "I refused", and
---   whether the refusal was the model declining or a safety filter blocking).
---
---   An earlier POC put that vocabulary (`stop_reason == "refusal"`) straight
---   into the shim as a literal. That leaks provider knowledge into the piece
---   that is meant to be provider-agnostic. The fix is a Port (hexagonal
---   Port/Adapter, realized here with metatable polymorphism): the interface
---   `LLMPort` fixes the three methods `build` / `parse` / `classify`, the shared
---   shim `LLMPort:open` depends on the Port alone (no `== "refusal"`, no
---   status of its own), and each concrete provider closes its refusal
---   vocabulary inside its own `classify()`. `classify` returns both the status
---   and a normalized refusal detail from one method, so they cannot disagree.
---   Provider knowledge lives in the adapter by construction, not by convention.
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

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

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
-- Result shape — the Port's clean-data contract
-- ============================================================

--- The token accounting the Port promises: three counts, always present as
--- numbers. A provider that reports no usage (nil or {}) is normalized to
--- zeros by the Mapper, so this stays strict rather than admitting a missing
--- field. Closed so a stray usage key cannot ride across the boundary.
local USAGE = T.shape({
    input_tokens = T.number,
    output_tokens = T.number,
    thinking_tokens = T.number,
}, { open = false })

--- The refusal detail the Port surfaces alongside a "refused" status. `kind`
--- normalizes *why* the beat did not progress across providers: "model" is the
--- model declining, "content_filter" is a provider safety filter blocking it —
--- a distinction the kernel's 3-value status cannot carry, so it rides here.
--- `detail` is the provider's own refusal message when it gave one. Closed so a
--- stray key cannot cross the boundary. Present iff status == "refused" (the
--- Port's classify() is one method, so status and refusal cannot disagree).
local REFUSAL = T.shape({
    kind = T.one_of({ "model", "content_filter" }),
    detail = T.string:is_optional(),
}, { open = false })

--- What `LLMPort:open`'s closure hands back — the shape the kernel reads.
--- Closed (open=false) so a contract gap in one provider's parse cannot leak
--- past the Port boundary: `content` is an array of blocks (tagged as an empty
--- array by the Mapper when the model said nothing, so it crosses the JSON
--- bridge as `[]` and not `{}`), `usage` is the strict count above,
--- `stop_reason` is absent when no reason was given, `status` is the Port's own
--- "ok" | "refused" verdict, and `refusal` carries the normalized reason — absent
--- on "ok", present with a `kind` on "refused".
local RESULT = T.shape({
    content = T.array_of(T.table),
    usage = USAGE,
    stop_reason = T.string:is_optional(),
    status = T.one_of({ "ok", "refused" }),
    refusal = REFUSAL:is_optional(),
}, { open = false })

--- The contract this module holds itself to, as data — mirrors how `agent` and
--- `tool_loop` expose theirs via `M.shapes`.
M.shapes = { llm_result = RESULT }

-- ============================================================
-- LLMPort — the provider Port (interface)
-- ============================================================

--- The Port. `build` / `parse` / `classify` are the per-provider methods; the
--- shared shim `open` is inherited by every instance through `__index`.
local LLMPort = {}
LLMPort.__index = LLMPort
M.LLMPort = LLMPort

--- Construct a Port instance from a provider impl.
---
--- @param impl table  { build, parse, classify } — all three are required
---                     functions. `build(request, conf) -> wire | nil, err`,
---                     `parse(raw) -> result | nil, err`,
---                     `classify(result) -> { status = "ok" | "refused",
---                       refusal = { kind, detail? } | nil }` (refusal present
---                       iff status == "refused").
--- @return table port  a Port instance (impl with the LLMPort metatable)
function LLMPort.new(impl)
    if type(impl) ~= "table" then
        error("LLMPort.new: impl must be a table")
    end
    for _, method in ipairs({ "build", "parse", "classify" }) do
        if type(impl[method]) ~= "function" then
            error("LLMPort.new: missing method: " .. method)
        end
    end
    return setmetatable(impl, LLMPort)
end

--- Open the Port into a knl backend: the closure `knl.turn` binds as
--- `conf.llm`. Shared by every Port instance — it knows no provider dialect,
--- it only calls `self:build` / `self:parse` / `self:classify`. There is no
--- `== "refusal"` here and no status of its own; the one judgement it makes is
--- delegated to `self:classify`.
---
--- @param conf table  Forwarded verbatim to the Port's build (model, api_key,
---                     max_tokens, thinking, tool_choice, ...). Shim-level keys:
---                     max_retries (default 2), timeout (default 120), dump.
--- @return function llm  function(request) -> resp | nil, err, where resp is
---                       { status, content, usage, stop_reason, refusal? }
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
            local http_ok, resp_or_err = pcall(http.request, wire.url, request_opts)
            if not http_ok then
                return nil, "http transport error: " .. tostring(resp_or_err)
            end
            resp = resp_or_err
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

        -- Mapper (anti-corruption layer). The parse result is not handed across
        -- the Port boundary verbatim: it is normalized into the Port's own clean
        -- result shape (RESULT), so a contract gap in one provider — an empty
        -- `content` that would cross the JSON bridge as `{}`, a missing usage —
        -- cannot leak past the boundary.
        --
        -- Content that is not a table is an unreadable response: a runtime
        -- failure of the same class as a transport or parse failure, mapped onto
        -- the closure's (nil, err) contract — not a shape violation to assert on.
        if type(result.content) ~= "table" then
            return nil, "unreadable content: not an array"
        end
        -- The Port's own judgement — status and the normalized refusal detail
        -- come from one method, so they cannot disagree. The shim adds no
        -- classification and holds no provider literal.
        local verdict = port:classify(result)
        local usage = result.usage
        local mapped = {
            -- Tag an empty content as a JSON array so it crosses the host bridge
            -- as `[]`, not `{}`; a non-empty content passes through untouched.
            -- llm_proto owns the tag — the Mapper reuses its export rather than
            -- reimplementing the metatable.
            content = proto._response_blocks(result.content),
            -- A provider that named no counts (nil or {}) becomes zeros, so the
            -- strict USAGE shape holds without inventing a `{}`.
            usage = {
                input_tokens = tonumber(usage and usage.input_tokens) or 0,
                output_tokens = tonumber(usage and usage.output_tokens) or 0,
                thinking_tokens = tonumber(usage and usage.thinking_tokens) or 0,
            },
            -- Absent when the provider named no reason — kept nil, not
            -- fabricated to "".
            stop_reason = result.stop_reason,
            -- The Port's verdict: status plus, on a refusal, the normalized
            -- reason. `refusal` is nil on "ok" (absent from the RESULT shape).
            status = verdict.status,
            refusal = verdict.refusal,
        }
        -- Validate at the boundary. A violation here is a Mapper bug, so it
        -- raises in dev (LSHAPE_CHECK=1) and is a no-op in prod. Provider /
        -- transport failures never reach this line — they returned (nil, err).
        return shape.assert_dev(mapped, RESULT, "llm_result")
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

--- Anthropic's refusal vocabulary, closed inside anthropic's classify() — this
--- is the provider knowledge the shim must not hold. Anthropic signals a refusal
--- on the Messages API `stop_reason` enum (the value "refusal") and/or a
--- `stop_details` object of type "refusal". Read both, so the judgement is
--- robust to whichever the parse carried. An Anthropic refusal is always a model
--- refusal (kind = "model"): the Messages API has no content_filter finish
--- reason, so there is no safety-filter case to distinguish here. Everything
--- else — end_turn / tool_use / max_tokens / stop_sequence — is "ok".
---
--- @param result table  a parse result
--- @return table  { status = "refused", refusal = { kind = "model" } } | { status = "ok" }
local function anthropic_classify(_, result)
    if
        result.stop_reason == "refusal"
        or (type(result.stop_details) == "table" and result.stop_details.type == "refusal")
    then
        return { status = "refused", refusal = { kind = "model" } }
    end
    return { status = "ok" }
end

--- The anthropic Port. Provider knowledge (its refusal vocabulary) lives in
--- `classify`; adding a second provider is one more `LLMPort.new{...}` and the
--- shim never changes.
M.anthropic = LLMPort.new({
    build = anthropic_build,
    parse = anthropic_parse,
    classify = anthropic_classify,
})

-- ============================================================
-- openai — a second concrete LLMPort
-- ============================================================

--- The llm_proto openai adapter, captured once — the same accessor pattern the
--- anthropic port uses. build / parse below delegate to it; llm_proto is not
--- modified.
local proto_openai = proto.adapter("openai")

--- Merge conf and request into the one spec `llm_proto.adapter.build` wants —
--- the request wins field by field, mirroring `llm_proto.backend` and the
--- anthropic port — then delegate. Thin: the merge is the only work; the wire
--- format is llm_proto's.
---
--- @param request table  { messages, system?, tools? } from knl.fold
--- @param conf table  provider conf from `open`
--- @return table|nil wire  { url, headers, body }
--- @return string|nil err
local function openai_build(_, request, conf)
    local spec = {}
    for key, value in pairs(conf or {}) do
        spec[key] = value
    end
    for key, value in pairs(request or {}) do
        spec[key] = value
    end
    spec.max_tokens = (request and request.max_tokens) or (conf and conf.max_tokens) or DEFAULT_MAX_TOKENS
    return proto_openai.build(spec)
end

--- Delegate to llm_proto's openai parse. It already returns the full neutral
--- result (content / usage / stop_reason / refusal / stop_details / ...), so
--- there is nothing to add.
---
--- @param raw table  parsed response JSON
--- @return table|nil result
--- @return string|nil err
local function openai_parse(_, raw)
    return proto_openai.parse(raw)
end

--- OpenAI's refusal vocabulary, closed inside openai's classify() — the provider
--- knowledge the shim must not hold. OpenAI has two distinct ways to not
--- progress, and this is where the "model refusal vs safety filter" distinction
--- is drawn (review findings #3 + #4):
---
---   * a *model* refusal: llm_proto's openai parse maps a non-empty message
---     `refusal` string onto `stop_reason == "refusal"` and exposes that same
---     string on `result.refusal`. Match a non-empty refusal string OR the
---     mapped stop_reason; an empty-string refusal is NOT a refusal (it mirrors
---     openai.lua's own non-empty check). The refusal message rides in `detail`.
---     (kind = "model")
---   * a *content filter* block: OpenAI's `content_filter` finish_reason passes
---     through llm_proto's map_finish_reason unchanged, so it arrives as
---     `stop_reason == "content_filter"`. The model was stopped, not finished —
---     a refusal-to-progress, distinguished from a model refusal by its kind.
---     (kind = "content_filter")
---
--- Order: a model refusal is checked first, so if a non-empty refusal AND
--- content_filter somehow co-occur, the model refusal wins (deterministic).
--- Everything else the finish_reason map produces — end_turn / tool_use /
--- max_tokens / stop_sequence / stop / length — is "ok".
---
--- @param result table  a parse result
--- @return table  { status = "refused", refusal = { kind, detail? } } | { status = "ok" }
local function openai_classify(_, result)
    local refusal_msg = type(result.refusal) == "string" and result.refusal ~= "" and result.refusal or nil
    if refusal_msg or result.stop_reason == "refusal" then
        return { status = "refused", refusal = { kind = "model", detail = refusal_msg } }
    end
    if result.stop_reason == "content_filter" then
        return { status = "refused", refusal = { kind = "content_filter" } }
    end
    return { status = "ok" }
end

--- The openai Port. Provider knowledge (its refusal vocabulary) lives in
--- `classify`; this second provider is one more `LLMPort.new{...}` and the shim
--- (LLMPort / LLMPort:open) and M.anthropic are untouched.
M.openai = LLMPort.new({
    build = openai_build,
    parse = openai_parse,
    classify = openai_classify,
})

return M
