-- llm_proto_count_test.lua — mlua-lspec tests for the adapters' `count` and
-- `profile`: what a server knows about a request and about its model, asked
-- of the server's own surface.
--
-- Run via:
--   just test-lua llm_proto_count_test
--
-- What is pinned:
--   1. openai / vllm dialect: `count` posts the built chat form (messages,
--      tools, chat_template_kwargs) to `/tokenize` beside `/v1` and reads
--      `count`; `profile` reads the model card's `max_model_len` off
--      `/v1/models`
--   2. openai / llamacpp dialect: `count` renders through `/apply-template`
--      and counts the prompt with `/tokenize`; `profile` reads `n_ctx` off
--      `/props`
--   3. openai / ollama dialect: no counter (nil, err); `profile` reads
--      `<arch>.context_length` off `/api/show`
--   4. openai / api.openai.com: neither — nil, and an error that says so
--   5. anthropic: `count` posts the built body less the generation fields to
--      `count_tokens` and reads `input_tokens`; `profile` reads
--      `max_input_tokens` / `max_tokens` off `/v1/models/{model}`
--
-- The transport is stubbed the way llm_proto_backend_test stubs it: each
-- request body encodes to a sentinel the test can look the table back up
-- from, and each response decodes to whatever the test queued.

local describe, it, expect = lust.describe, lust.it, lust.expect

local env = { OPENAI_API_KEY = "sk-test", ANTHROPIC_API_KEY = "sk-ant-test" }
local queue = {}
local requests = {}
local encoded = {}
local encode_n = 0
local decodable = {}
local decode_n = 0

local function reset()
    queue = {}
    requests = {}
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
                    error("llm_proto_count_test: unexpected body to decode: " .. tostring(s), 0)
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
            sleep = function() end,
        },
    }
end

