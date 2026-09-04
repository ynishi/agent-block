-- llm_proto_backend_test.lua — mlua-lspec tests for llm_proto.backend.
--
-- Run via:
--   just test-lua llm_proto_backend_test   # this file
--   just test-lua                          # every spec fixture
--
-- `llm_proto.backend` is the whole model call as one closure: build the wire
-- request, post it with the retries worth taking, parse the answer. `tool_loop`
-- and the agent block call it directly, and `knl_adapter`'s Port reuses the
-- same pieces behind a device's `llm`, so every side gets the same transport
-- and none of them carries provider knowledge.
--
-- What is pinned here is the closure's end of that arrangement:
--   1. the result satisfies the contract its callers read (content / usage /
--      stop_reason, or nil and an error)
--   2. the conf and the request meet on the wire, request first
--   3. api_key / model resolution, including the failure before the request
--   4. which failures are retried and which are answered as they are
--   5. the hooks, which are the only way anything behind the neutral result
--      (stop_details, provider extras) reaches a caller
--
-- The transport is stubbed at its two ends rather than by writing a JSON codec:
-- each request body encodes to a sentinel the test can look the table back up
-- from, and each response body decodes to whatever the test queued. The wire
-- format itself has llm_proto_test.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ============================================================
-- Host stubs
-- ============================================================

-- Swapped per test, so each one states the environment it needs.
local env = {}

-- Responses the stubbed transport hands back, in order:
-- { status = number, response = table|nil, headers = table|nil }.
-- A entry with no `response` produces a body that does not decode.
local queue = {}

-- What the transport was asked to send.
local requests = {}

-- Retry backoffs taken.
local sleeps = 0

local encoded = {}
local encode_n = 0
local decodable = {}
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
                local v = decodable[s]
                if v == nil then
                    -- Error bodies land here too (classify_error tries to read
                    -- them); it calls through pcall, so raising is the honest
                    -- answer for a body the test never queued.
                    error("llm_proto_backend_test: unexpected body to decode: " .. tostring(s), 0)
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
    }
end

if not http then
    http = {
        request = function(url, opts)
            table.insert(requests, {
                url = url,
                headers = opts.headers or {},
                body = encoded[opts.body],
                body_json = opts.body,
                method = opts.method,
                timeout = opts.timeout,
                dump = opts.dump,
            })
            local entry = table.remove(queue, 1)
            if not entry then
                error("llm_proto_backend_test: the backend asked for a response the test did not queue", 0)
            end
            local body = "<undecodable-body>"
            if entry.response then
                decode_n = decode_n + 1
                body = "<response-" .. decode_n .. ">"
                decodable[body] = entry.response
            end
            return { status = entry.status, body = body, headers = entry.headers or {} }
        end,
    }
end

local proto = require("llm_proto")

-- ============================================================
-- Fixtures
-- ============================================================

--- An Anthropic Messages response carrying one text block.
local function text_response(text, stop_reason)
    return {
        id = "msg_1",
        role = "assistant",
        content = { { type = "text", text = text or "ok" } },
        stop_reason = stop_reason or "end_turn",
        usage = { input_tokens = 3, output_tokens = 4 },
    }
end

--- An OpenAI chat completion carrying one message.
local function oai_response(text)
    return {
        id = "cmpl_1",
        choices = {
            {
                index = 0,
                message = { role = "assistant", content = text or "ok" },
                finish_reason = "stop",
            },
        },
        usage = { prompt_tokens = 1, completion_tokens = 1 },
    }
end

local function queue_ok(response)
    table.insert(queue, { status = 200, response = response })
end

--- A backend over the Anthropic path with credentials, unless the case is
--- about resolving them.
local function anthropic_backend(conf)
    conf = conf or {}
    conf.provider = conf.provider or "anthropic"
    if conf.api_key == nil and conf.api_key_env == nil then
        conf.api_key = "k"
    end
    conf.model = conf.model or "claude-haiku-4-5-20251001"
    local backend, err = proto.backend(conf)
    assert(backend, "backend conf was rejected: " .. tostring(err))
    return backend
end

