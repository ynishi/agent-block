-- llm_port_spec.lua — mlua-lspec unit tests for knl_adapter's LLM Port: the
-- LLMPort interface, its shared shim (LLMPort:open), and the concrete anthropic
-- and openai providers' refusal vocabularies (classify -> { status, refusal? }).
--
-- Run via:
--   test_launch(code_file=".../knl_adapter/spec/llm_port_spec.lua",
--               search_paths=[".../blocks/lib"])  -- so require resolves
--
-- Everything is tested with no network and no real provider:
--   * a fake `llm_proto` is installed into package.loaded BEFORE
--     require("knl_adapter"), so the module's load-time require("llm_proto")
--     and proto.adapter("anthropic") capture the fake; and its `transport`
--     is the seam the shim now delegates the whole middle of a call to
--     (POST + retries + the classified non-200 + the decode).
--   * fake `std` (json encode/decode, task.sleep) and `http` globals stub the
--     device layer the shim's returned closure touches at call time.
--
-- What this proves (the Port, and classify behind it):
--   1 LLMPort.new  — rejects an impl missing any of build/parse/classify.
--   2 shim no literal + error path — with a *test* port whose parse/classify are
--     scripted, the closure returns { status = <the port's verdict>,
--     content/usage/stop_reason verbatim, refusal = <the port's detail> }; on a
--     non-200 it returns (nil, err). The shim itself contains no "refusal"
--     literal and no status of its own — it only calls self:classify.
--   3 anthropic classify() — end_turn/max_tokens/tool_use -> { status="ok" };
--     stop_reason=="refusal"/stop_details.type=="refusal" ->
--     { status="refused", refusal={kind="model"} } (Anthropic has no filter case).
--   5 openai classify() — model refusal (non-empty refusal string OR
--     stop_reason=="refusal") -> { refused, kind="model", detail }; #3
--     content_filter -> { refused, kind="content_filter" }; #4 empty-string
--     refusal is NOT a refusal -> { ok }; everything else -> { ok }.
--   invariant — across providers, refusal ~= nil iff status == "refused".
--   4 verbatim carry — content/usage/stop_reason pass through the shim as the
--     same tables/values the parse produced.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Fakes, installed BEFORE require("knl_adapter"). The fake llm_proto only needs
-- what the module touches at load (adapter) and what the Mapper reuses
-- (response_blocks), plus the transport seam. The real provider dialect is
-- never exercised: the shim is driven by a TEST port with scripted methods.
-- ─────────────────────────────────────────────────────────────────────────────

-- proto_script drives what the fake proto.adapter's build/parse return. The
-- anthropic status tests and the make_test_port shim tests never reach these
-- (they call status() directly, or use a separate scripted test port), so it is
-- only exercised by the openai "same shim drives a second provider" test: that
-- test needs M.openai:open -> openai_build -> proto_openai.build to yield a real
-- wire, and openai_parse -> proto_openai.parse to yield a result whose refusal
-- signal the test controls, so openai's own status() (not the shim) sets the
-- verdict.
local proto_script = {
    wire = { url = "http://example", headers = {}, body = {} },
    result = { content = {}, usage = {}, stop_reason = "end_turn" },
}

local fake_proto = {
    adapter = function(_)
        -- anthropic_build / anthropic_parse and openai_build / openai_parse
        -- delegate here. The anthropic path is never invoked (its tests call
        -- status() directly), the openai path is driven via proto_script.
        return {
            build = function(_)
                return proto_script.wire
            end,
            parse = function(_)
                return proto_script.result
            end,
        }
    end,
    -- The seam the shim delegates the middle of a call to. The real one is
    -- `llm_proto.transport`: POST with the retry policy, a classified non-200
    -- as (nil, err), the JSON decode, and a RAISE on a transport failure —
    -- which is what makes case 2d (the shim must not let that raise out) a
    -- case about the shim. This fake reproduces those four behaviours and
    -- nothing else; retries are the real transport's business, and the shim
    -- has no loop left to test.
    transport = function(wire, _opts)
        assert(type(wire) == "table" and wire.url, "transport was handed no wire")
        local resp = http.request(wire.url, {}) -- raises exactly as the host device does
        if resp.status ~= 200 then
            return nil, "API error " .. tostring(resp.status) .. " (server)"
        end
        return std.json.decode(resp.body), nil, { status = resp.status, latency_ms = 0 }
    end,
    -- The Mapper reuses llm_proto's content tagger; the real one marks an empty
    -- table as a JSON array (metatable { __jsontype = "array" }) and passes a
    -- non-empty one through. The fake mirrors that so the boundary tag can be
    -- asserted with no real llm_proto.
    response_blocks = function(content)
        if type(content) == "table" and #content == 0 then
            return setmetatable({}, { __jsontype = "array" })
        end
        return content
    end,
}

package.loaded["llm_proto"] = fake_proto

-- lshape resolves from blocks/lib (the search path). Used to assert the Port's
-- result shape (M.shapes.llm_result) and to drive the dev-mode boundary check.
local shape = require("lshape").check

-- The device layer the shim's closure reaches at call time. `http.request` is
-- scripted per test through `http_script`: `resp` is the value it returns, and
-- `raise` (when set) makes it raise a Lua error — the way the real Rust http
-- device signals a transport failure (timeout / connect / read-body / oversize).
local http_script = { resp = nil, raise = nil }

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
        if http_script.raise then
            error(http_script.raise)
        end
        return http_script.resp
    end,
}

