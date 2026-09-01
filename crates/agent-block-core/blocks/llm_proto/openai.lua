--- llm_proto.openai — OpenAI Chat Completions dialect (and compatibles).
---
--- Covers api.openai.com plus the OpenAI-compatible servers agent-block talks
--- to: vLLM, llama.cpp (`llama-server`), OpenRouter, RunPod. Those servers
--- share the endpoint shape but not the reasoning controls, so the adapter
--- carries a `dialect` axis:
---
---   dialect = "openai"  -> `reasoning_effort` (official Chat Completions param)
---   dialect = "compat"  -> `chat_template_kwargs.enable_thinking` (vLLM /
---                          llama.cpp convention; unknown to api.openai.com)
---
--- Auto-detection keys off `base_url`: anything that is not api.openai.com is
--- treated as compat, because sending `chat_template_kwargs` to OpenAI proper
--- is a 400 and sending `reasoning_effort` to a local server is at best ignored.
---
--- Reference: developers.openai.com/api/docs/guides/function-calling
---            docs.vllm.ai/en/latest/features/reasoning_outputs/
---            github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md

local proto = require("llm_proto")

local M = { name = "openai" }

local DEFAULT_MODEL = "gpt-4o-mini"
local DEFAULT_BASE_URL = "https://api.openai.com/v1"

-- ============================================================
-- Dialect
-- ============================================================

--- Pick the reasoning-control dialect for a base URL.
---
--- The servers disagree about how reasoning is switched on and off:
---   openai    `reasoning_effort`
---   vllm      `chat_template_kwargs` (+ `reasoning_effort`, + budget)
---   llamacpp  `chat_template_kwargs` and `reasoning_effort` together
---   ollama    `reasoning_effort` only — `chat_template_kwargs` is dropped
---             on its OpenAI-compatible endpoint, so sending only that
---             leaves thinking uncontrolled
---
--- `compat` is accepted as an alias for `vllm`.
---
--- @param spec table
--- @return string  "openai" | "vllm" | "llamacpp" | "ollama"
local function resolve_dialect(spec)
    if spec.dialect then
        return spec.dialect == "compat" and "vllm" or spec.dialect
    end
    local base = spec.base_url
    if not base or base:find("api.openai.com", 1, true) then
        return "openai"
    end
    if base:find("11434", 1, true) or base:lower():find("ollama", 1, true) then
        return "ollama"
    end
    return "vllm"
end

M._resolve_dialect = resolve_dialect

-- ============================================================
-- Model families
-- ============================================================

--- Does this model take `max_completion_tokens` instead of `max_tokens`?
---
--- `max_tokens` is rejected outright by the o-series and gpt-5 families, and
--- `max_completion_tokens` is rejected by older models that predate it, so the
--- field has to be chosen per model rather than fixed.
---
--- @param model string
--- @return boolean
local function is_reasoning_model(model)
    local m = tostring(model or ""):lower()
    return m:match("^o%d") ~= nil or m:match("^gpt%-5") ~= nil
end

--- Extract the major.minor version of a gpt-N.M model id.
--- @param model string
--- @return number major, number minor  0, 0 when the id carries no version
local function gpt_version(model)
    local major, minor = tostring(model or ""):lower():match("^gpt%-(%d+)%.(%d+)")
    if major then
        return tonumber(major), tonumber(minor)
    end
    major = tostring(model or ""):lower():match("^gpt%-(%d+)")
    if major then
        return tonumber(major), 0
    end
    return 0, 0
end

--- Does this model reject `tools` unless reasoning is switched off?
---
--- gpt-5.6 and later refuse function tools on Chat Completions while reasoning
--- is active — the documented options are `reasoning_effort: "none"` or the
--- Responses API. The default effort is not "none", so merely sending tools is
--- enough to trigger it.
---
--- @param model string
--- @return boolean
local function tools_need_effort_none(model)
    local major, minor = gpt_version(model)
    return major > 5 or (major == 5 and minor >= 6)
end

