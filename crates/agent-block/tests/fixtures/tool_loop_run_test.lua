-- tool_loop_run_test.lua — mlua-lspec tests that drive tool_loop.run itself.
--
-- Run via:
--   just test-lua tool_loop_run_test   # this file
--   just test-lua                      # every spec fixture
--
-- Nothing drove `run` from a spec, so its result contract had no call site
-- exercising it. These go through `M.run`, which means the dev-mode assert on
-- `M.shapes.result` fires on every case — the early returns included, since a
-- refusal to start is a shape too.
--
-- The HTTP round trip is stubbed at the two ends rather than by writing a JSON
-- codec: the request body is encoded to a sentinel and the response body is
-- decoded back to a canned table. What is under test is the loop's own result
-- assembly; the wire format has llm_proto_test, and the codec has the bridge.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- The response the stubbed transport hands back, and the token that stands in
-- for its serialized form.
local RESPONSE_BODY = "<canned-response-body>"
local canned_response = nil

if not log then
    log = { warn = function() end, info = function() end, debug = function() end, error = function() end }
end
if not tool then
    tool = { register = function() end }
end
if not std then
    std = {
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
        json = {
            encode = function(_v)
                return "<encoded-request-body>"
            end,
            decode = function(s)
                if s == RESPONSE_BODY then
                    return canned_response
                end
                error("tool_loop_run_test: unexpected body to decode: " .. tostring(s))
            end,
        },
        -- Only read to stamp request latency onto the observability hook.
        time = {
            now = function()
                return 0
            end,
        },
    }
end

local http_status = 200

if not http then
    http = {
        request = function(_url, _opts)
            return { status = http_status, body = RESPONSE_BODY, headers = {} }
        end,
    }
end

local lshape = require("lshape")
local check = lshape.check
local tool_loop = require("tool_loop")

local function text_response(text)
    return {
        id = "msg_1",
        role = "assistant",
        content = { { type = "text", text = text } },
        stop_reason = "end_turn",
        usage = { input_tokens = 1, output_tokens = 2 },
    }
end

local function run(opts)
    opts.llm = opts.llm or { provider = "anthropic", model = "claude-haiku-4-5-20251001", api_key = "k" }
    return tool_loop.run(opts)
end

describe("tool_loop.run result contract", function()
    it("checks the contract at the call site, not only as data", function()
        expect(check.is_dev_mode()).to.equal(true)
    end)

    it("refuses a missing prompt as a failure, not an exception", function()
        local res = tool_loop.run({})
        expect(res.ok).to.equal(false)
        expect(res.error).to.equal("prompt is required")
        expect(res.turns).to.equal(0)
        expect(check.check(res, tool_loop.shapes.result)).to.equal(true)
    end)

    it("refuses an unknown provider the same way", function()
        local res = tool_loop.run({ prompt = "ask", llm = { provider = "nonesuch" } })
        expect(res.ok).to.equal(false)
        expect(res.error ~= nil).to.be.truthy()
        expect(check.check(res, tool_loop.shapes.result)).to.equal(true)
    end)

    it("returns the model's text when it stops without asking for tools", function()
        http_status = 200
        canned_response = text_response("hello")
        local res = run({ prompt = "ask" })
        expect(res.ok).to.equal(true)
        expect(res.content).to.equal("hello")
        expect(check.check(res, tool_loop.shapes.result)).to.equal(true)
    end)

    -- Without this the fixture would only show that valid results are valid,
    -- and removing the wrapper would break nothing here. `stop_reason` is
    -- passed through from the decoded response, so a provider sending
    -- something other than a string is the drift the contract is placed to stop.
    it("raises when the assembled result does not match, rather than passing it on", function()
        http_status = 200
        canned_response = text_response("hello")
        canned_response.stop_reason = 42

        local ok, err = pcall(run, { prompt = "ask" })
        expect(ok).to.equal(false)
        expect(tostring(err):find("shape violation", 1, true) ~= nil).to.be.truthy()
        expect(tostring(err):find("tool_loop.run result", 1, true) ~= nil).to.be.truthy()
    end)

    it("reports a non-retryable API status as a failure", function()
        http_status = 400
        canned_response = text_response("unused")
        local res = run({ prompt = "ask", max_retries = 0 })
        expect(res.ok).to.equal(false)
        expect(tostring(res.error):find("API error 400", 1, true) ~= nil).to.be.truthy()
        expect(check.check(res, tool_loop.shapes.result)).to.equal(true)
        http_status = 200
    end)

    -- The keys of a plain run, pinned. tool_loop took an `opts.session`
    -- once, routing the call through a `s:call` the bridge never had; that
    -- path is gone (see the `knl` module doc) and running a loop over
    -- a recorded session is `knl.beat(session, device)` now. What this case
    -- holds is that nothing was left behind by the removal: the same keys,
    -- the same values, no field appearing from a feature that is not there.
    it("returns exactly the keys of a standalone run", function()
        http_status = 200
        canned_response = text_response("hello")
        local res = run({ prompt = "ask" })

        local keys = {}
        for k in pairs(res) do
            table.insert(keys, k)
        end
        table.sort(keys)
        expect(table.concat(keys, ",")).to.equal("content,messages,ok,stop_reason,tool_calls,turns,usage")
        expect(res.ok).to.equal(true)
        expect(res.content).to.equal("hello")
        expect(res.turns).to.equal(1)
        expect(#res.messages).to.equal(2)
        expect(res.usage.input_tokens).to.equal(1)
        expect(res.usage.output_tokens).to.equal(2)
    end)
end)

-- The block normalization that used to live here moved to `llm_proto.backend`
-- along with the rest of the transport; its cases are in
-- llm_proto_backend_test.
