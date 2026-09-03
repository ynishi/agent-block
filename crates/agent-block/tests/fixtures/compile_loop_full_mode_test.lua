-- compile_loop_full_mode_test.lua — mlua-lspec tests for full mode's model call.
--
-- Run via:
--   just test-lua compile_loop_full_mode_test   # this file
--   just test-lua                               # every spec fixture
--
-- Full mode used to call the provider through compile_loop's own transport,
-- which had no retry, ignored `stop_reason`, dropped thinking blocks and kept
-- the system prompt inside the messages array. It now goes through `tool_loop`
-- with no tools, which is one model call and nothing else. These are the
-- properties that changed hands, one test each:
--
--   1. a transient failure is retried instead of ending the run
--   2. a refusal ends the run instead of being read as an empty answer
--   3. a response cut off at max_tokens is not written to the target file
--   4. thinking blocks stay in the transcript and out of the file
--   4b. an answer with no blocks still goes back as a turn the provider accepts
--       (unchanged behaviour, pinned because the new path could have lost it)
--   5. the system prompt is passed beside the messages, not inside them
--   6. the OpenAI request still carries the browser User-Agent (RunPod proxy)
--   7. api_key and model come from llm_proto's resolution
--
-- The transport is stubbed at its two ends rather than by writing a JSON codec:
-- each request body is encoded to a unique sentinel the test can look the table
-- back up from, and each response body decodes to whatever the test queued.
-- What is under test is which request the loop builds and what it does with the
-- answer; the wire format has llm_proto_test.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ============================================================
-- Host stubs
-- ============================================================

-- Swapped per test, so each one states the environment it needs.
local env = {}

-- Responses the stubbed transport hands back, in order:
-- { status = number, response = table|nil }.
local queue = {}

-- What the transport was asked to send: { url, headers, body }.
local requests = {}

-- Retry backoffs taken.
local sleeps = 0

-- Request bodies are encoded to a fresh sentinel each time and looked back up
-- here, so a test reads the table that was sent rather than a re-encoding of it.
local encoded = {}
local encode_n = 0

-- Response bodies decode back to the table the test queued.
local decoded = {}
local decode_n = 0

local function reset()
    env = {}
    queue = {}
    requests = {}
    sleeps = 0
end

if not log then
    log = { warn = function() end, info = function() end, debug = function() end, error = function() end }
end
if not tool then
    tool = { register = function() end }
end
if not std then
    std = {
        env = {
            get = function(name)
                return env[name]
            end,
            get_or = function(name, default)
                local v = env[name]
                if v == nil then
                    return default
                end
                return v
            end,
            agent_id = function()
                return nil
            end,
        },
        json = {
            encode = function(v)
                encode_n = encode_n + 1
                local sentinel = "<encoded-" .. encode_n .. ">"
                encoded[sentinel] = v
                return sentinel
            end,
            decode = function(s)
                local v = decoded[s]
                if v == nil then
                    -- Error bodies land here too (classify_error tries to read
                    -- them); it calls through pcall, so raising is the honest
                    -- answer for a body the test never queued.
                    error("compile_loop_full_mode_test: unexpected body to decode: " .. tostring(s), 0)
                end
                return v
            end,
        },
        time = {
            now = function()
                return 0
            end,
        },
        task = {
            sleep = function(_ms)
                sleeps = sleeps + 1
            end,
        },
        -- run_loop builds the edit tool spec even in full mode, which never
        -- dispatches it. The handler raises rather than reporting success: a
        -- full-mode run that reaches it is a bug, not a passing test.
        fs = {
            metadata = function(_path)
                return nil
            end,
            tool_specs = function(_opts)
                return {
                    {
                        name = "fs_edit",
                        description = "stub (compile_loop_full_mode_test)",
                        input_schema = { type = "object" },
                        handler = function()
                            error("full mode must not dispatch tools", 0)
                        end,
                    },
                }
            end,
        },
    }
end

if not http then
    http = {
        request = function(url, opts)
            table.insert(requests, {
                url = url,
                headers = opts.headers or {},
                body = encoded[opts.body],
            })
            local entry = table.remove(queue, 1)
            if not entry then
                error("compile_loop_full_mode_test: the loop asked for a response the test did not queue", 0)
            end
            local body = "<error-body>"
            if entry.response then
                decode_n = decode_n + 1
                body = "<response-" .. decode_n .. ">"
                decoded[body] = entry.response
            end
            return { status = entry.status, body = body, headers = {} }
        end,
    }
end

local compile_loop = require("compile_loop")
local run_loop = compile_loop._test_helpers().run_loop

-- ============================================================
-- Fixtures
-- ============================================================

local PLACEHOLDER = "-- placeholder\n"

--- An Anthropic Messages response carrying one text block.
local function text_response(text, stop_reason)
    return {
        id = "msg_1",
        role = "assistant",
        content = { { type = "text", text = text } },
        stop_reason = stop_reason or "end_turn",
        usage = { input_tokens = 1, output_tokens = 1 },
    }
