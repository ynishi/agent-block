-- llm_proto_test.tl — `htl test` for `llm_proto`: the pure half, typed
-- against the module's own records. The specs beside it hold the wire,
-- the transport and the adapters.
local t = require("htl.test")
local host = require("host_types")

--- What `classify_error` reads a body with: the two JSON texts the tests
--- send, decoded by hand — the host's `std.json` is not in this state.
global std: host.Std = {
    json = {
        decode = function(text: string): any
            if text == '{"error":{"code":"insufficient_quota","message":"no"}}' then
                return { error = { code = "insufficient_quota", message = "no" } }
            elseif text == '{"message":"down"}' then
                return { message = "down" }
            end
            error("not JSON the test knows: " .. text)
        end,
        encode = function(_: any): string
            return "{}"
        end,
    },
}

local proto = require("llm_proto")

t.describe("normalize_tool_choice", function()
    t.it("reads both vocabularies into one", function()
        t.expect(proto.normalize_tool_choice("auto").kind):to_equal("auto")
        t.expect(proto.normalize_tool_choice("any").kind):to_equal("required")
        t.expect(proto.normalize_tool_choice({ type = "any" }).kind):to_equal("required")
        local by_name = proto.normalize_tool_choice({ type = "function", ["function"] = { name = "x" } })
        t.expect(by_name.kind):to_equal("tool")
        t.expect(by_name.name):to_equal("x")
    end)
    t.it("answers nil for nil, and an error for a name it does not know", function()
        local tc, err = proto.normalize_tool_choice(nil)
        t.expect(tc):to_be_nil()
        t.expect(err):to_be_nil()
        local _, bad = proto.normalize_tool_choice("sometimes")
        t.expect(bad):to_contain("invalid tool_choice")
        local _, unnamed = proto.normalize_tool_choice({ type = "tool" })
        t.expect(unnamed):to_contain("requires a tool name")
    end)
end)

t.describe("normalize_thinking", function()
    t.it("takes a boolean, a table, and defaults enabled to true", function()
        t.expect(proto.normalize_thinking(false).enabled):to_be_falsy()
        local by_effort = proto.normalize_thinking({ effort = "medium" })
        t.expect(by_effort.enabled):to_be_truthy()
        t.expect(by_effort.effort):to_equal("medium")
        t.expect(by_effort.mode):to_equal("auto")
        t.expect(proto.normalize_thinking({ budget_tokens = "8000" }).budget_tokens):to_equal(8000)
    end)
    t.it("refuses an effort, a budget and a mode it does not know", function()
        local _, e1 = proto.normalize_thinking({ effort = "max" })
        t.expect(e1):to_contain("invalid thinking.effort")
        local _, e2 = proto.normalize_thinking({ budget_tokens = 0 })
        t.expect(e2):to_contain("invalid thinking.budget_tokens")
        local _, e3 = proto.normalize_thinking({ mode = "loud" })
        t.expect(e3):to_contain("invalid thinking.mode")
    end)
end)

t.describe("classify_error", function()
    t.it("tells a quota 429 from a rate-limit 429, and reads retry-after", function()
        local quota = proto.classify_error(429, '{"error":{"code":"insufficient_quota","message":"no"}}', {})
        t.expect(quota.kind):to_equal("quota")
        t.expect(quota.retryable):to_be_falsy()
        t.expect(quota.message):to_equal("no")
        local limited = proto.classify_error(429, "", { ["Retry-After"] = "3" })
        t.expect(limited.kind):to_equal("rate_limit")
        t.expect(limited.retryable):to_be_truthy()
        t.expect(limited.retry_after):to_equal(3)
    end)
    t.it("classes the rest by status, with the server's word when it gave one", function()
        t.expect(proto.classify_error(503, nil, nil).kind):to_equal("overloaded")
        t.expect(proto.classify_error(401, nil, nil).retryable):to_be_falsy()
        t.expect(proto.classify_error(500, '{"message":"down"}', nil).message):to_equal("down")
        t.expect(proto.classify_error(418, nil, nil).message):to_equal("HTTP 418")
    end)
end)

t.describe("retry_delay", function()
    t.it("doubles from half a second to a cap, less a deterministic share", function()
        local none: proto.Classified = { kind = "server", retryable = true, message = "" }
        t.expect(proto.retry_delay(1, none, 0)):to_equal(0.5)
        t.expect(proto.retry_delay(2, none, 0)):to_equal(1)
        t.expect(proto.retry_delay(10, none, 0)):to_equal(8)
        t.expect(proto.retry_delay(2, none, 4)):to_equal(0.75)
    end)
    t.it("honours retry-after up to a minute", function()
        t.expect(proto.retry_delay(1, { kind = "rate_limit", retryable = true, retry_after = 7, message = "" }, 0)):to_equal(7)
        t.expect(proto.retry_delay(1, { kind = "rate_limit", retryable = true, retry_after = 600, message = "" }, 0)):to_equal(0.5)
    end)
end)

t.describe("health_of and server_root", function()
    t.it("reads a ping as ok / unavailable / down / unreachable", function()
        t.expect(proto.health_of({ status = 200, body = "", headers = {} }).kind):to_equal("ok")
        t.expect(proto.health_of({ status = 503, body = "loading", headers = {} }).kind):to_equal("unavailable")
        t.expect(proto.health_of({ status = 404, body = "", headers = {} }).alive):to_be_falsy()
        local gone = proto.health_of(nil, "connect refused")
        t.expect(gone.kind):to_equal("unreachable")
        t.expect(gone.message):to_equal("connect refused")
    end)
    t.it("hangs the compatible routes off the server root", function()
        t.expect(proto.server_root("http://localhost:8000/v1")):to_equal("http://localhost:8000")
        t.expect(proto.server_root("http://localhost:8000/v1/")):to_equal("http://localhost:8000")
        t.expect(proto.server_root("http://pod")):to_equal("http://pod")
    end)
end)

t.describe("estimate_tokens", function()
    t.it("counts the bytes of every string at the ratio given", function()
        t.expect(proto.estimate_tokens({ system = "abcd" }, 4)):to_equal(3)
        -- "messages" + "role" + "user" + "content" + "hello" = 28 bytes.
        t.expect(proto.estimate_tokens({ messages = { { role = "user", content = "hello" } } }, 1)):to_equal(28)
    end)
end)
