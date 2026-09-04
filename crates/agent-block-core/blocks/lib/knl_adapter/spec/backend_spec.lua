-- backend_spec.lua — mlua-lspec unit tests for knl_adapter's Port form: the
-- LLMPort interface, its shared shim (LLMPort:open), and the concrete anthropic
-- provider's refusal vocabulary.
--
-- Run via:
--   test_launch(code_file=".../knl_adapter/spec/backend_spec.lua",
--               search_paths=[".../blocks/lib"])  -- so require resolves
--
-- Everything is tested with no network and no real provider:
--   * a fake `llm_proto` is installed into package.loaded BEFORE
--     require("knl_adapter"), so the module's load-time require("llm_proto")
--     and proto.adapter("anthropic") capture the fake; and its exported
--     classify_error / retry_delay drive the shim's retry loop.
--   * fake `std` (json encode/decode, task.sleep) and `http` globals stub the
--     device layer the shim's returned closure touches at call time.
--
-- What this proves (device-if-design.md §1 Port + §2 status behind the Port):
--   1 LLMPort.new  — rejects an impl missing any of build/parse/status.
--   2 shim no literal + error path — with a *test* port whose parse/status are
--     scripted, the closure returns { status = <the port's verdict>,
--     content/usage/stop_reason verbatim }; on a non-200 it returns (nil, err).
--     The shim itself contains no "refusal" literal and no status of its own.
--   3 anthropic status() — end_turn/max_tokens/tool_use -> "ok";
--     stop_reason=="refusal" and stop_details.type=="refusal" -> "refused".
--   4 verbatim carry — content/usage/stop_reason pass through the shim as the
--     same tables/values the parse produced.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Fakes, installed BEFORE require("knl_adapter"). The fake llm_proto only needs
-- what the module touches at load (adapter) and what the shim's retry loop
-- reuses (classify_error / retry_delay). The real provider dialect is never
-- exercised: the shim is driven by a TEST port with scripted methods.
-- ─────────────────────────────────────────────────────────────────────────────

local fake_proto = {
    adapter = function(_)
        -- anthropic_build / anthropic_parse delegate here, but the shim tests
        -- use a separate test port, and the status tests call status() only —
        -- so these are never invoked and can be inert.
        return {
            build = function() end,
            parse = function() end,
        }
    end,
    -- Non-retryable by default: a non-200 ends the loop at once and becomes an
    -- error, which is the path the error case checks.
    classify_error = function(_, _, _)
        return { retryable = false, kind = "server" }
    end,
    retry_delay = function()
        return 0
    end,
}

package.loaded["llm_proto"] = fake_proto

-- The device layer the shim's closure reaches at call time. `http.request` is
-- scripted per test through `http_script`.
local http_script = { resp = nil }

_G.std = {
    json = {
        encode = function(_)
            return "ENCODED_BODY"
        end,
        decode = function(_)
            return { decoded = true }
        end,
    },
    task = { sleep = function() end },
}

_G.http = {
    request = function(_, _)
        return http_script.resp
    end,
}

local knl_adapter = require("knl_adapter")

-- ─────────────────────────────────────────────────────────────────────────────
-- Test port: scripted build / parse / status, so the shim can be exercised with
-- no provider dialect at all. `seen` records what each method was handed.
-- ─────────────────────────────────────────────────────────────────────────────

local function make_test_port(opts)
    local seen = {}
    local port = knl_adapter.LLMPort.new({
        build = function(_, request, conf)
            seen.request = request
            seen.conf = conf
            return opts.wire
        end,
        parse = function(_, raw)
            seen.raw = raw
            return opts.result, opts.parse_err
        end,
        status = function(_, result)
            seen.status_arg = result
            return opts.verdict
        end,
    })
    return port, seen
end