local knl_adapter = require("knl_adapter")

-- ─────────────────────────────────────────────────────────────────────────────
-- Test port: scripted build / parse / classify, so the shim can be exercised
-- with no provider dialect at all. `seen` records what each method was handed.
-- classify returns { status = opts.verdict, refusal = opts.refusal } — the
-- verdict is whatever the test scripts, proving the shim computes no status.
-- ─────────────────────────────────────────────────────────────────────────────

--- Run `fn` with the dev-mode gate pinned on or off.
---
--- Cases about the boundary assert have to state which mode they mean: this
--- file runs under a bare test_launch (dev off) and under the lua-spec
--- runner (LSHAPE_CHECK=1, dev on), and a scripted verdict that the
--- llm_result shape rejects behaves differently in each. Pinning the mode
--- is what makes the case about the shim rather than about the environment.
local function with_dev_mode(on, fn)
    local saved = shape.is_dev_mode
    shape.is_dev_mode = function()
        return on
    end
    local results = table.pack(pcall(fn))
    shape.is_dev_mode = saved
    if not results[1] then
        error(results[2], 0)
    end
    return table.unpack(results, 2, results.n)
end

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
        classify = function(_, result)
            seen.classify_arg = result
            return { status = opts.verdict, refusal = opts.refusal }
        end,
    })
    return port, seen
end

