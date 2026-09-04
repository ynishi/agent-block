--- knl_adapter — the Device IF backend for the Lua kernel, as a Port.
---
--- The interface, not a shim of literals
---   `knl.beat` calls `device.llm(request)` and reads a *status string* off the
---   answer: "ok" | "refused" (a transport or provider failure is `nil, err`).
---   Turning an llm_proto answer into that contract needs three
---   provider-specific steps — build the wire, parse the raw response, and
---   classify the result — and only the last one carries a provider's private
---   vocabulary (how *this* provider says "I refused", and whether the refusal
---   was the model declining or a safety filter blocking).
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
---   `build` / `parse` here are thin delegations to `llm_proto.adapter(...)`,
---   and the whole middle of a call — POST, retries, the classified non-200,
---   the JSON decode — is `llm_proto.transport`, the same one `llm_proto.backend`
---   runs. The shim holds no transport policy of its own.
---
--- Why the shim does not call llm_proto's own backend closure
---   `llm_proto.backend` is the whole call in one value: it picks the adapter
---   from a provider NAME, and hands back a reduced result (it drops
---   `stop_details` and reports the HTTP integer as `status`). A Port is the
---   other decomposition — `build` / `parse` are the Port's own methods, which
---   is what lets a caller supply a provider this build has never heard of —
---   and `classify` must see the FULL parse result to judge a refusal. So the
---   Port delegates the middle (`transport`) and keeps the ends.
---
--- Usage
---   local knl = require("knl")
---   local adapter = require("knl_adapter")
---
---   local device = knl.device({
---       llm = adapter.anthropic:open({ model = ..., max_tokens = ... }),
---       tools = adapter.tools({ ... }),        -- flat specs or ToolPorts
---   })
---   knl.session({ owner = ..., budget = { amount = 8, tag = "beats" } },
---       function(s) return knl.beat(s, device) end)
---
---   The two halves stay apart: what this module builds is *policy* (the llm
---   closure, the tools map) and belongs to a device; the session is state and
---   is opened by the kernel.
---
---   `open`'s conf is forwarded to llm_proto verbatim: model / api_key /
---   max_tokens / thinking / tool_choice / ... are llm_proto's vocabulary and
---   this module does not reinterpret any of them.

local proto = require("llm_proto")
--- The kernel module, for its published contracts only (`knl.shapes`). Named
--- `kernel` because the bare `knl` is the syscall bridge global in a full host
--- VM, and shadowing it here would read as the wrong one.
local kernel = require("knl")

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

local M = {}

--- Output cap when neither the request nor the conf names one.
local DEFAULT_MAX_TOKENS = 4096

-- ============================================================
-- Result shape — the Port's clean-data contract
-- ============================================================

--- What `LLMPort:open`'s closure hands back is the kernel's `llm_result`, not
--- a shape of this module's own: it is the contract `knl.beat` reads at its
--- call step, so the kernel publishes it (`knl.shapes.llm_result`,
--- session-device-design.md §9-k) and every adapter is held to that one copy.
--- Two definitions of the same boundary is exactly the drift §11 R7 removed on
--- the event side.
---
--- It is closed, so a contract gap in one provider's parse cannot leak past
--- the Port boundary: `content` is an array of blocks (tagged as an empty
--- array by the Mapper when the model said nothing, so it crosses the JSON
--- bridge as `[]` and not `{}`), `usage` is a strict three-count, `stop_reason`
--- is absent when no reason was given, `status` is the Port's own "ok" |
--- "refused" verdict, and `refusal` carries the normalized reason — absent on
--- "ok", present with a `kind` on "refused".
local RESULT = kernel.shapes.llm_result

--- The contract this module holds itself to, as data — mirrors how `agent` and
--- `tool_loop` expose theirs via `M.shapes`. Re-exported rather than
--- redefined, so `knl_adapter.shapes.llm_result == knl.shapes.llm_result`.
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

