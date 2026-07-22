-- agent_helpers_test.lua — mlua-lspec unit tests for pure helpers in blocks/agent/init.lua.
--
-- Run via:
--   mcp__lua-debugger__test_launch(
--     code_file    = "crates/agent-block/tests/fixtures/agent_helpers_test.lua",
--     search_paths = ["crates/agent-block-core/blocks"]
--   )
--
-- These exercise the I/O-free branches exposed by agent._test_helpers():
--   * map_finish_reason        — OpenAI finish_reason → Anthropic stop_reason mapping
--   * count_tool_use_blocks    — tool_use block counting (nil / empty / mixed)
--   * count_text_chars         — text char counting (nil text / non-text blocks)
--   * extract_text             — text block concatenation
--   * normalize_dump_mode      — LLM dump mode normalization (case + alias + fallback)
--   * sanitize_headers_for_dump— secret header redaction (case-insensitive)
--   * kv_escape / format_kv    — structured kv-log escaping
--   * new_budget_tracker       — token budget accumulation / exceeded semantics
--   * normalize_openai_response— OpenAI chat response → Anthropic-shape decode
--   * convert_messages_to_openai — Anthropic-shape history → OpenAI-shape messages
--   * resolve_mcp_group        — _meta.group precedence over server_name
--
-- NOTE: these helpers read the std/log globals that the agent-block runtime injects.
-- The mlua test harness has neither, so we install minimal stubs (incl. a small but
-- correct JSON encode/decode) BEFORE the helpers are invoked. require("agent") itself
-- does not touch these globals at load time (all module-level code only defines locals).

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Runtime global stubs (std / log). Installed before any helper is called.
-- ─────────────────────────────────────────────────────────────────────────────