end

--- An OpenAI chat completion carrying one message.
local function oai_response(text, finish_reason)
    return {
        id = "cmpl_1",
        choices = {
            {
                index = 0,
                message = { role = "assistant", content = text },
                finish_reason = finish_reason or "stop",
            },
        },
        usage = { prompt_tokens = 1, completion_tokens = 1 },
    }
end

local function fenced(body)
    return "```lua\n" .. body .. "\n```"
end

local function queue_ok(response)
    table.insert(queue, { status = 200, response = response })
end

--- Run one full-mode loop over a fresh target file.
---
--- The base conf carries no `api_key` and no `model`: a Lua table cannot say
--- "unset this", so the tests about resolution would not be able to take them
--- away again. Every test that is not about resolution passes `api_key = "k"`.
---
--- @param overrides table|nil  conf fields to set or replace
--- @return table result  run_loop's result
--- @return string|nil written  the target file's content after the run
local function run_full(overrides)
    local tmp = "/tmp/cl_full_mode_" .. tostring(os.time()) .. "_" .. tostring(math.random(1000000)) .. ".lua"
    local f = io.open(tmp, "w")
    if f then
        f:write(PLACEHOLDER)
        f:close()
    end

    local conf = {
        target_files = { tmp },
        multi_file = false,
        edit_mode = "full",
        lang = "lua",
        spec = "print something",
        system = "SYSTEM-MARKER",
        runner = function(_path)
            return { ok = true, stdout = "", exit_code = 0 }
        end,
        max_iters = 3,
    }
    for k, v in pairs(overrides or {}) do
        conf[k] = v
    end

    local result = run_loop(conf)

    local handle = io.open(tmp, "r")
    local written = handle and handle:read("*a") or nil
    if handle then
        handle:close()
    end
    os.remove(tmp)
    return result, written
end

-- ============================================================
-- 1. Transient retry
-- ============================================================