--- Which chat-template key toggles thinking for this model.
---
--- Most templates read `enable_thinking`; the DeepSeek-V3.1, Granite and
--- Holo families read plain `thinking`, and sending the wrong one is a
--- silent no-op rather than an error.
---
--- @param model string
--- @return string
local function thinking_kwarg_for(model)
    local m = tostring(model or ""):lower()
    if m:find("deepseek", 1, true) or m:find("granite", 1, true) or m:find("holo", 1, true) then
        return "thinking"
    end
    return "enable_thinking"
end

M._is_reasoning_model = is_reasoning_model
M._tools_need_effort_none = tools_need_effort_none
M._thinking_kwarg_for = thinking_kwarg_for

-- ============================================================
-- Message / tool conversion
-- ============================================================

--- Convert canonical (Anthropic-shaped) history to OpenAI messages.
---
--- Anthropic uses content-block arrays; OpenAI uses a flat list with
--- `tool_calls` on assistant messages and `role="tool"` for results.
--- Blocks that have no OpenAI equivalent (`thinking`, `redacted_thinking`)
--- are dropped — OpenAI-compatible servers have no round-trip requirement
--- for reasoning text, unlike Anthropic.
---
--- Reshape a message list for chat templates that only accept a strictly
--- alternating user/assistant transcript.
---
--- Gemma's template is the case that forces this: it has no `tool` role and
--- raises when two same-role turns are adjacent, so a tool-using conversation
--- otherwise fails at template render rather than at the API boundary. The
--- system turn is folded into the first user turn for the same reason.
---
--- @param messages table  OpenAI-shaped messages array
--- @return table
function M.flatten_for_strict_alternation(messages)
    local out = {}
    for _, msg in ipairs(messages or {}) do
        local role = msg.role
        local text = msg.content

        if role == "tool" then
            -- Tool results become part of the user side of the exchange.
            role = "user"
            text = "[tool result] " .. tostring(text or "")
        elseif role == "system" then
            role = "user"
        end

        if type(text) ~= "string" then
            text = text and std.json.encode(text) or ""
        end

        -- An assistant turn that only carried tool_calls has no text of its
        -- own; describing the call keeps the alternation intact.
        if role == "assistant" and text == "" and msg.tool_calls then
            local names = {}
            for _, tc in ipairs(msg.tool_calls) do
                table.insert(names, (tc["function"] or {}).name or "?")
            end
            text = "[tool call] " .. table.concat(names, ", ")
        end

        local prev = out[#out]
        if prev and prev.role == role then
            prev.content = prev.content .. "\n\n" .. text
        else
            table.insert(out, { role = role, content = text })
        end
    end
    return out
end

--- Does this model need `flatten_for_strict_alternation`?
--- @param model string
--- @return boolean
local function needs_strict_alternation(model)
    return tostring(model or ""):lower():find("gemma", 1, true) ~= nil
end

M._needs_strict_alternation = needs_strict_alternation

--- @param messages table  Canonical messages array
--- @param system string|nil
--- @return table  OpenAI-shaped messages array
function M.convert_messages(messages, system)
    local out = {}

    if system and system ~= "" then
        table.insert(out, { role = "system", content = system })
    end

    for _, msg in ipairs(messages or {}) do
        if type(msg.content) == "string" then
            table.insert(out, { role = msg.role, content = msg.content })
        elseif type(msg.content) == "table" then
            if msg.role == "assistant" then
                local text_parts = {}
                local tool_calls = {}
                for _, block in ipairs(msg.content) do
                    if block.type == "text" then
                        table.insert(text_parts, block.text or "")
                    elseif block.type == "tool_use" then
                        table.insert(tool_calls, {
                            id = block.id,
                            type = "function",
                            ["function"] = {
                                name = block.name,
                                arguments = std.json.encode(block.input or {}),
                            },
                        })
                    end
                end
                local text_content = #text_parts > 0 and table.concat(text_parts, "\n") or nil
                local oai_msg = { role = "assistant" }
                if text_content then
                    oai_msg.content = text_content
                end
                if #tool_calls > 0 then
                    oai_msg.tool_calls = tool_calls
                end
                table.insert(out, oai_msg)
            elseif msg.role == "user" then
                local has_tool_result = false
                for _, block in ipairs(msg.content) do
                    if block.type == "tool_result" then
                        has_tool_result = true
                        break
                    end
                end
                if has_tool_result then
                    for _, block in ipairs(msg.content) do
                        if block.type == "tool_result" then
                            table.insert(out, {
                                role = "tool",
                                tool_call_id = block.tool_use_id,
                                content = tostring(block.content or ""),
                            })
                        end
                    end
                else
                    local parts = {}
                    for _, block in ipairs(msg.content) do
                        if block.type == "text" then
                            table.insert(parts, block.text or "")
                        end
                    end
                    table.insert(out, { role = "user", content = table.concat(parts, "\n") })
                end
            else
                table.insert(out, { role = msg.role, content = msg.content })
            end
        end
    end

    return out
end

--- Convert Anthropic-shaped tool definitions to OpenAI function tools.
--- `cache_control` is stripped defensively — it is an Anthropic-only marker.
--- @param tools table|nil
--- @return table|nil
function M.convert_tools(tools)
    if not tools or #tools == 0 then
        return nil
    end
    local out = {}
    for _, t in ipairs(tools) do
        local fn = {
            name = t.name,
            description = t.description or "",
            parameters = t.input_schema or { type = "object", properties = {} },
        }
        -- Structured Outputs for tool arguments. The schema requirements
        -- (additionalProperties:false, every property required) are the
        -- caller's to satisfy; forcing them here would silently change the
        -- contract the model is shown.
        if t.strict ~= nil then
            fn.strict = t.strict
        end
        table.insert(out, { type = "function", ["function"] = fn })
    end
    return out
end

-- ============================================================
-- build
-- ============================================================

--- Build an OpenAI Chat Completions request.
---
--- @param spec table {
---   model, messages, system, tools, max_tokens, temperature, timeout,
---   tool_choice, parallel_tool_calls, thinking, dialect,
---   extra_body, api_key, api_key_env, base_url
--- }
--- @return table|nil req  { url, headers, body }
--- @return string|nil err
function M.build(spec)
    spec = spec or {}

    local api_key = spec.api_key
    if not api_key then
        local key_env = spec.api_key_env or "OPENAI_API_KEY"
        api_key = std.env.get(key_env)
        if not api_key then
            return nil, "API key not set: env=" .. key_env
        end
    end

    local model = spec.model or std.env.get_or("OPENAI_MODEL", DEFAULT_MODEL)
    local base_url = spec.base_url or DEFAULT_BASE_URL
    local dialect = resolve_dialect(spec)
    local reasoning_model = dialect == "openai" and is_reasoning_model(model)

    local messages = M.convert_messages(spec.messages, spec.system)
    local strict_alternation = spec.strict_alternation
    if strict_alternation == nil then
        strict_alternation = needs_strict_alternation(model)
    end
    if strict_alternation then
        messages = M.flatten_for_strict_alternation(messages)
    end

    local body = {
        model = model,
        messages = messages,
    }

    -- The o-series and gpt-5 families reject `max_tokens`; models older than
    -- the rename reject `max_completion_tokens`. Compatible servers only know
    -- the original name.
    if reasoning_model then
        body.max_completion_tokens = spec.max_tokens or 4096
    else
        body.max_tokens = spec.max_tokens or 4096
    end

    -- Reasoning models accept only the default for the sampling knobs and
    -- return 400 for anything else, so a caller-supplied value is dropped
    -- rather than turned into a failed request.
    local function sampling_ok(name, value)
        if value == nil then
            return false
        end
        if reasoning_model then
            log.warn("llm_proto.openai: " .. name .. " is not adjustable on reasoning model '" .. model .. "'; dropped")
            return false
        end
        return true
    end

    if sampling_ok("temperature", spec.temperature) then
        body.temperature = spec.temperature
    end
    if sampling_ok("top_p", spec.top_p) then
        body.top_p = spec.top_p
    end
    if sampling_ok("frequency_penalty", spec.frequency_penalty) then
        body.frequency_penalty = spec.frequency_penalty
    end
    if sampling_ok("presence_penalty", spec.presence_penalty) then
        body.presence_penalty = spec.presence_penalty
    end

    -- `top_k` is not an OpenAI parameter; vLLM and llama.cpp both take it.
    if spec.top_k ~= nil then
        if dialect == "openai" then
            log.warn("llm_proto.openai: top_k is not an OpenAI parameter; dropped")
        else
            body.top_k = spec.top_k
        end
    end

    if spec.stop ~= nil then
        body.stop = spec.stop
    end
    for _, key in ipairs({
        "seed",
        "n",
        "logit_bias",
        "logprobs",
        "top_logprobs",
        "metadata",
        "service_tier",
        "store",
        "prompt_cache_key",
        "safety_identifier",
        "verbosity",
        "response_format",
    }) do
        if spec[key] ~= nil then
            body[key] = spec[key]
        end
    end

    local oai_tools = M.convert_tools(spec.tools)
    if oai_tools then
        body.tools = oai_tools
    end

    -- ── tool_choice ───────────────────────────────────────────
    local tc, tcerr = proto.normalize_tool_choice(spec.tool_choice)
    if tcerr then
        return nil, tcerr
    end
    if tc then
        if tc.kind == "tool" then
            body.tool_choice = { type = "function", ["function"] = { name = tc.name } }
        else
            body.tool_choice = tc.kind -- "auto" | "none" | "required"
        end
    end
    -- Sent for `true` as well as `false`: llama.cpp keeps parallel tool calls
    -- off unless the request asks for them explicitly.
    if spec.parallel_tool_calls ~= nil then
        body.parallel_tool_calls = spec.parallel_tool_calls
    end

    -- ── thinking / reasoning ──────────────────────────────────
    local thinking, terr = proto.normalize_thinking(spec.thinking)
    if terr then
        return nil, terr
    end
    if thinking then
        local effort = thinking.effort or (not thinking.enabled and "none" or nil)

        -- `reasoning_effort` is understood everywhere except plain vLLM, where
        -- the chat-template switch is the reliable control.
        if dialect ~= "vllm" and effort then
            body.reasoning_effort = effort
        end

        -- Only the self-hosted servers read the chat-template switch: OpenAI
        -- rejects the unknown field, and Ollama's compatible endpoint drops
        -- it, which would merely look like the switch was set.
        if dialect == "vllm" or dialect == "llamacpp" then
            local key = thinking.kwarg or thinking_kwarg_for(model)
            body.chat_template_kwargs = body.chat_template_kwargs or {}
            body.chat_template_kwargs[key] = thinking.enabled and true or false
            if dialect == "vllm" and thinking.effort then
                body.reasoning_effort = thinking.effort
            end
        elseif thinking.enabled and not thinking.effort then
            -- Nothing else on this dialect expresses "reasoning on".
            body.reasoning_effort = "medium"
        end

        -- Opt-in only: a per-request budget exists on vLLM (with
        -- --reasoning-config); llama.cpp and Ollama only take a server flag.
        if thinking.budget_tokens then
            if dialect == "vllm" then
                body.thinking_token_budget = thinking.budget_tokens
            else
                log.warn(
                    "llm_proto.openai: thinking.budget_tokens has no per-request equivalent on dialect '"
                        .. dialect
                        .. "'; ignored"
                )
            end
        end
    end

    -- gpt-5.6 and later reject function tools on Chat Completions unless
    -- reasoning is off. The default effort is not "none", so tools alone
    -- trigger it; an explicit non-none effort cannot be satisfied here at all.
    if dialect == "openai" and body.tools and tools_need_effort_none(model) then
        local effort = body.reasoning_effort
        if effort and effort ~= "none" then
            return nil,
                "model "
                    .. model
                    .. " does not accept tools together with reasoning on Chat Completions; "
                    .. "use thinking = { effort = 'none' } (or drop the tools)"
        end
        body.reasoning_effort = "none"
    end

    if spec.extra_body and type(spec.extra_body) == "table" then
        for k, v in pairs(spec.extra_body) do
            body[k] = v
        end
    end

    return {
        url = base_url .. "/chat/completions",
        headers = {
            ["Authorization"] = "Bearer " .. api_key,
            ["Content-Type"] = "application/json",
        },
        body = body,
    },
        nil
end

-- ============================================================
-- parse
-- ============================================================

--- Map an OpenAI finish_reason to the canonical stop_reason vocabulary.
--- @param finish_reason string|nil
--- @return string
--- Split reasoning text that arrived inside the content instead of its own
--- field. The closing tag is searched from the end so nested `<think>` blocks
--- stay inside the reasoning half, and a missing opening tag is treated as
--- "everything up to the first close was thinking".
---
--- @param text string
--- @return string|nil thinking
--- @return string remaining_text
function M.split_think(text)
    local CLOSE = "</think>"

    -- Last close, not first: a nested block would otherwise cut the split in
    -- the middle of the reasoning.
    local close_s, pos = nil, 1
    while true do
        local s = text:find(CLOSE, pos, true)
        if not s then
            break
        end
        close_s, pos = s, s + 1
    end
    if not close_s then
        return nil, text
    end
    local close_e = close_s + #CLOSE - 1

    local open_s, open_e = text:find("<think>", 1, true)
    local thinking, rest
    if open_s and open_s < close_s then
        thinking = text:sub(open_e + 1, close_s - 1)
        rest = text:sub(1, open_s - 1) .. text:sub(close_e + 1)
    else
        thinking = text:sub(1, close_s - 1)
        rest = text:sub(close_e + 1)
    end

    return (thinking:gsub("^%s+", ""):gsub("%s+$", "")), (rest:gsub("^%s+", ""))
end

--- `content_filter` is deliberately left as its own value rather than folded
--- into `end_turn`: the model was stopped, it did not finish.
function M.map_finish_reason(finish_reason)
    if finish_reason == "stop" then
        return "end_turn"
    elseif finish_reason == "tool_calls" then
        return "tool_use"
    elseif finish_reason == "length" then
        return "max_tokens"
    else
        return tostring(finish_reason or "end_turn")
    end
end

--- Normalize an OpenAI chat completion into the canonical decoded shape.
---
--- Tolerances for OpenAI-compatible stacks:
---   * missing `tool_calls[].id` (Ollama native, some Gemini paths) — a
---     deterministic per-index id is synthesized so tool_use/tool_result
---     pairing survives downstream
---   * `arguments` delivered as a JSON object instead of a JSON string
---     (Ollama /api/chat, some vLLM tool-call parsers)
---   * reasoning text under either `reasoning_content` (DeepSeek/llama.cpp,
---     older vLLM) or `reasoning` (current vLLM), surfaced as a `thinking`
---     content block
---
--- @param raw table  Parsed response JSON
--- @return table|nil decoded { content, stop_reason, usage, context_management }
--- @return string|nil err
function M.parse(raw)
    if not raw or not raw.choices or #raw.choices == 0 then
        return nil, "invalid OpenAI response: missing choices"
    end
    local choice = raw.choices[1]
    local message = choice and choice.message
    if not message then
        return nil, "invalid OpenAI response: missing choices[0].message"
    end

    -- Raw assistant dump, gated by AGENT_BLOCK_DEBUG_RAW=1. Compatible servers
    -- disagree about where tool calls and reasoning end up, and the normalized
    -- blocks below hide that; this prints what actually arrived on the wire.
    if std.env.get("AGENT_BLOCK_DEBUG_RAW") == "1" then
        local text_raw = message.content
        log.info(
            "[DEBUG_RAW] content_len="
                .. tostring(text_raw and #text_raw or 0)
                .. " tool_calls="
                .. tostring(#(message.tool_calls or {}))
                .. " content_preview<<<"
                .. (text_raw or ""):sub(1, 1500)
                .. ">>>"
        )
        for i, tc in ipairs(message.tool_calls or {}) do
            local fn = tc["function"] or {}
            log.info(
                "[DEBUG_RAW] tool_call["
                    .. i
                    .. "] name="
                    .. tostring(fn.name)
                    .. " args="
                    .. tostring(fn.arguments or ""):sub(1, 500)
            )
        end
    end

    local content = {}

    -- Reasoning first, mirroring Anthropic's block ordering. Servers moving
    -- from `reasoning_content` to `reasoning` can send both, one of them
    -- empty, so the non-empty one wins rather than the first one present.
    local reasoning = nil
    if type(message.reasoning_content) == "string" and message.reasoning_content ~= "" then
        reasoning = message.reasoning_content
    elseif type(message.reasoning) == "string" and message.reasoning ~= "" then
        reasoning = message.reasoning
    end

    local text = message.content

    -- No reasoning field, but the tags are in the text: llama.cpp with
    -- `--reasoning-format none` leaves them there, and DeepSeek-R1 sometimes
    -- omits the opening tag so the parser never splits it out.
    if not reasoning and type(text) == "string" and text:find("</think>", 1, true) then
        local split, rest = M.split_think(text)
        reasoning, text = split, rest
    end

    if reasoning then
        table.insert(content, { type = "thinking", thinking = reasoning })
    end
    if text and text ~= "" then
        table.insert(content, { type = "text", text = text })
    end

    for i, tc in ipairs(message.tool_calls or {}) do
        local fn = tc["function"] or {}
        local tc_id = tc.id
        if tc_id == nil or tc_id == "" then
            -- Exactly 9 alphanumerics: Mistral's chat template rejects any
            -- other shape when the id comes back on a tool_result, and no
            -- other provider constrains the format.
            tc_id = string.format("tc%07d", i)
        end

        local input
        local raw_args = fn.arguments
        if type(raw_args) == "table" then
            input = raw_args
        else
            local ok, parsed = pcall(std.json.decode, raw_args or "{}")
            if ok and type(parsed) == "table" then
                input = parsed
            else
                log.warn(
                    "llm_proto.openai: tool_call arguments JSON parse failed for tool '"
                        .. tostring(fn.name)
                        .. "'; using empty input"
                )
                table.insert(content, {
                    type = "tool_use",
                    id = tc_id,
                    name = fn.name or "",
                    input = {},
                    is_error_hint = "arguments_parse_failed",
                })
                goto continue_tc
            end
        end
        table.insert(content, {
            type = "tool_use",
            id = tc_id,
            name = fn.name or "",
            input = input,
        })
        ::continue_tc::
    end

    local u = raw.usage or {}
    local out_details = u.completion_tokens_details or {}
    local in_details = u.prompt_tokens_details or {}

    -- A refusal comes back with content = null, which would otherwise read as
    -- an empty but successful turn.
    local stop_reason = M.map_finish_reason(choice.finish_reason)
    if type(message.refusal) == "string" and message.refusal ~= "" then
        stop_reason = "refusal"
    end

    return {
        content = content,
        stop_reason = stop_reason,
        refusal = message.refusal,
        stop_details = message.refusal and { type = "refusal", explanation = message.refusal } or nil,
        usage = {
            input_tokens = tonumber(u.prompt_tokens) or 0,
            output_tokens = tonumber(u.completion_tokens) or 0,
            -- Prompt caching is automatic on OpenAI; these mirror the
            -- Anthropic counters so callers read one shape.
            cache_creation_input_tokens = tonumber(in_details.cache_write_tokens) or 0,
            cache_read_input_tokens = tonumber(in_details.cached_tokens) or 0,
            thinking_tokens = tonumber(out_details.reasoning_tokens) or 0,
        },
        context_management = nil,
    },
        nil
end

return M