-- Minimal but correct JSON encoder (arrays vs objects, string escaping).
local function json_encode(v)
    local t = type(v)
    if v == nil then
        return "null"
    elseif t == "boolean" or t == "number" then
        return tostring(v)
    elseif t == "string" then
        local s = v:gsub("\\", "\\\\"):gsub('"', '\\"'):gsub("\n", "\\n"):gsub("\t", "\\t")
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
        local obj_parts = {}
        for _, key in ipairs(keys) do
            obj_parts[#obj_parts + 1] = json_encode(key) .. ":" .. json_encode(v[key])
        end
        return "{" .. table.concat(obj_parts, ",") .. "}"
    end
    return "null"
end

-- Minimal recursive-descent JSON decoder (objects / arrays / strings / numbers /
-- booleans / null). Sufficient for the tool_call.arguments strings under test.
local function json_decode(s)
    local pos = 1
    local parse_value

    local function skip_ws()
        local _, e = s:find("^[ \t\r\n]*", pos)
        if e and e >= pos then
            pos = e + 1
        end
    end

    local function parse_string()
        pos = pos + 1 -- consume opening quote
        local buf = {}
        while pos <= #s do
            local c = s:sub(pos, pos)
            if c == '"' then
                pos = pos + 1
                return table.concat(buf)
            elseif c == "\\" then
                local nc = s:sub(pos + 1, pos + 1)
                if nc == "n" then
                    buf[#buf + 1] = "\n"
                elseif nc == "t" then
                    buf[#buf + 1] = "\t"
                else
                    buf[#buf + 1] = nc
                end
                pos = pos + 2
            else
                buf[#buf + 1] = c
                pos = pos + 1
            end
        end
        error("unterminated string")
    end

    local function parse_object()
        pos = pos + 1 -- consume {
        local obj = {}
        skip_ws()
        if s:sub(pos, pos) == "}" then
            pos = pos + 1
            return obj
        end
        while true do
            skip_ws()
            local key = parse_string()
            skip_ws()
            if s:sub(pos, pos) ~= ":" then
                error("expected ':'")
            end
            pos = pos + 1
            obj[key] = parse_value()
            skip_ws()
            local c = s:sub(pos, pos)
            if c == "," then
                pos = pos + 1
            elseif c == "}" then
                pos = pos + 1
                return obj
            else
                error("expected ',' or '}'")
            end
        end
    end

    local function parse_array()
        pos = pos + 1 -- consume [
        local arr = {}
        skip_ws()
        if s:sub(pos, pos) == "]" then
            pos = pos + 1
            return arr
        end
        while true do
            arr[#arr + 1] = parse_value()
            skip_ws()
            local c = s:sub(pos, pos)
            if c == "," then
                pos = pos + 1
            elseif c == "]" then
                pos = pos + 1
                return arr
            else
                error("expected ',' or ']'")
            end
        end
    end

    parse_value = function()
        skip_ws()
        local c = s:sub(pos, pos)
        if c == "{" then
            return parse_object()
        elseif c == "[" then
            return parse_array()
        elseif c == '"' then
            return parse_string()
        elseif s:find("^true", pos) then
            pos = pos + 4
            return true
        elseif s:find("^false", pos) then
            pos = pos + 5
            return false
        elseif s:find("^null", pos) then
            pos = pos + 4
            return nil
        else
            local num = s:match("^%-?%d+%.?%d*", pos)
            if num then
                pos = pos + #num
                return tonumber(num)
            end
            error("unexpected token at " .. pos)
        end
    end

    return parse_value()
end

-- Install globals (assigned, so static analysis treats them as defined globals).
std = {
    json = { encode = json_encode, decode = json_decode },
    env = {
        get = function(_name)
            return nil
        end,
        get_or = function(_name, default)
            return default
        end,
        agent_id = function()
            return nil
        end,
    },
}
log = { warn = function() end, info = function() end, debug = function() end, error = function() end }

local agent = require("agent")
local H = agent._test_helpers()

-- ─────────────────────────────────────────────────────────────────────────────
-- map_finish_reason
-- ─────────────────────────────────────────────────────────────────────────────

describe("agent.map_finish_reason", function()
    local map = H.map_finish_reason

    it("maps 'stop' to 'end_turn'", function()
        expect(map("stop")).to.equal("end_turn")
    end)

    it("maps 'tool_calls' to 'tool_use'", function()
        expect(map("tool_calls")).to.equal("tool_use")
    end)

    it("maps 'length' to 'max_tokens'", function()
        expect(map("length")).to.equal("max_tokens")
    end)

    it("passes an unknown reason through as string", function()
        expect(map("content_filter")).to.equal("content_filter")
    end)

    it("defaults nil to 'end_turn'", function()
        expect(map(nil)).to.equal("end_turn")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- count_tool_use_blocks / count_text_chars / extract_text
-- ─────────────────────────────────────────────────────────────────────────────

describe("agent.count_tool_use_blocks", function()
    local count = H.count_tool_use_blocks

    it("returns 0 for nil content", function()
        expect(count(nil)).to.equal(0)
    end)

    it("returns 0 for empty content", function()
        expect(count({})).to.equal(0)
    end)

    it("counts only tool_use blocks in a mixed array", function()
        local content = {
            { type = "text", text = "hi" },
            { type = "tool_use", id = "a", name = "x", input = {} },
            { type = "tool_use", id = "b", name = "y", input = {} },
            { type = "text", text = "bye" },
        }
        expect(count(content)).to.equal(2)
    end)
end)

describe("agent.count_text_chars", function()
    local count = H.count_text_chars

    it("returns 0 for nil content", function()
        expect(count(nil)).to.equal(0)
    end)

    it("sums lengths of text blocks only", function()
        local content = {
            { type = "text", text = "abc" }, -- 3
            { type = "tool_use", id = "a", name = "x", input = {} }, -- ignored
            { type = "text", text = "de" }, -- 2
        }
        expect(count(content)).to.equal(5)
    end)

    it("ignores text blocks missing the text field", function()
        local content = {
            { type = "text" }, -- no text → ignored
            { type = "text", text = "xy" }, -- 2
        }
        expect(count(content)).to.equal(2)
    end)
end)

describe("agent.extract_text", function()
    local extract = H.extract_text

    it("returns empty string for nil content", function()
        expect(extract(nil)).to.equal("")
    end)

    it("joins multiple text blocks with newline", function()
        local content = {
            { type = "text", text = "line1" },
            { type = "tool_use", id = "a", name = "x", input = {} },
            { type = "text", text = "line2" },
        }
        expect(extract(content)).to.equal("line1\nline2")
    end)

    it("returns the single text block verbatim", function()
        expect(extract({ { type = "text", text = "only" } })).to.equal("only")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- normalize_dump_mode
-- ─────────────────────────────────────────────────────────────────────────────

describe("agent.normalize_dump_mode", function()
    local norm = H.normalize_dump_mode

    it("returns nil for nil / empty (unset → caller falls back)", function()
        expect(norm(nil)).to.equal(nil)
        expect(norm("")).to.equal(nil)
    end)

    it("maps 'off' and 'none' to 'off'", function()
        expect(norm("off")).to.equal("off")
        expect(norm("none")).to.equal("off")
    end)

    it("preserves 'meta' and 'full'", function()
        expect(norm("meta")).to.equal("meta")
        expect(norm("full")).to.equal("full")
    end)

    it("is case-insensitive", function()
        expect(norm("FULL")).to.equal("full")
        expect(norm("Meta")).to.equal("meta")
    end)

    it("falls back to 'off' for unrecognized values", function()
        expect(norm("verbose")).to.equal("off")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- sanitize_headers_for_dump
-- ─────────────────────────────────────────────────────────────────────────────

describe("agent.sanitize_headers_for_dump", function()
    local san = H.sanitize_headers_for_dump

    it("returns empty table for nil headers", function()
        local out = san(nil)
        expect(type(out)).to.equal("table")
        expect(next(out)).to.equal(nil)
    end)

    it("redacts x-api-key and authorization (case-insensitive)", function()
        local out = san({ ["X-Api-Key"] = "secret1", ["Authorization"] = "Bearer secret2" })
        expect(out["X-Api-Key"]).to.equal("***REDACTED***")
        expect(out["Authorization"]).to.equal("***REDACTED***")
    end)

    it("passes non-secret headers through unchanged", function()
        local out = san({ ["Content-Type"] = "application/json" })
        expect(out["Content-Type"]).to.equal("application/json")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- kv_escape / format_kv
-- ─────────────────────────────────────────────────────────────────────────────

describe("agent.kv_escape", function()
    local esc = H.kv_escape

    it("renders nil as the literal 'nil'", function()
        expect(esc(nil)).to.equal("nil")
    end)

    it("renders booleans and numbers via tostring", function()
        expect(esc(true)).to.equal("true")
        expect(esc(42)).to.equal("42")
    end)

    it("renders an empty string as quoted empty", function()
        expect(esc("")).to.equal('""')
    end)

    it("passes a plain token through unquoted", function()
        expect(esc("hello")).to.equal("hello")
    end)

    it("json-quotes a value containing whitespace", function()
        expect(esc("a b")).to.equal('"a b"')
    end)

    it("json-quotes a value containing '='", function()
        expect(esc("k=v")).to.equal('"k=v"')
    end)
end)

describe("agent.format_kv", function()
    local fmt = H.format_kv

    it("joins pairs as space-separated key=value", function()
        local out = fmt({ { "event", "start" }, { "n", 3 } })
        expect(out).to.equal("event=start n=3")
    end)

    it("escapes values that need quoting", function()
        local out = fmt({ { "msg", "hi there" } })
        expect(out).to.equal('msg="hi there"')
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- new_budget_tracker
-- ─────────────────────────────────────────────────────────────────────────────

describe("agent.new_budget_tracker", function()
    local mk = H.new_budget_tracker

    it("never exceeds when no limit is set", function()
        local t = mk(nil)
        t:add({ input_tokens = 1000000, output_tokens = 1000000 })
        expect(t:exceeded()).to.equal(false)
    end)

    it("accumulates input and output tokens into total", function()
        local t = mk(100)
        t:add({ input_tokens = 10, output_tokens = 5 })
        t:add({ input_tokens = 20, output_tokens = 0 })
        local s = t:summary()
        expect(s.input_tokens).to.equal(30)
        expect(s.output_tokens).to.equal(5)
        expect(s.total_tokens).to.equal(35)
    end)

    it("exceeds once total reaches the limit", function()
        local t = mk(30)
        t:add({ input_tokens = 20, output_tokens = 10 })
        expect(t:exceeded()).to.equal(true)
    end)

    it("does not exceed just below the limit", function()
        local t = mk(30)
        t:add({ input_tokens = 20, output_tokens = 9 })
        expect(t:exceeded()).to.equal(false)
    end)

    it("tolerates a nil usage argument", function()
        local t = mk(10)
        t:add(nil)
        expect(t:summary().total_tokens).to.equal(0)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- normalize_openai_response
-- ─────────────────────────────────────────────────────────────────────────────

describe("agent.normalize_openai_response", function()
    local norm = H.normalize_openai_response

    it("errors when choices is missing", function()
        local decoded, err = norm({})
        expect(decoded).to.equal(nil)
        expect(type(err)).to.equal("string")
    end)

    it("errors when choices[0].message is missing", function()
        local decoded, err = norm({ choices = { {} } })
        expect(decoded).to.equal(nil)
        expect(type(err)).to.equal("string")
    end)

    it("decodes a plain text response into an Anthropic-shape text block", function()
        local decoded = norm({
            choices = { { message = { content = "hello world" }, finish_reason = "stop" } },
            usage = { prompt_tokens = 12, completion_tokens = 4 },
        })
        expect(decoded.content[1].type).to.equal("text")
        expect(decoded.content[1].text).to.equal("hello world")
        expect(decoded.stop_reason).to.equal("end_turn")
        expect(decoded.usage.input_tokens).to.equal(12)
        expect(decoded.usage.output_tokens).to.equal(4)
    end)

    it("skips the text block when content is empty (tool-only turn)", function()
        local decoded = norm({
            choices = {
                {
                    message = {
                        content = "",
                        tool_calls = {
                            { id = "call_1", ["function"] = { name = "search", arguments = '{"q":"x"}' } },
                        },
                    },
                    finish_reason = "tool_calls",
                },
            },
        })
        expect(#decoded.content).to.equal(1)
        expect(decoded.content[1].type).to.equal("tool_use")
        expect(decoded.content[1].name).to.equal("search")
        expect(decoded.content[1].input.q).to.equal("x")
        expect(decoded.stop_reason).to.equal("tool_use")
    end)

    it("marks a tool_use with is_error_hint when arguments JSON is invalid", function()
        local decoded = norm({
            choices = {
                {
                    message = {
                        content = nil,
                        tool_calls = {
                            { id = "call_2", ["function"] = { name = "broken", arguments = "{not json" } },
                        },
                    },
                    finish_reason = "tool_calls",
                },
            },
        })
        expect(decoded.content[1].type).to.equal("tool_use")
        expect(decoded.content[1].is_error_hint).to.equal("arguments_parse_failed")
        -- input must degrade to an empty table, not crash.
        expect(type(decoded.content[1].input)).to.equal("table")
        expect(next(decoded.content[1].input)).to.equal(nil)
    end)

    it("defaults usage counters to 0 when usage is absent", function()
        local decoded = norm({
            choices = { { message = { content = "x" }, finish_reason = "stop" } },
        })
        expect(decoded.usage.input_tokens).to.equal(0)
        expect(decoded.usage.output_tokens).to.equal(0)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- convert_messages_to_openai
-- ─────────────────────────────────────────────────────────────────────────────

describe("agent.convert_messages_to_openai", function()
    local convert = H.convert_messages_to_openai

    it("prepends a system message when provided", function()
        local out = convert({ { role = "user", content = "hi" } }, "be terse")
        expect(out[1].role).to.equal("system")
        expect(out[1].content).to.equal("be terse")
        expect(out[2].role).to.equal("user")
        expect(out[2].content).to.equal("hi")
    end)

    it("omits the system message when system is nil", function()
        local out = convert({ { role = "user", content = "hi" } }, nil)
        expect(out[1].role).to.equal("user")
    end)

    it("converts an assistant text+tool_use block into content + tool_calls", function()
        local out = convert({
            {
                role = "assistant",
                content = {
                    { type = "text", text = "thinking" },
                    { type = "tool_use", id = "t1", name = "grep", input = { pattern = "x" } },
                },
            },
        }, nil)
        local msg = out[1]
        expect(msg.role).to.equal("assistant")
        expect(msg.content).to.equal("thinking")
        expect(#msg.tool_calls).to.equal(1)
        expect(msg.tool_calls[1].id).to.equal("t1")
        expect(msg.tool_calls[1]["function"].name).to.equal("grep")
        -- arguments is the JSON encoding of the input table.
        expect(type(msg.tool_calls[1]["function"].arguments)).to.equal("string")
        expect(msg.tool_calls[1]["function"].arguments).to.equal('{"pattern":"x"}')
    end)

    it("expands user tool_result blocks into role='tool' messages", function()
        local out = convert({
            {
                role = "user",
                content = {
                    { type = "tool_result", tool_use_id = "t1", content = "result-text" },
                },
            },
        }, nil)
        expect(out[1].role).to.equal("tool")
        expect(out[1].tool_call_id).to.equal("t1")
        expect(out[1].content).to.equal("result-text")
    end)

    it("flattens a user text-block array into a single content string", function()
        local out = convert({
            {
                role = "user",
                content = {
                    { type = "text", text = "part1" },
                    { type = "text", text = "part2" },
                },
            },
        }, nil)
        expect(out[1].role).to.equal("user")
        expect(out[1].content).to.equal("part1\npart2")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- resolve_mcp_group
-- ─────────────────────────────────────────────────────────────────────────────

describe("agent.resolve_mcp_group", function()
    local resolve = H.resolve_mcp_group

    it("uses _meta.group when it is a non-empty string", function()
        expect(resolve({ _meta = { group = "search" } }, "outline")).to.equal("search")
    end)

    it("falls back to server_name when _meta is absent", function()
        expect(resolve({}, "outline")).to.equal("outline")
    end)

    it("falls back to server_name when _meta.group is empty", function()
        expect(resolve({ _meta = { group = "" } }, "outline")).to.equal("outline")
    end)

    it("falls back to server_name when _meta.group is not a string", function()
        expect(resolve({ _meta = { group = 5 } }, "outline")).to.equal("outline")
    end)
end)