describe("knl_adapter Port", function()
    it("1: LLMPort.new rejects an impl missing build/parse/classify", function()
        local function f() end

        -- missing build
        expect(pcall(knl_adapter.LLMPort.new, { parse = f, classify = f })).to.equal(false)
        -- missing parse
        expect(pcall(knl_adapter.LLMPort.new, { build = f, classify = f })).to.equal(false)
        -- missing classify
        expect(pcall(knl_adapter.LLMPort.new, { build = f, parse = f })).to.equal(false)
        -- not a table at all
        expect(pcall(knl_adapter.LLMPort.new, "nope")).to.equal(false)
        -- all three present: accepted
        expect(pcall(knl_adapter.LLMPort.new, { build = f, parse = f, classify = f })).to.equal(true)
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
        -- The scripted verdict is deliberately outside the llm_result
        -- vocabulary — that is the point of the case — so this one is about
        -- what the shim carries, with the boundary assert off. Case 11 is
        -- the other half: with it on, the same result raises.
        local resp, err = with_dev_mode(false, function()
            return llm({ messages = {} })
        end)

        expect(err).to.equal(nil)
        expect(resp.status).to.equal("SCRIPTED_VERDICT")
        -- the port scripted no refusal, so the shim carries none
        expect(resp.refusal).to.equal(nil)
        -- build received the request and the open() conf
        expect(type(seen.request.messages)).to.equal("table")
        expect(seen.conf.model).to.equal("test")
        -- non-empty content rides through the Mapper untouched; usage is
        -- normalized into the Port's strict shape (thinking_tokens filled in).
        expect(resp.content).to.equal(content)
        expect(resp.usage).to.equal({ input_tokens = 10, output_tokens = 3, thinking_tokens = 0 })
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
            classify = function()
                error("classify must not run when build failed")
            end,
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }

        local llm = port:open({})
        local resp, err = llm({ messages = {} })

        expect(resp).to.equal(nil)
        expect(err).to.equal("no api key")
    end)

    it("2d: shim transport raise — a raising http.request becomes (nil, err), no escape", function()
        local port = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = { content = {}, usage = {}, stop_reason = "end_turn" },
            verdict = "ok",
        })
        -- The real Rust http device RAISES a Lua error on transport failure. The
        -- shim must convert it into its declared (nil, err) contract, not let the
        -- raise escape the closure (which would crash the whole run).
        http_script.raise = "http timeout after 120s"

        local llm = port:open({ model = "test" })
        local ok, resp, err = pcall(llm, { messages = {} })
        http_script.raise = nil

        -- the closure itself did NOT raise
        expect(ok).to.equal(true)
        expect(resp).to.equal(nil)
        expect(type(err)).to.equal("string")
        expect(err:find("http timeout after 120s", 1, true) ~= nil).to.equal(true)
    end)

    it("3: anthropic classify() maps the refusal vocabulary to kind=model, everything else ok", function()
        local port = knl_adapter.anthropic

        expect(port:classify({ stop_reason = "end_turn" })).to.equal({ status = "ok" })
        expect(port:classify({ stop_reason = "refusal" })).to.equal({
            status = "refused",
            refusal = { kind = "model" },
        })
        -- refusal on stop_details even when stop_reason is not "refusal"
        expect(port:classify({ stop_reason = "end_turn", stop_details = { type = "refusal" } })).to.equal({
            status = "refused",
            refusal = { kind = "model" },
        })
        expect(port:classify({ stop_reason = "max_tokens" })).to.equal({ status = "ok" })
        expect(port:classify({ stop_reason = "tool_use" })).to.equal({ status = "ok" })
    end)

    it("4: non-empty content is carried through the Mapper untouched", function()
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

        -- a non-empty content is passed on as the same table (no tag rewrite);
        -- usage is normalized; stop_reason rides through.
        expect(resp.content).to.equal(content)
        expect(resp.usage).to.equal({ input_tokens = 7, output_tokens = 4096, thinking_tokens = 0 })
        expect(resp.stop_reason).to.equal("max_tokens")
    end)

    it("7: empty content is normalized to a tagged JSON array (the #2 bug fixed)", function()
        -- The openai refusal case parses to content = {} (empty). Untagged, the
        -- host bridge reads it as an empty MAPPING; the kernel's llm_response
        -- requires an empty ARRAY. The Mapper tags it so it crosses as [].
        local port = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = { content = {}, usage = { input_tokens = 1, output_tokens = 0 }, stop_reason = "refusal" },
            verdict = "refused",
            refusal = { kind = "model" },
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }

        local llm = port:open({ model = "test" })
        local resp = llm({ messages = {} })

        expect(type(resp.content)).to.equal("table")
        expect(#resp.content).to.equal(0)
        local mt = getmetatable(resp.content)
        expect(type(mt)).to.equal("table")
        expect(mt.__jsontype).to.equal("array")
    end)

    it("8: a missing or {} usage is normalized to zeros", function()
        -- usage = nil
        local port_nil = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = { content = {}, usage = nil, stop_reason = "end_turn" },
            verdict = "ok",
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }
        local resp_nil = port_nil:open({})({ messages = {} })
        expect(resp_nil.usage).to.equal({ input_tokens = 0, output_tokens = 0, thinking_tokens = 0 })
        expect(shape.check(resp_nil.usage, knl_adapter.shapes.llm_usage)).to.equal(true)

        -- usage = {}
        local port_empty = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = { content = {}, usage = {}, stop_reason = "end_turn" },
            verdict = "ok",
        })
        local resp_empty = port_empty:open({})({ messages = {} })
        expect(resp_empty.usage).to.equal({ input_tokens = 0, output_tokens = 0, thinking_tokens = 0 })
    end)

    it('9: an absent stop_reason stays nil, not fabricated to ""', function()
        local port = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = { content = {}, usage = {}, stop_reason = nil },
            verdict = "ok",
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }
        local resp = port:open({})({ messages = {} })
        expect(resp.stop_reason).to.equal(nil)
    end)

    it("10: the result shape validates a well-formed mapped result and rejects a malformed one", function()
        -- a well-formed mapped result passes the schema
        local port = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = {
                content = { { type = "text", text = "hi" } },
                usage = { input_tokens = 1, output_tokens = 2 },
                stop_reason = "end_turn",
            },
            verdict = "ok",
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }
        local resp = port:open({})({ messages = {} })
        expect(shape.check(resp, knl_adapter.shapes.llm_result)).to.equal(true)

        -- a deliberately malformed result (bad status) fails the schema
        local bad = {
            content = setmetatable({}, { __jsontype = "array" }),
            usage = { input_tokens = 0, output_tokens = 0, thinking_tokens = 0 },
            status = "weird",
        }
        expect((shape.check(bad, knl_adapter.shapes.llm_result))).to.equal(false)
    end)

    it("10b: a refusal without a refusal.kind is not an llm_result", function()
        -- The status discriminates: the refused variant REQUIRES the kind,
        -- because that is what a beat reports the refusal as. An optional
        -- field would have said only "sometimes".
        local without = {
            content = setmetatable({}, { __jsontype = "array" }),
            usage = { input_tokens = 0, output_tokens = 0, thinking_tokens = 0 },
            status = "refused",
        }
        expect((shape.check(without, knl_adapter.shapes.llm_result))).to.equal(false)
        without.refusal = { kind = "model" }
        expect(shape.check(without, knl_adapter.shapes.llm_result)).to.equal(true)
        -- and a kind outside the two the kernel knows is not one
        without.refusal = { kind = "vibes" }
        expect((shape.check(without, knl_adapter.shapes.llm_result))).to.equal(false)
    end)

    it("10c: an ok result carrying a refusal is not an llm_result either", function()
        -- Present exactly on a refusal, in both directions.
        local ok_with_refusal = {
            content = setmetatable({}, { __jsontype = "array" }),
            usage = { input_tokens = 0, output_tokens = 0, thinking_tokens = 0 },
            status = "ok",
            refusal = { kind = "model" },
        }
        expect((shape.check(ok_with_refusal, knl_adapter.shapes.llm_result))).to.equal(false)
    end)

    it("11: the boundary assert_dev raises in dev mode on a Mapper violation", function()
        -- A port whose status() yields a value outside {ok, refused} makes the
        -- Mapper build a result the RESULT shape rejects — a Mapper bug. In dev
        -- mode (LSHAPE_CHECK=1) the boundary assert_dev must raise.
        local port = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = { content = {}, usage = {}, stop_reason = "end_turn" },
            verdict = "not_a_status",
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }
        local llm = port:open({})

        -- prod (dev off): assert_dev is a no-op, the malformed result passes on
        local passed_in_prod = with_dev_mode(false, function()
            return (pcall(llm, { messages = {} }))
        end)
        expect(passed_in_prod).to.equal(true)

        -- dev on: the boundary catches the Mapper bug and raises
        local passed_in_dev = with_dev_mode(true, function()
            return (pcall(llm, { messages = {} }))
        end)
        expect(passed_in_dev).to.equal(false)
    end)

    it("11b: dev mode also holds a tool_use block to naming its call", function()
        -- The kernel reads `id` and `name` straight off a tool_use block, so
        -- a block that named neither is caught here rather than becoming a
        -- tool_result about a tool called "".
        local port = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = {
                content = { { type = "tool_use", input = {} } },
                usage = {},
                stop_reason = "tool_use",
            },
            verdict = "ok",
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }
        local llm = port:open({})

        local passed_in_dev = with_dev_mode(true, function()
            return (pcall(llm, { messages = {} }))
        end)
        expect(passed_in_dev).to.equal(false)

        -- a named one passes the same gate
        local named = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = {
                content = { { type = "tool_use", id = "c1", name = "echo", input = {} } },
                usage = {},
                stop_reason = "tool_use",
            },
            verdict = "ok",
        })
        local passed_named = with_dev_mode(true, function()
            return (pcall(named:open({}), { messages = {} }))
        end)
        expect(passed_named).to.equal(true)
    end)

    it("12: non-table content is an unreadable-response (nil, err), not a raise", function()
        local port = make_test_port({
            wire = { url = "http://example", headers = {}, body = {} },
            result = { content = "not a table", usage = {}, stop_reason = "end_turn" },
            verdict = "ok",
        })
        http_script.resp = { status = 200, body = "{}", headers = {} }
        local ok, resp, err = pcall(port:open({}), { messages = {} })
        expect(ok).to.equal(true) -- closure did not raise
        expect(resp).to.equal(nil)
        expect(type(err)).to.equal("string")
        expect(err:find("unreadable content", 1, true) ~= nil).to.equal(true)
    end)

    it("5: openai classify() distinguishes model refusal (#4) from content_filter (#3), everything else ok", function()
        local port = knl_adapter.openai

        -- openai.parse maps finish_reason "stop" -> "end_turn"; either spelling
        -- is a non-refusal and reads as ok.
        expect(port:classify({ stop_reason = "stop" })).to.equal({ status = "ok" })
        expect(port:classify({ stop_reason = "end_turn" })).to.equal({ status = "ok" })
        -- openai signals a model refusal by mapping the message `refusal` field
        -- onto stop_reason == "refusal" (no message carried -> detail nil).
        expect(port:classify({ stop_reason = "refusal" })).to.equal({
            status = "refused",
            refusal = { kind = "model" },
        })
        -- a model refusal with the message present: detail carries it, and a
        -- non-empty refusal string is a refusal even without stop_reason.
        expect(port:classify({ stop_reason = "refusal", refusal = "I can't help" })).to.equal({
            status = "refused",
            refusal = { kind = "model", detail = "I can't help" },
        })
        expect(port:classify({ stop_reason = "end_turn", refusal = "I can't help with that" })).to.equal({
            status = "refused",
            refusal = { kind = "model", detail = "I can't help with that" },
        })
        -- #3: a content_filter block is a refusal distinguished by kind.
        expect(port:classify({ stop_reason = "content_filter" })).to.equal({
            status = "refused",
            refusal = { kind = "content_filter" },
        })
        -- #4: an empty-string refusal is NOT a refusal (mirrors openai.lua's
        -- own non-empty check).
        expect(port:classify({ stop_reason = "stop", refusal = "" })).to.equal({ status = "ok" })
        -- everything else the finish_reason map produces is "ok".
        expect(port:classify({ stop_reason = "tool_use" })).to.equal({ status = "ok" })
        expect(port:classify({ stop_reason = "length" })).to.equal({ status = "ok" })
        expect(port:classify({ stop_reason = "max_tokens" })).to.equal({ status = "ok" })
    end)

    it("5b: classify invariant — refusal is present iff status == refused", function()
        local cases = {
            knl_adapter.anthropic:classify({ stop_reason = "end_turn" }),
            knl_adapter.anthropic:classify({ stop_reason = "refusal" }),
            knl_adapter.openai:classify({ stop_reason = "stop" }),
            knl_adapter.openai:classify({ stop_reason = "refusal", refusal = "no" }),
            knl_adapter.openai:classify({ stop_reason = "content_filter" }),
            knl_adapter.openai:classify({ stop_reason = "stop", refusal = "" }),
        }
        for _, verdict in ipairs(cases) do
            expect((verdict.refusal ~= nil)).to.equal(verdict.status == "refused")
        end
    end)

    it("6: the SAME shim drives openai — a second provider needed no shim edit", function()
        -- M.openai goes through the very same LLMPort:open the anthropic port
        -- and the test port use. build/parse delegate to the (scripted) openai
        -- proto adapter; the verdict must come from openai's classify(), not the
        -- shim.
        local llm = knl_adapter.openai:open({ model = "gpt-4o-mini" })

        -- refusal via openai's own signal on the parse result
        proto_script.result = { content = {}, usage = {}, stop_reason = "refusal" }
        http_script.resp = { status = 200, body = "{}", headers = {} }
        local resp, err = llm({ messages = {} })
        expect(err).to.equal(nil)
        expect(resp.status).to.equal("refused")
        expect(resp.refusal.kind).to.equal("model")
        -- the mapped result validates the RESULT schema (refusal present)
        expect(shape.check(resp, knl_adapter.shapes.llm_result)).to.equal(true)

        -- #3: a content_filter block flows through the SAME shim to a refusal
        -- distinguished by kind — the shim held no literal, classify did the work
        proto_script.result = { content = {}, usage = {}, stop_reason = "content_filter" }
        local resp_cf, err_cf = llm({ messages = {} })
        expect(err_cf).to.equal(nil)
        expect(resp_cf.status).to.equal("refused")
        expect(resp_cf.refusal.kind).to.equal("content_filter")
        expect(shape.check(resp_cf, knl_adapter.shapes.llm_result)).to.equal(true)

        -- non-refusal through the same closure -> ok, no refusal, schema-valid
        proto_script.result = { content = {}, usage = {}, stop_reason = "end_turn" }
        local resp2, err2 = llm({ messages = {} })
        expect(err2).to.equal(nil)
        expect(resp2.status).to.equal("ok")
        expect(resp2.refusal).to.equal(nil)
        expect(shape.check(resp2, knl_adapter.shapes.llm_result)).to.equal(true)
    end)
end)
