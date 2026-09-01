-- llm_proto_test.lua — mlua-lspec unit tests for blocks/llm_proto.
--
-- Run via:
--   mcp__lua-debugger__test_launch(
--     code_file    = "crates/agent-block/tests/fixtures/llm_proto_test.lua",
--     search_paths = ["crates/agent-block-core/blocks"]
--   )
--
-- Covers the pieces that used to be duplicated (or missing) across
-- blocks/agent and blocks/compile_loop:
--   * tool_choice normalization across both provider vocabularies
--   * Anthropic manual vs adaptive extended thinking, chosen by model generation
--   * the thinking x forced-tool-use combination Anthropic rejects
--   * OpenAI dialect split (reasoning_effort vs chat_template_kwargs)
--   * reasoning_content / reasoning surfaced as a thinking block
--
-- The runtime injects std / log as globals; the test harness has neither, so
-- minimal stubs are installed before the module is required.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Runtime global stubs (std / log)
-- ─────────────────────────────────────────────────────────────────────────────

local function json_encode(v)
    local t = type(v)
    if v == nil then
        return "null"
    elseif t == "boolean" or t == "number" then
        return tostring(v)
    elseif t == "string" then
        local s = v:gsub("\\", "\\\\"):gsub('"', '\\"'):gsub("\n", "\\n")
        return '"' .. s .. '"'
    elseif t == "table" then
        local n = 0
        for _ in pairs(v) do
            n = n + 1
        end
        if n == #v then
            local parts = {}
            for i = 1, #v do
                parts[i] = json_encode(v[i])
            end
            return "[" .. table.concat(parts, ",") .. "]"
        end
        local keys = {}
        for key in pairs(v) do
            keys[#keys + 1] = tostring(key)
        end
        table.sort(keys)
        local obj = {}
        for _, key in ipairs(keys) do
            obj[#obj + 1] = json_encode(key) .. ":" .. json_encode(v[key])
        end
        return "{" .. table.concat(obj, ",") .. "}"
    end
    return "null"
end

-- Only the shapes the adapters actually decode: flat objects of scalars.
local function json_decode(s)
    if type(s) ~= "string" then
        error("json_decode expects a string")
    end
    local out = {}
    local body = s:match("^%s*{(.*)}%s*$")
    if not body then
        error("json_decode: not an object: " .. tostring(s))
    end
    if body:match("^%s*$") then
        return out
    end
    for key, value in body:gmatch('"([^"]+)"%s*:%s*([^,]+)') do
        value = value:gsub("^%s+", ""):gsub("%s+$", "")
        local str = value:match('^"(.*)"$')
        if str then
            out[key] = str
        elseif value == "true" then
            out[key] = true
        elseif value == "false" then
            out[key] = false
        else
            out[key] = tonumber(value) or value
        end
    end
    return out
end

std = {
    json = { encode = json_encode, decode = json_decode },
    env = {
        get = function(_name)
            return nil
        end,
        get_or = function(_name, default)
            return default
        end,
    },
}
log = { warn = function() end, info = function() end, debug = function() end, error = function() end }

local proto = require("llm_proto")
local anthropic = proto.adapter("anthropic")
local openai = proto.adapter("openai")

-- Every build call needs credentials; the stub env has none, so pass explicitly.
local function a_build(spec)
    spec.api_key = spec.api_key or "test-key"
    spec.model = spec.model or "claude-haiku-4-5-20251001"
    return anthropic.build(spec)
end

local function o_build(spec)
    spec.api_key = spec.api_key or "test-key"
    spec.model = spec.model or "gpt-4o-mini"
    return openai.build(spec)
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("llm_proto.normalize_tool_choice", function()
    it("accepts the OpenAI vocabulary", function()
        expect(proto.normalize_tool_choice("auto").kind).to.equal("auto")
        expect(proto.normalize_tool_choice("none").kind).to.equal("none")
        expect(proto.normalize_tool_choice("required").kind).to.equal("required")
    end)

    it("accepts the Anthropic vocabulary", function()
        expect(proto.normalize_tool_choice("any").kind).to.equal("required")
        expect(proto.normalize_tool_choice({ type = "any" }).kind).to.equal("required")
        local tc = proto.normalize_tool_choice({ type = "tool", name = "grep" })
        expect(tc.kind).to.equal("tool")
        expect(tc.name).to.equal("grep")
    end)

    it("accepts both named-function spellings", function()
        local flat = proto.normalize_tool_choice({ type = "function", name = "grep" })
        expect(flat.name).to.equal("grep")
        local nested = proto.normalize_tool_choice({
            type = "function",
            ["function"] = { name = "grep" },
        })
        expect(nested.name).to.equal("grep")
    end)

    it("returns nil for nil (API default)", function()
        expect(proto.normalize_tool_choice(nil)).to.equal(nil)
    end)

    it("rejects unknown forms", function()
        local ok, err = proto.normalize_tool_choice("sometimes")
        expect(ok).to.equal(nil)
        expect(err ~= nil).to.be.truthy()

        local ok2, err2 = proto.normalize_tool_choice({ type = "tool" })
        expect(ok2).to.equal(nil)
        expect(err2 ~= nil).to.be.truthy()
    end)
end)

describe("llm_proto.normalize_thinking", function()
    it("treats a bare table as enabled", function()
        expect(proto.normalize_thinking({ effort = "medium" }).enabled).to.be.truthy()
    end)

    it("honours an explicit disable", function()
        expect(proto.normalize_thinking(false).enabled).to.equal(false)
        expect(proto.normalize_thinking({ enabled = false }).enabled).to.equal(false)
    end)

    it("rejects an unknown effort", function()
        local ok, err = proto.normalize_thinking({ effort = "extreme" })
        expect(ok).to.equal(nil)
        expect(err ~= nil).to.be.truthy()
    end)
end)

describe("llm_proto.anthropic tool_choice", function()
    it("maps required to type=any", function()
        local req = a_build({ tool_choice = "required" })
        expect(req.body.tool_choice.type).to.equal("any")
    end)

    it("maps a named function to type=tool", function()
        local req = a_build({ tool_choice = { type = "function", name = "grep" } })
        expect(req.body.tool_choice.type).to.equal("tool")
        expect(req.body.tool_choice.name).to.equal("grep")
    end)

    it("moves parallel_tool_calls=false into tool_choice", function()
        local req = a_build({ parallel_tool_calls = false })
        expect(req.body.tool_choice.type).to.equal("auto")
        expect(req.body.tool_choice.disable_parallel_tool_use).to.equal(true)
        expect(req.body.parallel_tool_calls).to.equal(nil)
    end)
end)

describe("llm_proto.anthropic caching", function()
    it("marks system and the last tool by default", function()
        local req = a_build({
            system = "be brief",
            tools = { { name = "a" }, { name = "b" } },
        })
        expect(req.body.system[1].cache_control.type).to.equal("ephemeral")
        expect(req.body.tools[2].cache_control.type).to.equal("ephemeral")
    end)

    it("leaves the caller's tools table untouched", function()
        local tools = { { name = "a" } }
        a_build({ tools = tools })
        expect(tools[1].cache_control).to.equal(nil)
    end)

    it("sends a plain system string when disabled", function()
        local req = a_build({ system = "be brief", cache_control = false })
        expect(req.body.system).to.equal("be brief")
    end)
end)

describe("llm_proto.anthropic thinking", function()
    it("reads the generation off the model id", function()
        local major, minor = anthropic._model_generation("claude-haiku-4-5-20251001")
        expect(major).to.equal(4)
        expect(minor).to.equal(5)
        expect(anthropic._model_generation("claude-opus-5")).to.equal(5)
    end)

    it("uses the manual form on 4.5-and-earlier models", function()
        local req = a_build({
            model = "claude-haiku-4-5-20251001",
            thinking = { budget_tokens = 2048 },
            max_tokens = 8192,
        })
        expect(req.body.thinking.type).to.equal("enabled")
        expect(req.body.thinking.budget_tokens).to.equal(2048)
    end)

    it("uses the adaptive form on 4.7+ and 5.x models", function()
        local req = a_build({ model = "claude-opus-5", thinking = { effort = "high" } })
        expect(req.body.thinking.type).to.equal("adaptive")
        expect(req.body.output_config.effort).to.equal("high")

        local req47 = a_build({ model = "claude-sonnet-4-7", thinking = true })
        expect(req47.body.thinking.type).to.equal("adaptive")
    end)

    it("honours an explicit mode override", function()
        local req = a_build({
            model = "claude-opus-5",
            thinking = { mode = "enabled", budget_tokens = 2048 },
            max_tokens = 8192,
        })
        expect(req.body.thinking.type).to.equal("enabled")
    end)

    it("rejects forced tool use while manual thinking is on", function()
        local req, err = a_build({
            model = "claude-haiku-4-5-20251001",
            thinking = { budget_tokens = 2048 },
            max_tokens = 8192,
            tool_choice = "required",
        })
        expect(req).to.equal(nil)
        expect(err ~= nil).to.be.truthy()
    end)

    it("allows forced tool use with adaptive thinking", function()
        local req = a_build({
            model = "claude-opus-5",
            thinking = { effort = "low" },
            tool_choice = "required",
        })
        expect(req.body.tool_choice.type).to.equal("any")
    end)

    it("rejects a budget that does not fit in max_tokens", function()
        local req, err = a_build({
            model = "claude-haiku-4-5-20251001",
            thinking = { budget_tokens = 9000 },
            max_tokens = 4096,
        })
        expect(req).to.equal(nil)
        expect(err ~= nil).to.be.truthy()
    end)

    it("omits the field entirely when unset", function()
        expect(a_build({}).body.thinking).to.equal(nil)
    end)
end)

describe("llm_proto.openai request", function()
    it("passes the tool_choice string through", function()
        expect(o_build({ tool_choice = "required" }).body.tool_choice).to.equal("required")
    end)

    it("maps the Anthropic named form to the OpenAI shape", function()
        local req = o_build({ tool_choice = { type = "tool", name = "grep" } })
        expect(req.body.tool_choice.type).to.equal("function")
        expect(req.body.tool_choice["function"].name).to.equal("grep")
    end)

    it("keeps parallel_tool_calls at the top level", function()
        expect(o_build({ parallel_tool_calls = false }).body.parallel_tool_calls).to.equal(false)
    end)

    it("converts input_schema to parameters", function()
        local req = o_build({
            tools = { { name = "grep", description = "d", input_schema = { type = "object" } } },
        })
        local fn = req.body.tools[1]["function"]
        expect(fn.name).to.equal("grep")
        expect(fn.parameters.type).to.equal("object")
    end)

    it("detects the dialect from base_url", function()
        expect(openai._resolve_dialect({})).to.equal("openai")
        expect(openai._resolve_dialect({ base_url = "https://api.openai.com/v1" })).to.equal("openai")
        expect(openai._resolve_dialect({ base_url = "http://localhost:8000/v1" })).to.equal("vllm")
    end)

    it("sends reasoning_effort on the OpenAI dialect and nothing else", function()
        local req = o_build({ thinking = { effort = "medium" } })
        expect(req.body.reasoning_effort).to.equal("medium")
        -- chat_template_kwargs is an unknown field to api.openai.com.
        expect(req.body.chat_template_kwargs).to.equal(nil)
        expect(o_build({ thinking = true }).body.chat_template_kwargs).to.equal(nil)
    end)

    it("sends enable_thinking on compatible servers", function()
        local off = o_build({ base_url = "http://localhost:8000/v1", thinking = false })
        expect(off.body.chat_template_kwargs.enable_thinking).to.equal(false)
        expect(off.body.reasoning_effort).to.equal(nil)

        local on = o_build({ base_url = "http://localhost:8000/v1", thinking = true })
        expect(on.body.chat_template_kwargs.enable_thinking).to.equal(true)
    end)
end)

describe("llm_proto.openai model families", function()
    it("sends max_completion_tokens to reasoning models", function()
        local req = o_build({ model = "gpt-5.2", max_tokens = 512 })
        expect(req.body.max_completion_tokens).to.equal(512)
        expect(req.body.max_tokens).to.equal(nil)

        local o3 = o_build({ model = "o3-mini", max_tokens = 512 })
        expect(o3.body.max_completion_tokens).to.equal(512)
    end)

    it("keeps max_tokens for non-reasoning and compatible servers", function()
        expect(o_build({ model = "gpt-4o-mini", max_tokens = 512 }).body.max_tokens).to.equal(512)
        local compat = o_build({
            model = "gpt-5.2",
            base_url = "http://localhost:8000/v1",
            max_tokens = 512,
        })
        expect(compat.body.max_tokens).to.equal(512)
        expect(compat.body.max_completion_tokens).to.equal(nil)
    end)

    it("drops sampling knobs reasoning models reject", function()
        local req = o_build({ model = "gpt-5.2", temperature = 0.2, top_p = 0.5 })
        expect(req.body.temperature).to.equal(nil)
        expect(req.body.top_p).to.equal(nil)
    end)

    it("forces reasoning off when gpt-5.6+ is given tools", function()
        local req = o_build({ model = "gpt-5.6", tools = { { name = "grep" } } })
        expect(req.body.reasoning_effort).to.equal("none")
    end)

    it("rejects gpt-5.6+ tools combined with an explicit effort", function()
        local req, err = o_build({
            model = "gpt-5.6",
            tools = { { name = "grep" } },
            thinking = { effort = "high" },
        })
        expect(req).to.equal(nil)
        expect(err ~= nil).to.be.truthy()
    end)

    it("leaves earlier gpt-5 models alone", function()
        local req = o_build({ model = "gpt-5.2", tools = { { name = "grep" } } })
        expect(req.body.reasoning_effort).to.equal(nil)
    end)

    it("routes top_k only to compatible servers", function()
        expect(o_build({ top_k = 40 }).body.top_k).to.equal(nil)
        expect(o_build({ base_url = "http://localhost:8000/v1", top_k = 40 }).body.top_k).to.equal(40)
    end)

    it("passes tool strict through", function()
        local req = o_build({ tools = { { name = "grep", strict = true } } })
        expect(req.body.tools[1]["function"].strict).to.equal(true)
    end)
end)

describe("llm_proto.openai dialects", function()
    it("detects ollama from the base_url", function()
        expect(openai._resolve_dialect({ base_url = "http://localhost:11434/v1" })).to.equal("ollama")
        expect(openai._resolve_dialect({ base_url = "http://localhost:8000/v1" })).to.equal("vllm")
        expect(openai._resolve_dialect({ dialect = "compat" })).to.equal("vllm")
    end)

    it("uses reasoning_effort only on ollama", function()
        local off = o_build({ base_url = "http://localhost:11434/v1", thinking = false })
        expect(off.body.reasoning_effort).to.equal("none")
        expect(off.body.chat_template_kwargs).to.equal(nil)

        local on = o_build({ base_url = "http://localhost:11434/v1", thinking = true })
        expect(on.body.reasoning_effort).to.equal("medium")
    end)

    it("sends both controls on llama.cpp", function()
        local req = o_build({ dialect = "llamacpp", thinking = false })
        expect(req.body.reasoning_effort).to.equal("none")
        expect(req.body.chat_template_kwargs.enable_thinking).to.equal(false)
    end)

    it("picks the thinking kwarg name per model family", function()
        local qwen = o_build({ dialect = "vllm", model = "qwen3-32b", thinking = false })
        expect(qwen.body.chat_template_kwargs.enable_thinking).to.equal(false)

        local ds = o_build({ dialect = "vllm", model = "deepseek-v3.1", thinking = false })
        expect(ds.body.chat_template_kwargs.thinking).to.equal(false)
        expect(ds.body.chat_template_kwargs.enable_thinking).to.equal(nil)
    end)

    it("keeps the thinking budget on vllm only", function()
        local vllm = o_build({ dialect = "vllm", thinking = { budget_tokens = 2048 } })
        expect(vllm.body.thinking_token_budget).to.equal(2048)

        local lcpp = o_build({ dialect = "llamacpp", thinking = { budget_tokens = 2048 } })
        expect(lcpp.body.thinking_token_budget).to.equal(nil)
    end)

    it("sends parallel_tool_calls in both directions", function()
        expect(o_build({ parallel_tool_calls = true }).body.parallel_tool_calls).to.equal(true)
        expect(o_build({ parallel_tool_calls = false }).body.parallel_tool_calls).to.equal(false)
    end)
end)

describe("llm_proto.openai strict alternation (Gemma)", function()
    local history = {
        { role = "user", content = "fix it" },
        {
            role = "assistant",
            content = { { type = "tool_use", id = "t1", name = "grep", input = {} } },
        },
        {
            role = "user",
            content = { { type = "tool_result", tool_use_id = "t1", content = "hit" } },
        },
    }

    it("turns on automatically for gemma models", function()
        expect(openai._needs_strict_alternation("gemma-3-27b-it")).to.be.truthy()
        expect(openai._needs_strict_alternation("qwen3-32b")).to_not.be.truthy()
    end)

    it("removes the tool role and merges same-role turns", function()
        local req = o_build({ model = "gemma-3-27b-it", messages = history, system = "be brief" })
        local roles = {}
        for _, m in ipairs(req.body.messages) do
            table.insert(roles, m.role)
        end
        expect(table.concat(roles, ",")).to.equal("user,assistant,user")
        expect(req.body.messages[1].content:find("be brief", 1, true) ~= nil).to.be.truthy()
        expect(req.body.messages[3].content:find("hit", 1, true) ~= nil).to.be.truthy()
    end)

    it("leaves other models untouched", function()
        local req = o_build({ model = "qwen3-32b", messages = history })
        expect(req.body.messages[3].role).to.equal("tool")
    end)

    it("can be forced off for a gemma variant that supports tools", function()
        local req = o_build({ model = "gemma-3-27b-it", messages = history, strict_alternation = false })
        expect(req.body.messages[3].role).to.equal("tool")
    end)
end)

describe("llm_proto error classification", function()
    it("separates rate limits from exhausted quota", function()
        local rl = proto.classify_error(429, { error = { type = "rate_limit_error" } })
        expect(rl.kind).to.equal("rate_limit")
        expect(rl.retryable).to.be.truthy()

        -- Body given as JSON text rather than a decoded table.
        expect(proto.classify_error(429, '{"type":"error"}').kind).to.equal("rate_limit")

        -- Passed decoded: the stub json decoder in this fixture is flat-only,
        -- while the nesting here is what the real decoder returns.
        local quota = proto.classify_error(429, {
            error = {
                type = "rate_limit_error",
                details = { error_code = "enforced_spend_limit_reached" },
            },
        })
        expect(quota.kind).to.equal("quota")
        expect(quota.retryable).to_not.be.truthy()

        local oai = proto.classify_error(429, { error = { code = "insufficient_quota" } })
        expect(oai.kind).to.equal("quota")
        expect(oai.retryable).to_not.be.truthy()
    end)

    it("retries overload and server errors but not client errors", function()
        expect(proto.classify_error(529, "").retryable).to.be.truthy()
        expect(proto.classify_error(500, "").retryable).to.be.truthy()
        expect(proto.classify_error(400, "").retryable).to_not.be.truthy()
        expect(proto.classify_error(401, "").kind).to.equal("auth")
    end)

    it("honours retry-after over the backoff curve", function()
        local c = proto.classify_error(429, "", { ["Retry-After"] = "7" })
        expect(c.retry_after).to.equal(7)
        expect(proto.retry_delay(1, c)).to.equal(7)
    end)

    it("backs off exponentially when no header is given", function()
        local c = proto.classify_error(529, "")
        expect(proto.retry_delay(1, c, 0)).to.equal(1)
        expect(proto.retry_delay(3, c, 0)).to.equal(4)
    end)

    it("survives a non-JSON body", function()
        expect(proto.classify_error(503, "<html>gateway</html>").kind).to.equal("overloaded")
    end)
end)

describe("llm_proto.openai think-tag fallback", function()
    it("splits a well-formed think block out of the content", function()
        local thinking, rest = openai.split_think("<think>step one</think>answer")
        expect(thinking).to.equal("step one")
        expect(rest).to.equal("answer")
    end)

    it("treats a missing opening tag as leading thinking", function()
        local thinking, rest = openai.split_think("step one</think>answer")
        expect(thinking).to.equal("step one")
        expect(rest).to.equal("answer")
    end)

    it("keeps nested tags inside the thinking half", function()
        local thinking, rest = openai.split_think("<think>a<think>b</think>c</think>answer")
        expect(rest).to.equal("answer")
        expect(thinking:find("b", 1, true) ~= nil).to.be.truthy()
    end)

    it("applies the fallback when no reasoning field arrived", function()
        local decoded = openai.parse({
            choices = { { message = { content = "<think>why</think>final" }, finish_reason = "stop" } },
        })
        expect(decoded.content[1].type).to.equal("thinking")
        expect(decoded.content[1].thinking).to.equal("why")
        expect(decoded.content[2].text).to.equal("final")
    end)
end)

describe("llm_proto.openai response", function()
    local function response(message, finish)
        return { choices = { { message = message, finish_reason = finish } } }
    end

    it("surfaces reasoning_content as a thinking block", function()
        local decoded = openai.parse(response({ reasoning_content = "hmm", content = "hi" }, "stop"))
        expect(decoded.content[1].type).to.equal("thinking")
        expect(decoded.content[1].thinking).to.equal("hmm")
        expect(decoded.content[2].type).to.equal("text")
        expect(decoded.stop_reason).to.equal("end_turn")
    end)

    it("accepts the newer reasoning field name", function()
        local decoded = openai.parse(response({ reasoning = "hmm", content = "hi" }, "stop"))
        expect(decoded.content[1].thinking).to.equal("hmm")
    end)

    it("prefers whichever reasoning field is non-empty", function()
        local decoded = openai.parse(response({ reasoning_content = "", reasoning = "hmm" }, "stop"))
        expect(decoded.content[1].thinking).to.equal("hmm")
    end)

    it("synthesizes a missing tool_call id as 9 alphanumerics", function()
        local decoded = openai.parse(response({
            tool_calls = { { ["function"] = { name = "grep", arguments = '{"q":"x"}' } } },
        }, "tool_calls"))
        expect(decoded.content[1].type).to.equal("tool_use")
        expect(#decoded.content[1].id).to.equal(9)
        expect(decoded.content[1].id:match("^[a-zA-Z0-9]+$") ~= nil).to.be.truthy()
        expect(decoded.content[1].input.q).to.equal("x")
        expect(decoded.stop_reason).to.equal("tool_use")
    end)

    it("accepts object-shaped arguments", function()
        local decoded = openai.parse(response({
            tool_calls = { { id = "c1", ["function"] = { name = "grep", arguments = { q = "x" } } } },
        }, "tool_calls"))
        expect(decoded.content[1].input.q).to.equal("x")
    end)

    it("marks unparseable arguments instead of failing", function()
        local decoded = openai.parse(response({
            tool_calls = { { id = "c1", ["function"] = { name = "grep", arguments = "not json" } } },
        }, "tool_calls"))
        expect(decoded.content[1].is_error_hint).to.equal("arguments_parse_failed")
    end)

    it("errors on a response with no choices", function()
        local decoded, err = openai.parse({ choices = {} })
        expect(decoded).to.equal(nil)
        expect(err ~= nil).to.be.truthy()
    end)

    it("reports a refusal instead of an empty success", function()
        local decoded = openai.parse(response({ content = nil, refusal = "cannot help" }, "stop"))
        expect(decoded.stop_reason).to.equal("refusal")
        expect(decoded.refusal).to.equal("cannot help")
    end)

    it("keeps content_filter distinct from a normal stop", function()
        local decoded = openai.parse(response({ content = "x" }, "content_filter"))
        expect(decoded.stop_reason).to.equal("content_filter")
    end)

    it("surfaces the prompt cache counters", function()
        local decoded = openai.parse({
            choices = { { message = { content = "x" }, finish_reason = "stop" } },
            usage = {
                prompt_tokens = 100,
                completion_tokens = 10,
                prompt_tokens_details = { cached_tokens = 64, cache_write_tokens = 8 },
            },
        })
        expect(decoded.usage.cache_read_input_tokens).to.equal(64)
        expect(decoded.usage.cache_creation_input_tokens).to.equal(8)
    end)
end)

describe("llm_proto.anthropic sampling and extras", function()
    it("renames stop to stop_sequences", function()
        expect(a_build({ stop = { "END" } }).body.stop_sequences[1]).to.equal("END")
        expect(a_build({ stop = "END" }).body.stop_sequences[1]).to.equal("END")
    end)

    it("passes temperature on models that allow it", function()
        local req = a_build({ model = "claude-haiku-4-5-20251001", temperature = 0.2 })
        expect(req.body.temperature).to.equal(0.2)
    end)

    it("drops temperature on generations locked to 1.0", function()
        local req = a_build({ model = "claude-opus-5", temperature = 0.2 })
        expect(req.body.temperature).to.equal(nil)
        expect(a_build({ model = "claude-opus-5", temperature = 1 }).body.temperature).to.equal(1)
    end)

    it("maps safety_identifier onto metadata.user_id", function()
        expect(a_build({ safety_identifier = "u1" }).body.metadata.user_id).to.equal("u1")
    end)

    it("maps response_format onto output_config with the beta header", function()
        local req = a_build({
            response_format = { type = "json_schema", json_schema = { schema = { type = "object" } } },
        })
        expect(req.body.output_config.format.type).to.equal("json_schema")
        expect(req.headers["anthropic-beta"]:find("structured-outputs", 1, true) ~= nil).to.be.truthy()
    end)

    it("keeps effort and format together in output_config", function()
        local req = a_build({
            model = "claude-opus-5",
            thinking = { effort = "low" },
            response_format = { type = "json_schema", json_schema = { schema = { type = "object" } } },
        })
        expect(req.body.output_config.effort).to.equal("low")
        expect(req.body.output_config.format.type).to.equal("json_schema")
    end)

    it("joins beta headers into one comma-separated value", function()
        local req = a_build({ context_management = { edits = {} }, betas = { "extra-beta-1" } })
        local hdr = req.headers["anthropic-beta"]
        expect(hdr:find("context-management", 1, true) ~= nil).to.be.truthy()
        expect(hdr:find("extra-beta-1", 1, true) ~= nil).to.be.truthy()
    end)

    it("carries stop_details through for refusals", function()
        local decoded = anthropic.parse({
            content = { { type = "text", text = "" } },
            stop_reason = "refusal",
            stop_details = { type = "refusal", category = "cyber" },
        })
        expect(decoded.stop_reason).to.equal("refusal")
        expect(decoded.stop_details.category).to.equal("cyber")
    end)
end)

describe("llm_proto.anthropic response", function()
    it("preserves thinking blocks verbatim for the tool-use round-trip", function()
        local decoded = anthropic.parse({
            content = {
                { type = "thinking", thinking = "step", signature = "sig" },
                { type = "redacted_thinking", data = "enc" },
                { type = "text", text = "hi" },
            },
            stop_reason = "end_turn",
            usage = { input_tokens = 10, output_tokens = 4, cache_read_input_tokens = 7 },
        })
        expect(decoded.content[1].signature).to.equal("sig")
        expect(decoded.content[2].type).to.equal("redacted_thinking")
        expect(decoded.usage.input_tokens).to.equal(10)
        expect(decoded.usage.cache_read_input_tokens).to.equal(7)
    end)

    it("errors when content blocks are missing", function()
        local decoded, err = anthropic.parse({ stop_reason = "end_turn" })
        expect(decoded).to.equal(nil)
        expect(err ~= nil).to.be.truthy()
    end)
end)