--- The neutral request every case sends unless it is about the request.
local function ask(backend, req)
    return backend(req or { messages = { { role = "user", content = "hi" } } })
end

-- ============================================================
-- 1. The result contract
-- ============================================================

describe("what the backend returns", function()
    it("carries the three fields the kernel records", function()
        reset()
        queue_ok(text_response("hello"))

        local res, err = ask(anthropic_backend())

        expect(err).to.equal(nil)
        expect(#res.content).to.equal(1)
        expect(res.content[1].type).to.equal("text")
        expect(res.content[1].text).to.equal("hello")
        expect(type(res.usage)).to.equal("table")
        expect(res.usage.input_tokens).to.equal(3)
        expect(res.usage.output_tokens).to.equal(4)
        expect(res.stop_reason).to.equal("end_turn")
    end)

    it("carries what the kernel drops beside them", function()
        reset()
        queue_ok(text_response("hello"))

        local res = ask(anthropic_backend())

        expect(res.status).to.equal(200)
        expect(type(res.latency_ms)).to.equal("number")
    end)

    -- An answer with no blocks is an answer. It is reported as the empty array
    -- it was, tagged so the empty Lua table crosses into the kernel as an array
    -- rather than as a mapping — which is the whole reason a block used to be
    -- invented here, and the invented block is what the record then held.
    it("reports an answer with no blocks as an empty array", function()
        reset()
        queue_ok({
            id = "msg_empty",
            role = "assistant",
            content = {},
            stop_reason = "end_turn",
            usage = { input_tokens = 1, output_tokens = 0 },
        })

        local res = ask(anthropic_backend())

        expect(#res.content).to.equal(0)
        expect(getmetatable(res.content).__jsontype).to.equal("array")
        expect(res.usage.input_tokens).to.equal(1)
    end)

    it("passes the model's blocks through untouched", function()
        local blocks = { { type = "thinking", thinking = "..." }, { type = "text", text = "hi" } }
        expect(proto.response_blocks(blocks)).to.equal(blocks)

        -- No blocks: an empty array, said in the one way an empty Lua table
        -- can say it.
        local empty = proto.response_blocks({})
        expect(#empty).to.equal(0)
        expect(getmetatable(empty).__jsontype).to.equal("array")

        -- Not blocks at all: handed on as it came, for the kernel to refuse.
        for _, odd in ipairs({ "not a table", 42 }) do
            expect(proto.response_blocks(odd)).to.equal(odd)
        end
    end)

    -- A provider that names no reason still produced an answer, and the record
    -- says which of the two happened: the field is absent rather than empty.
    it("leaves an unnamed stop reason absent", function()
        reset()
        local response = text_response("hello")
        response.stop_reason = nil
        queue_ok(response)

        local res = ask(anthropic_backend())
        expect(res.stop_reason).to.equal(nil)
    end)
end)

-- ============================================================
-- 2. Where the conf and the request meet
-- ============================================================

describe("the request the backend builds", function()
    it("puts the neutral request on the wire beside the conf", function()
        reset()
        queue_ok(text_response())

        local backend = anthropic_backend({ max_tokens = 512 })
        local res = backend({
            messages = { { role = "user", content = "spec" } },
            system = "SYSTEM-MARKER",
            tools = { { name = "echo", description = "d", input_schema = { type = "object" } } },
        })
        expect(res ~= nil).to.be.truthy()

        local body = requests[1].body
        expect(body.model).to.equal("claude-haiku-4-5-20251001")
        expect(body.max_tokens).to.equal(512)
        expect(body.messages[1].content).to.equal("spec")
        expect(body.tools[1].name).to.equal("echo")
        -- The system prompt rides beside the messages, not inside them.
        expect(body.system[1].text).to.equal("SYSTEM-MARKER")
        expect(requests[1].method).to.equal("POST")
        expect(requests[1].url:find("/v1/messages", 1, true) ~= nil).to.equal(true)
    end)

    it("caps the output at 4096 when nothing names a limit", function()
        reset()
        queue_ok(text_response())
        ask(anthropic_backend())
        expect(requests[1].body.max_tokens).to.equal(4096)
    end)

    it("lets the request override a conf field for one call", function()
        reset()
        queue_ok(text_response())
        queue_ok(text_response())

        local backend = anthropic_backend({ max_tokens = 512 })
        backend({ messages = {}, max_tokens = 99 })
        backend({ messages = {} })

        expect(requests[1].body.max_tokens).to.equal(99)
        expect(requests[2].body.max_tokens).to.equal(512)
    end)

    it("merges the conf headers onto the wire", function()
        reset()
        queue_ok(oai_response())

        -- RunPod's proxy and Cloudflare's gate answer an unfamiliar User-Agent
        -- with a challenge page instead of the model.
        local backend = proto.backend({
            provider = "openai",
            api_key = "k",
            model = "gpt-4o-mini",
            headers = { ["User-Agent"] = "Mozilla/5.0" },
        })
        expect(ask(backend) ~= nil).to.be.truthy()

        expect(requests[1].headers["User-Agent"]).to.equal("Mozilla/5.0")
        expect(requests[1].headers["Authorization"]).to.equal("Bearer k")
    end)

    it("carries the transport knobs the conf sets", function()
        reset()
        queue_ok(text_response())
        ask(anthropic_backend({ timeout = 7, dump = "full" }))

        expect(requests[1].timeout).to.equal(7)
        expect(requests[1].dump).to.equal("full")
    end)

    it("refuses a provider this build does not speak", function()
        local backend, err = proto.backend({ provider = "nonesuch" })
        expect(backend).to.equal(nil)
        expect(tostring(err):find("nonesuch", 1, true) ~= nil).to.equal(true)
    end)
end)

-- ============================================================
-- 3. api_key / model resolution
-- ============================================================

describe("api_key and model resolution", function()
    it("falls back to the environment and the adapter default", function()
        reset()
        env["OPENAI_API_KEY"] = "env-key"
        queue_ok(oai_response())

        local backend = proto.backend({ provider = "openai" })
        expect(ask(backend) ~= nil).to.be.truthy()

        expect(requests[1].headers["Authorization"]).to.equal("Bearer env-key")
        expect(requests[1].body.model).to.equal("gpt-4o-mini")
    end)

    it("takes the model from the environment on the Anthropic path", function()
        reset()
        env["ANTHROPIC_MODEL"] = "claude-from-env"
        queue_ok(text_response())

        local backend = proto.backend({ provider = "anthropic", api_key = "k" })
        expect(ask(backend) ~= nil).to.be.truthy()
        expect(requests[1].body.model).to.equal("claude-from-env")
    end)

    it("fails before the request when no key can be found", function()
        reset()
        queue_ok(text_response())

        local backend = proto.backend({ provider = "anthropic" })
        local res, err = ask(backend)

        expect(res).to.equal(nil)
        -- The failure names the variable that was looked at.
        expect(tostring(err):find("ANTHROPIC_API_KEY", 1, true) ~= nil).to.equal(true)
        expect(#requests).to.equal(0)
    end)
end)

-- ============================================================
-- 4. Retries
-- ============================================================

describe("retries", function()
    it("recovers from a transient failure", function()
        reset()
        table.insert(queue, { status = 429 })
        queue_ok(text_response("recovered"))

        local res, err = ask(anthropic_backend())

        expect(err).to.equal(nil)
        expect(res.content[1].text).to.equal("recovered")
        expect(#requests).to.equal(2)
        expect(sleeps).to.equal(1)
    end)

    it("does not retry what cannot succeed", function()
        reset()
        table.insert(queue, { status = 400 })

        local res, err = ask(anthropic_backend())

        expect(res).to.equal(nil)
        expect(err).to.equal("API error 400 (invalid_request)")
        expect(#requests).to.equal(1)
        expect(sleeps).to.equal(0)
    end)

    it("gives up after max_retries and reports the last status", function()
        reset()
        table.insert(queue, { status = 429 })
        table.insert(queue, { status = 429 })

        local res, err = ask(anthropic_backend({ max_retries = 1 }))

        expect(res).to.equal(nil)
        expect(err).to.equal("API error 429 (rate_limit)")
        expect(#requests).to.equal(2)
        expect(sleeps).to.equal(1)
    end)

    it("takes none when the conf asks for none", function()
        reset()
        table.insert(queue, { status = 503 })

        local res, err = ask(anthropic_backend({ max_retries = 0 }))

        expect(res).to.equal(nil)
        expect(tostring(err):find("API error 503", 1, true) ~= nil).to.equal(true)
        expect(#requests).to.equal(1)
    end)
end)

-- ============================================================
-- 5. Answers that cannot be read
-- ============================================================

describe("an answer the backend cannot read", function()
    it("reports a body that does not decode", function()
        reset()
        -- 200 with no queued response: the body reaches the codec and raises.
        table.insert(queue, { status = 200 })

        local res, err = ask(anthropic_backend())

        expect(res).to.equal(nil)
        expect(err).to.equal("response JSON decode failed")
    end)

    it("reports what the adapter refused to parse", function()
        reset()
        queue_ok({ id = "msg_1", role = "assistant", stop_reason = "end_turn" })

        local res, err = ask(anthropic_backend())

        expect(res).to.equal(nil)
        expect(tostring(err):find("content", 1, true) ~= nil).to.equal(true)
    end)
end)

-- ============================================================
-- 6. Hooks
-- ============================================================

describe("the hooks", function()
    it("fire around the call with what each end saw", function()
        reset()
        queue_ok(text_response("hello"))

        local seen_request, requests_at_hook, seen_response
        local backend = anthropic_backend({
            on_request = function(info)
                seen_request = info
                requests_at_hook = #requests
            end,
            on_response = function(info)
                seen_response = info
            end,
        })
        expect(ask(backend) ~= nil).to.be.truthy()

        -- Before the POST, with the bytes that went on the wire.
        expect(requests_at_hook).to.equal(0)
        expect(seen_request.url:find("/v1/messages", 1, true) ~= nil).to.equal(true)
        expect(seen_request.headers["x-api-key"]).to.equal("k")
        expect(seen_request.body.model).to.equal("claude-haiku-4-5-20251001")
        expect(seen_request.body_json).to.equal(requests[1].body_json)

        -- After it, with the answer as it came back rather than as it parsed.
        expect(seen_response.status).to.equal(200)
        expect(type(seen_response.latency_ms)).to.equal("number")
        expect(type(seen_response.body)).to.equal("string")
        expect(type(seen_response.headers)).to.equal("table")
    end)

    -- The neutral result carries three fields; everything else the provider
    -- said is here or nowhere.
    it("hand the parsed answer, with what the result drops, to on_decoded", function()
        reset()
        local refused = text_response("I will not")
        refused.stop_reason = "refusal"
        refused.stop_details = { type = "refusal", category = "policy" }
        queue_ok(refused)

        local seen
        local res = ask(anthropic_backend({
            on_decoded = function(decoded)
                seen = decoded
            end,
        }))

        expect(res.stop_reason).to.equal("refusal")
        expect(res.stop_details).to.equal(nil)
        expect(seen.stop_details.category).to.equal("policy")
        expect(seen.content[1].text).to.equal("I will not")
    end)

    it("cannot fail the call by raising", function()
        reset()
        queue_ok(text_response("hello"))

        local res, err = ask(anthropic_backend({
            on_request = function()
                error("hook exploded", 0)
            end,
            on_response = function()
                error("hook exploded", 0)
            end,
            on_decoded = function()
                error("hook exploded", 0)
            end,
        }))

        expect(err).to.equal(nil)
        expect(res.content[1].text).to.equal("hello")
    end)

    it("keep the closure's own knobs off the wire", function()
        reset()
        queue_ok(text_response())

        ask(anthropic_backend({
            max_retries = 0,
            on_request = function() end,
            on_response = function() end,
            on_decoded = function() end,
        }))

        -- The conf keys that configure the closure do not reach the wire.
        local body = requests[1].body
        expect(body.on_request).to.equal(nil)
        expect(body.on_response).to.equal(nil)
        expect(body.on_decoded).to.equal(nil)
        expect(body.max_retries).to.equal(nil)
    end)
end)