if not http then
    http = {
        request = function(url, opts)
            table.insert(requests, {
                url = url,
                method = opts.method,
                headers = opts.headers or {},
                body = opts.body and encoded[opts.body] or nil,
            })
            local entry = table.remove(queue, 1)
            if not entry then
                error("llm_proto_count_test: a probe the test did not queue: " .. url, 0)
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
local openai = proto.adapter("openai")
local anthropic = proto.adapter("anthropic")

local REQUEST = {
    system = "be brief",
    messages = { { role = "user", content = "hello" } },
    tools = { { name = "t", description = "a tool", input_schema = { type = "object" } } },
}

local function spec(extra)
    local s = { model = "m", max_tokens = 100 }
    for k, v in pairs(REQUEST) do
        s[k] = v
    end
    for k, v in pairs(extra or {}) do
        s[k] = v
    end
    return s
end

describe("openai adapter count / profile — vllm dialect", function()
    it("counts the built chat form through /tokenize beside /v1", function()
        reset()
        table.insert(queue, { status = 200, response = { count = 321, max_model_len = 32768 } })
        local n, err = openai.count(spec({ base_url = "http://localhost:8000/v1", dialect = "vllm" }))
        expect(err).to.equal(nil)
        expect(n).to.equal(321)
        expect(requests[1].url).to.equal("http://localhost:8000/tokenize")
        expect(requests[1].method).to.equal("POST")
        local sent = requests[1].body
        expect(sent.model).to.equal("m")
        expect(type(sent.messages)).to.equal("table")
        expect(type(sent.tools)).to.equal("table")
        expect(sent.add_generation_prompt).to.equal(true)
    end)

    it("reads the model card's max_model_len off /v1/models, and the caller's max_tokens as the room", function()
        reset()
        table.insert(queue, {
            status = 200,
            response = { data = { { id = "other", max_model_len = 1 }, { id = "m", max_model_len = 32768 } } },
        })
        local p, err = openai.profile(spec({ base_url = "http://localhost:8000/v1", dialect = "vllm" }))
        expect(err).to.equal(nil)
        expect(p.context_window).to.equal(32768)
        expect(p.max_output).to.equal(100)
        expect(requests[1].url).to.equal("http://localhost:8000/v1/models")
        expect(requests[1].method).to.equal("GET")
    end)

    it("answers nil and the server's words when the probe fails", function()
        reset()
        table.insert(queue, { status = 500, response = { message = "down" } })
        local n, err = openai.count(spec({ base_url = "http://localhost:8000/v1", dialect = "vllm" }))
        expect(n).to.equal(nil)
        expect(tostring(err):find("HTTP 500", 1, true) ~= nil).to.equal(true)
    end)
end)

describe("openai adapter count / profile — llamacpp dialect", function()
    it("renders through /apply-template and counts the prompt with /tokenize", function()
        reset()
        table.insert(queue, { status = 200, response = { prompt = "<rendered>" } })
        table.insert(queue, { status = 200, response = { tokens = { 1, 2, 3, 4, 5 } } })
        local n, err = openai.count(spec({ base_url = "http://localhost:8080/v1", dialect = "llamacpp" }))
        expect(err).to.equal(nil)
        expect(n).to.equal(5)
        expect(requests[1].url).to.equal("http://localhost:8080/apply-template")
        expect(requests[2].url).to.equal("http://localhost:8080/tokenize")
        expect(requests[2].body.content).to.equal("<rendered>")
        expect(requests[2].body.add_special).to.equal(false)
    end)

    it("reads n_ctx off /props", function()
        reset()
        table.insert(queue, { status = 200, response = { default_generation_settings = { n_ctx = 8192 } } })
        local p = openai.profile(spec({ base_url = "http://localhost:8080/v1", dialect = "llamacpp" }))
        expect(p.context_window).to.equal(8192)
        expect(requests[1].url).to.equal("http://localhost:8080/props")
    end)
end)

describe("openai adapter count / profile — ollama and api.openai.com", function()
    it("ollama has no counter, and its window is the training context_length off /api/show", function()
        reset()
        local n, err = openai.count(spec({ base_url = "http://localhost:11434/v1" }))
        expect(n).to.equal(nil)
        expect(tostring(err):find("ollama", 1, true) ~= nil).to.equal(true)
        expect(#requests).to.equal(0)

        table.insert(queue, { status = 200, response = { model_info = { ["llama.context_length"] = 8192 } } })
        local p = openai.profile(spec({ base_url = "http://localhost:11434/v1" }))
        expect(p.context_window).to.equal(8192)
        expect(requests[1].url).to.equal("http://localhost:11434/api/show")
        expect(requests[1].body.model).to.equal("m")
    end)

    it("api.openai.com has neither: nil, and an error that names the dialect", function()
        reset()
        local n, err = openai.count(spec({}))
        expect(n).to.equal(nil)
        expect(tostring(err):find("openai", 1, true) ~= nil).to.equal(true)
        local p, perr = openai.profile(spec({}))
        expect(p).to.equal(nil)
        expect(tostring(perr):find("context_window", 1, true) ~= nil).to.equal(true)
        expect(#requests).to.equal(0)
    end)
end)

describe("anthropic adapter count / profile", function()
    it("counts the built body, less the generation fields, through count_tokens", function()
        reset()
        table.insert(queue, { status = 200, response = { input_tokens = 42 } })
        local n, err = anthropic.count(spec({}))
        expect(err).to.equal(nil)
        expect(n).to.equal(42)
        expect(requests[1].url).to.equal("https://api.anthropic.com/v1/messages/count_tokens")
        local sent = requests[1].body
        expect(sent.model).to.equal("m")
        expect(sent.max_tokens).to.equal(nil)
        expect(type(sent.messages)).to.equal("table")
        expect(type(sent.tools)).to.equal("table")
        expect(requests[1].headers["x-api-key"]).to.equal("sk-ant-test")
    end)

    it(
        "reads max_input_tokens off /v1/models/{model}; the room is the caller's max_tokens, else the model's",
        function()
            reset()
            table.insert(queue, { status = 200, response = { max_input_tokens = 200000, max_tokens = 64000 } })
            local p = anthropic.profile(spec({}))
            expect(p.context_window).to.equal(200000)
            expect(p.max_output).to.equal(100)
            expect(requests[1].url).to.equal("https://api.anthropic.com/v1/models/m")
            expect(requests[1].method).to.equal("GET")

            reset()
            table.insert(queue, { status = 200, response = { max_input_tokens = 200000, max_tokens = 64000 } })
            local bare = anthropic.profile({ model = "m" })
            expect(bare.max_output).to.equal(64000)
        end
    )

    it("follows a caller's base_url", function()
        reset()
        table.insert(queue, { status = 200, response = { input_tokens = 7 } })
        anthropic.count(spec({ base_url = "http://proxy.local" }))
        expect(requests[1].url).to.equal("http://proxy.local/v1/messages/count_tokens")
    end)
end)

describe("llm_proto.estimate_tokens", function()
    it("is bytes over the request at 3.2 to the token by default, and grows with the request", function()
        local small = proto.estimate_tokens({ messages = { { role = "user", content = "hi" } } })
        local large = proto.estimate_tokens({ messages = { { role = "user", content = string.rep("x", 3200) } } })
        expect(small >= 1).to.equal(true)
        expect(large >= 1000).to.equal(true)
        expect(proto.estimate_tokens({ messages = { { role = "user", content = string.rep("x", 3200) } } }, 4) < large).to.equal(
            true
        )
    end)
end)