describe("a transient failure is retried", function()
    it("recovers from a 429 within the same iteration", function()
        reset()
        table.insert(queue, { status = 429 })
        queue_ok(text_response(fenced("print('ok')")))

        local result, written = run_full({ provider = "anthropic", api_key = "k" })

        expect(result.ok).to.equal(true)
        expect(result.iters).to.equal(1)
        -- Two attempts for one iteration: the retry is the transport's, not the
        -- loop's, so no iteration was spent on it.
        expect(#requests).to.equal(2)
        expect(sleeps).to.equal(1)
        expect(written).to.equal("print('ok')")
    end)
end)

-- ============================================================
-- 2. Refusal
-- ============================================================

describe("a refusal ends the run", function()
    it("reports it and leaves the target file alone", function()
        reset()
        queue_ok(text_response("I will not write that", "refusal"))

        local result, written = run_full({ provider = "anthropic", api_key = "k" })

        expect(result.ok).to.equal(false)
        expect(result.failure_reason).to.equal("llm_call")
        expect(tostring(result.last_error):find("refus", 1, true) ~= nil).to.equal(true)
        -- A refusal is not an empty answer to be written out.
        expect(written).to.equal(PLACEHOLDER)
    end)
end)

-- ============================================================
-- 3. max_tokens truncation
-- ============================================================

describe("a response cut off at max_tokens", function()
    it("ends the run instead of writing half a file", function()
        reset()
        -- No closing fence: the model was stopped mid-emission.
        queue_ok(text_response("```lua\nprint('half", "max_tokens"))

        local result, written = run_full({ provider = "anthropic", api_key = "k" })

        expect(result.ok).to.equal(false)
        expect(result.failure_reason).to.equal("llm_call")
        expect(tostring(result.last_error):find("max_tokens", 1, true) ~= nil).to.equal(true)
        expect(written).to.equal(PLACEHOLDER)
        -- The truncation ended the run rather than costing an iteration.
        expect(result.iters).to.equal(0)
    end)
end)

-- ============================================================
-- 4. Thinking blocks
-- ============================================================

describe("thinking blocks", function()
    it("stay in the transcript and out of the file", function()
        reset()
        queue_ok({
            id = "msg_thinking",
            role = "assistant",
            content = {
                { type = "thinking", thinking = "REASONING-MARKER" },
                { type = "text", text = fenced("print('first')") },
            },
            stop_reason = "end_turn",
            usage = { input_tokens = 1, output_tokens = 1 },
        })
        queue_ok(text_response(fenced("print('second')")))

        local runs = 0
        local result, written = run_full({
            provider = "anthropic",
            api_key = "k",
            runner = function(_path)
                runs = runs + 1
                if runs == 1 then
                    return { ok = false, stderr = "boom", exit_code = 1 }
                end
                return { ok = true, stdout = "", exit_code = 0 }
            end,
        })

        expect(result.ok).to.equal(true)
        expect(written).to.equal("print('second')")
        expect(written:find("REASONING-MARKER", 1, true) == nil).to.equal(true)

        -- The second request replays the assistant turn verbatim, thinking
        -- block first: Anthropic rejects a turn that comes back reordered.
        local replayed = requests[2].body.messages[2]
        expect(replayed.role).to.equal("assistant")
        expect(replayed.content[1].type).to.equal("thinking")
        expect(replayed.content[1].thinking).to.equal("REASONING-MARKER")
    end)
end)

-- ============================================================
-- 4b. An answer with no blocks (preserved, not a change)
-- ============================================================

describe("an answer with no blocks at all", function()
    it("goes back as an empty turn rather than an absent one", function()
        reset()
        queue_ok({
            id = "msg_empty",
            role = "assistant",
            content = {},
            stop_reason = "end_turn",
            usage = { input_tokens = 1, output_tokens = 0 },
        })
        queue_ok(text_response(fenced("print('recovered')")))

        local runs = 0
        local result = run_full({
            provider = "anthropic",
            api_key = "k",
            runner = function(_path)
                runs = runs + 1
                if runs == 1 then
                    return { ok = false, stderr = "empty file", exit_code = 1 }
                end
                return { ok = true, stdout = "", exit_code = 0 }
            end,
        })

        expect(result.ok).to.equal(true)

        -- The iteration that has to say "you produced nothing" cannot be the
        -- one whose request the provider rejects for carrying nothing.
        local replayed = requests[2].body.messages[2]
        expect(replayed.role).to.equal("assistant")
        expect(#replayed.content).to.equal(1)
        expect(replayed.content[1].type).to.equal("text")
        expect(replayed.content[1].text).to.equal("")
    end)
end)

-- ============================================================
-- 5. System prompt placement
-- ============================================================

describe("the system prompt", function()
    it("rides beside the messages rather than inside them", function()
        reset()
        queue_ok(text_response(fenced("print('ok')")))

        local result = run_full({ provider = "anthropic", api_key = "k" })
        expect(result.ok).to.equal(true)

        local body = requests[1].body
        expect(body.system).to.equal("SYSTEM-MARKER")
        expect(body.messages[1].role).to.equal("user")
        for _, msg in ipairs(body.messages) do
            expect(msg.role == "system").to.equal(false)
        end
    end)
end)

-- ============================================================
-- 6. Header injection (RunPod / Cloudflare compatibility)
-- ============================================================

describe("the OpenAI-compatible request", function()
    it("carries the browser User-Agent alongside the auth header", function()
        reset()
        queue_ok(oai_response(fenced("print('ok')")))

        -- No provider: full mode resolves an unset one to openai.
        local result = run_full({ api_key = "k" })
        expect(result.ok).to.equal(true)

        expect(requests[1].url:find("/chat/completions", 1, true) ~= nil).to.equal(true)
        expect(requests[1].headers["User-Agent"]).to.equal("Mozilla/5.0")
        expect(requests[1].headers["Authorization"]).to.equal("Bearer k")
    end)

    it("is the only path that carries it", function()
        reset()
        queue_ok(text_response(fenced("print('ok')")))

        local result = run_full({ provider = "anthropic", api_key = "k" })
        expect(result.ok).to.equal(true)

        expect(requests[1].url:find("/v1/messages", 1, true) ~= nil).to.equal(true)
        expect(requests[1].headers["User-Agent"] == nil).to.equal(true)
    end)
end)

-- ============================================================
-- 7. api_key / model resolution
-- ============================================================

describe("api_key and model resolution", function()
    it("falls back to the environment and the adapter default", function()
        reset()
        env["OPENAI_API_KEY"] = "env-key"
        queue_ok(oai_response(fenced("print('ok')")))

        local result = run_full({})
        expect(result.ok).to.equal(true)

        expect(requests[1].headers["Authorization"]).to.equal("Bearer env-key")
        expect(requests[1].body.model).to.equal("gpt-4o-mini")
    end)

    it("takes the model from the environment on the Anthropic path", function()
        reset()
        env["ANTHROPIC_MODEL"] = "claude-from-env"
        queue_ok(text_response(fenced("print('ok')")))

        -- The key is given; the model is the one under test here.
        local result = run_full({ provider = "anthropic", api_key = "k" })
        expect(result.ok).to.equal(true)
        expect(requests[1].body.model).to.equal("claude-from-env")
    end)

    it("ends the run before the request when no key can be found", function()
        reset()
        queue_ok(oai_response(fenced("print('ok')")))

        local result, written = run_full({})

        expect(result.ok).to.equal(false)
        expect(result.failure_reason).to.equal("llm_call")
        -- The failure names the variable that was looked at.
        expect(tostring(result.last_error):find("OPENAI_API_KEY", 1, true) ~= nil).to.equal(true)
        expect(#requests).to.equal(0)
        expect(written).to.equal(PLACEHOLDER)
    end)
end)