--- Open the Port into a knl backend: the closure a device carries as its
--- `llm` (`knl.device{ llm = adapter.anthropic:open{...} }`), and the one
--- `knl.beat` calls. Shared by every Port instance — it knows no provider dialect,
--- it only calls `self:build` / `self:parse` / `self:classify`. There is no
--- `== "refusal"` here and no status of its own; the one judgement it makes is
--- delegated to `self:classify`.
---
--- @param conf table  Forwarded verbatim to the Port's build (model, api_key,
---                     max_tokens, thinking, tool_choice, ...). Shim-level keys
---                     (handed to `llm_proto.transport`, which owns their
---                     defaults): max_retries, timeout, dump.
--- @return function llm  function(request) -> resp | nil, err, where resp is
---                       { status, content, usage, stop_reason, refusal? }
function LLMPort:open(conf)
    conf = conf or {}
    local port = self

    --- @param request table  knl.fold output: { messages, system?, tools? }.
    return function(request)
        -- build: neutral request -> provider wire { url, headers, body }.
        local wire, berr = port:build(request, conf)
        if not wire then
            return nil, berr
        end

        -- transport: POST with retries, the classified non-200, the decode —
        -- all of it llm_proto's, none of it reimplemented here. It raises on
        -- a transport failure (that is what the host's http device does), and
        -- this closure's contract is `resp | nil, err`, so the raise is caught
        -- and returned rather than let out.
        local transported, raw, terr = pcall(proto.transport, wire, {
            max_retries = conf.max_retries,
            timeout = conf.timeout,
            dump = conf.dump,
        })
        if not transported then
            return nil, "http transport error: " .. tostring(raw)
        end
        -- A non-200 after retries, or a body that would not decode: the beat
        -- did not come off. beat records the (nil, err) as `llm_call_failed`
        -- and reports Outcome.err("call").
        if not raw then
            return nil, terr
        end

        -- parse: provider raw -> full neutral result (content, usage,
        -- stop_reason, and whatever else the parse carries, e.g. stop_details).
        -- The full result is what classify() needs, which is why the Port keeps
        -- this end instead of taking proto.backend's reduced one.
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

-- ============================================================
-- ToolPort — the tool-source Port (tool-port-design.md)
-- ============================================================
--
-- The tool-side sibling of LLMPort: a source's private vocabulary (how it
-- declares a tool, how its failures look) closes behind two methods, and
-- the generic binding below turns any Port into a knl tools entry without
-- a single source literal.
--
--   port:declare() -> { name, description?, input_schema? }  -- what may be called
--   port:invoke(args) -> result                              -- how; failure = raise
--
-- Today every real tool in this codebase is a Lua closure in the flat spec
-- shape ({name, description, input_schema, handler} — std.fs.tool_specs /
-- tool_loop / knl all agree), so the initial concrete Port is `ToolPort.lua`
-- alone: a validate-and-passthrough wrapper. There is no Rust-native
-- handler to bind (bridge/tool.rs is a Lua registry helper; handlers are
-- always LuaFunction), and MCP binding is the second real source, deferred
-- to its own ST.

--- The declare() contract — the same triple fold's wire_tools puts on the
--- request. Asserted in dev mode; `name` is checked loudly always (a
--- nameless tool is a construction error, not a policy).
local TOOL_DECL = T.shape({
    name = T.string,
    description = T.string:is_optional(),
    input_schema = T.any:is_optional(),
})

local ToolPort = {}
ToolPort.__index = ToolPort
M.ToolPort = ToolPort

--- Construct a Port instance from a source impl.
---
--- @param impl table  { declare, invoke } — both required functions.
---                     `declare() -> {name, description?, input_schema?}`,
---                     `invoke(args) -> result` (failure = raise; the
---                     source's failure vocabulary is normalized to a raise
---                     inside invoke, never surfaced as a convention).
--- @return table port  a Port instance (impl with the ToolPort metatable)
function ToolPort.new(impl)
    if type(impl) ~= "table" then
        error("ToolPort.new: impl must be a table")
    end
    for _, method in ipairs({ "declare", "invoke" }) do
        if type(impl[method]) ~= "function" then
            error("ToolPort.new: missing method: " .. method)
        end
    end
    return setmetatable(impl, ToolPort)
end

--- The lua Port: wrap one flat spec ({name, description?, input_schema?
--- (`schema` accepted as the legacy alias), handler}) — the shape
--- std.fs.tool_specs returns — into a Port. Pass-through: declare hands the
--- triple back verbatim, invoke calls the closure.
---
--- @param spec table  a flat tool spec
--- @return table port
function ToolPort.lua(spec)
    if type(spec) ~= "table" then
        error("ToolPort.lua: spec must be a table")
    end
    if type(spec.name) ~= "string" or spec.name == "" then
        error("ToolPort.lua: spec.name must be a non-empty string")
    end
    if type(spec.handler) ~= "function" then
        error("ToolPort.lua: spec.handler must be a function (tool '" .. spec.name .. "')")
    end
    local decl = {
        name = spec.name,
        description = spec.description,
        input_schema = spec.input_schema or spec.schema,
    }
    return ToolPort.new({
        declare = function()
            return decl
        end,
        invoke = function(_, args)
            return spec.handler(args)
        end,
    })
end