describe("knl_adapter Port", function()
    it("1: LLMPort.new rejects an impl missing build/parse/status", function()
        local function f() end

        -- missing build
        expect(pcall(knl_adapter.LLMPort.new, { parse = f, status = f })).to.equal(false)
        -- missing parse
        expect(pcall(knl_adapter.LLMPort.new, { build = f, status = f })).to.equal(false)
        -- missing status
        expect(pcall(knl_adapter.LLMPort.new, { build = f, parse = f })).to.equal(false)
        -- not a table at all
        expect(pcall(knl_adapter.LLMPort.new, "nope")).to.equal(false)
        -- all three present: accepted
        expect(pcall(knl_adapter.LLMPort.new, { build = f, parse = f, status = f })).to.equal(true)
    end)

    it("2: shim has no provider literal — status is the port's verdict, values verbatim", function()
        local content = { { type = "text", text = "hi" } }
        local usage = { input_tokens = 10, output_tokens = 3 }
        local port, seen = make_test_port({
            wire = { url = "http://example", headers = { h = "1" }, body = { b = "2" } },
            result = { content = content, usage = usage, stop_reason = "end_turn" },
            -- A made-up verdict the shim cannot have computed itself: proves the
            -- status comes only from the port, with zero classification in the shim.
            verdict = "SCRIPTED_VERDICT",
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }

        local llm = port:open({ model = "test" })
        local resp, err = llm({ messages = {} })

        expect(err).to.equal(nil)
        expect(resp.status).to.equal("SCRIPTED_VERDICT")
        -- build received the request and the open() conf
        expect(type(seen.request.messages)).to.equal("table")
        expect(seen.conf.model).to.equal("test")
        -- verbatim: the SAME tables the parse produced, not copies
        expect(resp.content).to.equal(content)
        expect(resp.usage).to.equal(usage)
        expect(resp.stop_reason).to.equal("end_turn")
    end)

    it("2b: shim error path — a non-200 returns (nil, err)", function()
        local port = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = { content = {}, usage = {}, stop_reason = "end_turn" },
            verdict = "ok",
        })
        http_script.resp = { status = 500, body = "boom", headers = {} }

        local llm = port:open({ model = "test" })
        local resp, err = llm({ messages = {} })

        expect(resp).to.equal(nil)
        expect(type(err)).to.equal("string")
        expect(err:find("500", 1, true) ~= nil).to.equal(true)
    end)

    it("2c: shim build failure — (nil, err) short-circuits before any http", function()
        local port = knl_adapter.LLMPort.new({
            build = function()
                return nil, "no api key"
            end,
            parse = function()
                error("parse must not run when build failed")
            end,
            status = function()
                error("status must not run when build failed")
            end,
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }

        local llm = port:open({})
        local resp, err = llm({ messages = {} })

        expect(resp).to.equal(nil)
        expect(err).to.equal("no api key")
    end)

    it("3: anthropic status() maps the refusal vocabulary, everything else ok", function()
        local port = knl_adapter.anthropic

        expect(port:status({ stop_reason = "end_turn" })).to.equal("ok")
        expect(port:status({ stop_reason = "refusal" })).to.equal("refused")
        -- refusal on stop_details even when stop_reason is not "refusal"
        expect(port:status({ stop_reason = "end_turn", stop_details = { type = "refusal" } })).to.equal("refused")
        expect(port:status({ stop_reason = "max_tokens" })).to.equal("ok")
        expect(port:status({ stop_reason = "tool_use" })).to.equal("ok")
    end)

    it("4: content/usage/stop_reason are carried verbatim through the shim", function()
        local content = { { type = "tool_use", id = "c1", name = "t", input = {} } }
        local usage = { input_tokens = 7, output_tokens = 4096 }
        local port = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = { content = content, usage = usage, stop_reason = "max_tokens" },
            verdict = "ok",
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }

        local llm = port:open({})
        local resp = llm({ messages = {} })

        expect(resp.content).to.equal(content)
        expect(resp.usage).to.equal(usage)
        expect(resp.stop_reason).to.equal("max_tokens")
    end)
end)
