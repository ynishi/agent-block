-- policy_test.tl — `htl test` for `policy`: the readings that need no
-- kernel — `split` off a fake Port, `retry` over an outcome, `escalate`'s
-- default judgement — typed against the module's records. The thirteen
-- specs beside it hold the rest against a fake kernel.
local t = require("htl.test")
local kernel = require("knl")
local shared = require("policy.shared")
local policy = require("policy")

--- A Port that answers a fixed window and counts nothing.
local function port(window: integer, max_output: integer): shared.Port
    return {
        profile = function(_: shared.Port, _conf: any): shared.Profile
            return { context_window = window, max_output = max_output }
        end,
        count = function(_: shared.Port, _request: any, _conf: any): number
            return 0
        end,
    }
end

t.describe("split", function()
    t.it("reads the prompt's side off the profile, holding back only what the cap does not", function()
        local s = policy.split({ port = port(32768, 0), reserve = 6144 })
        t.expect(s.window):to_equal(32768)
        t.expect(s.max_output):to_equal(0)
        t.expect(s.held):to_equal(6144)
        t.expect(s.limit):to_equal(26624)
        t.expect(s.room):to_be_nil()
        local capped = policy.split({ port = port(32768, 4096), reserve = 2048 })
        t.expect(capped.held):to_equal(0)
        t.expect(capped.limit):to_equal(28672)
    end)
    t.it("answers the reply's room once the request is known, under the cap where there is one", function()
        t.expect(policy.split({ port = port(32768, 0), reserve = 6144, used = 24984 }).room):to_equal(7784)
        t.expect(policy.split({ port = port(32768, 4096), used = 1000 }).room):to_equal(4096)
    end)
    t.it("refuses an option it does not know, and a session as one", function()
        -- Through an untyped handle: the checker would refuse the key
        -- first, and what is under test is the module's own refusal.
        local split = policy.split as function(any): any
        t.expect(function()
            split({ port = port(100, 0), sesion = 1 })
        end):to_error("unknown option 'sesion'")
        t.expect(function()
            split({ port = port(100, 0), session = 1 })
        end):to_error("a session is an argument, never an option")
    end)
end)

t.describe("retry", function()
    local function failed(kind: string, retryable: boolean): kernel.OutcomeValue
        return { status = "error", kind = "state", detail = { kind = kind, retryable = retryable } }
    end
    t.it("asks again exactly when the failure says it is worth it, under max attempts", function()
        local again = policy.retry({ max = 3 })
        t.expect((again(failed("busy", true), 1))):to_be_truthy()
        t.expect((again(failed("busy", true), 3))):to_be_falsy()
        t.expect((again(failed("storage", false), 1))):to_be_falsy()
        t.expect((again({ status = "ok", out = {} }, 1))):to_be_falsy()
    end)
    t.it("with kinds named, retries those and only those, whatever the failure said", function()
        local again = policy.retry({ kinds = { "storage" } })
        t.expect((again(failed("storage", false), 1))):to_be_truthy()
        t.expect((again(failed("busy", true), 1))):to_be_falsy()
        t.expect(function()
            policy.retry({ kinds = { "sometimes" } })
        end):to_error("is not a knl failure kind")
    end)
    t.it("hands the delay the detail named back beside the answer", function()
        local again = policy.retry({})
        local ask, delay = again(
            { status = "error", kind = "call", detail = { kind = "rate_limited", retryable = true, retry_after = 7 } },
            1
        )
        t.expect(ask):to_be_truthy()
        t.expect(delay):to_equal(7)
    end)
end)

t.describe("escalate", function()
    t.it("swaps the llm on a refusal or a failure asking again would not fix, and not otherwise", function()
        local strong = function(): any
            return nil
        end
        local swapped: any = nil
        local device = {
            with = function(_: kernel.Device, config: kernel.DeviceConfig): kernel.Device
                swapped = config.llm
                return { llm = config.llm } as kernel.Device
            end,
        } as kernel.Device
        local escalate = policy.escalate({ strong = strong })
        t.expect(escalate({ status = "ok", out = {} }, device)):to_equal(device)
        t.expect(escalate({ status = "error", detail = { retryable = true } }, device)):to_equal(device)
        escalate({ status = "refused", reason = "no" }, device)
        t.expect(swapped):to_equal(strong)
    end)
end)