--- The mcp Port: one MCP tool (a `tools/list` entry) behind the Port. MCP's
--- private vocabulary closes inside: the camelCase `inputSchema`, the
--- `<server>__<tool>` namespacing, the content-block extraction, and both
--- failure forms — transport (`ok=false`) and server-reported
--- (`is_error=true`) — normalized to a raise, which the kernel closes as an
--- ok=false tool_result.
---
--- The two translations are `llm_proto`'s (`mcp_tool_decl` /
--- `mcp_result_text`), shared with the agent block, which binds the same
--- servers through its own loop. A namespace that differed between the two
--- would give one tool two names.
---
--- The `mcp` global is resolved at invoke time (the bridge's surface), not
--- captured at load: a VM without the bridge can still require this module.
---
--- @param server string  connected MCP server name
--- @param entry table  one entry from `mcp.list_tools(server).tools`
--- @return table port
function ToolPort.mcp(server, entry)
    if type(server) ~= "string" or server == "" then
        error("ToolPort.mcp: server must be a non-empty string")
    end
    if type(entry) ~= "table" or type(entry.name) ~= "string" or entry.name == "" then
        error("ToolPort.mcp: entry must be a tools/list item with a name")
    end
    local decl = proto.mcp_tool_decl(server, entry)
    return ToolPort.new({
        declare = function()
            return decl
        end,
        invoke = function(_, args)
            if mcp == nil then
                error("ToolPort.mcp: the mcp bridge is not available in this VM")
            end
            local r = mcp.call(server, entry.name, args)
            if type(r) ~= "table" or not r.ok then
                error(
                    "mcp call failed ('"
                        .. decl.name
                        .. "'): "
                        .. tostring(type(r) == "table" and r.error or "unreadable result")
                )
            end
            -- Content extraction, the same one the agent block runs.
            local text = proto.mcp_result_text(r.content)
            if r.is_error == true then
                error("mcp tool '" .. decl.name .. "' reported error: " .. tostring(text))
            end
            return text
        end,
    })
end

--- Source-level mcp binding: every tool a connected server lists, as Ports
--- (1 server = many tools). Feed the result to `knl_adapter.tools` —
--- name collisions with other sources land on its loud error.
---
--- @param server string  connected MCP server name
--- @param opts table|nil  { allow = { "<tool name>", ... } } — unlisted
---                        tools are skipped; absent allow = every tool
--- @return table ports  array of ToolPort
function M.mcp_tools(server, opts)
    if mcp == nil then
        error("knl_adapter.mcp_tools: the mcp bridge is not available in this VM")
    end
    local list = mcp.list_tools(server)
    if type(list) ~= "table" or not list.ok then
        error(
            "knl_adapter.mcp_tools: list_tools failed for '"
                .. tostring(server)
                .. "': "
                .. tostring(type(list) == "table" and list.error or "unreadable result")
        )
    end
    local allow = nil
    if opts and opts.allow then
        allow = {}
        for _, n in ipairs(opts.allow) do
            allow[n] = true
        end
    end
    local ports = {}
    for _, t in ipairs(list.tools or {}) do
        if allow == nil or allow[t.name] then
            ports[#ports + 1] = ToolPort.mcp(server, t)
        end
    end
    return ports
end

--- Bind one Port into a knl tools entry. Returns (name, entry) so `tools`
--- below can key the map. The handler closes over the Port — knl's
--- execute_tools sees a plain `fn(args)` and a raise from invoke closes the
--- pair as ok=false, exactly the kernel contract.
---
--- @param port table  a ToolPort
--- @return string name
--- @return table entry  { description?, input_schema?, handler }
function M.tool(port)
    if getmetatable(port) ~= ToolPort then
        error("knl_adapter.tool: not a ToolPort (build one with ToolPort.new / ToolPort.lua)")
    end
    local decl = port:declare()
    if type(decl) ~= "table" or type(decl.name) ~= "string" or decl.name == "" then
        error("knl_adapter.tool: declare() must return { name = <non-empty string>, ... }")
    end
    shape.assert_dev(decl, TOOL_DECL, "tool_decl")
    return decl.name,
        {
            description = decl.description,
            input_schema = decl.input_schema,
            handler = function(args)
                return port:invoke(args)
            end,
        }
end

--- Bind a list into the knl tools map (`name -> entry`). Items are
--- ToolPorts or flat specs (auto-wrapped through ToolPort.lua, so
--- `std.fs.tool_specs()` output drops in as-is). A duplicate name is a
--- loud error — two sources claiming one name is a wiring bug, not a
--- merge policy.
---
--- @param list table  array of ToolPort | flat spec
--- @return table tools  knl's `config.tools` map
function M.tools(list)
    if type(list) ~= "table" then
        error("knl_adapter.tools: list must be an array")
    end
    local out = {}
    for _, item in ipairs(list) do
        local port = getmetatable(item) == ToolPort and item or ToolPort.lua(item)
        local name, entry = M.tool(port)
        if out[name] ~= nil then
            error("knl_adapter.tools: duplicate tool name '" .. name .. "'")
        end
        out[name] = entry
    end
    return out
end

M.shapes.tool_decl = TOOL_DECL

return M
