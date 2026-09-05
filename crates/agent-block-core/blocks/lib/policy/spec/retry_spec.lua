-- retry_spec.lua — mlua-lspec unit tests for `policy.retry`, the predicate a
-- caller's loop asks about an Outcome.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/retry_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- What this proves:
--   1 THE DECISION IS ON A KIND, NEVER ON A STATUS CLASS. A detail carrying a
--     503 / 429 / "5xx" and nothing that was classified is not retried, and no
--     naming of kinds can make it one — the vocabulary is closed on the two
--     lists knl publishes (`error_kinds` for a kernel failure,
--     `call_error_kinds` for a call that did not come off) and on nothing else;
--   2 `retry_after` in the detail rides back as the delay, and its absence is
--     an absent delay rather than a zero;
--   3 `max` counts attempts in total, the first one included;
--   4 refused / stopped / ok are never retried, whatever the kinds say;
--   5 without `kinds` the kernel's own `retryable` is the answer; with them,
--     the naming is;
--   6 the attempt count is the loop's and is required — a missing one is loud,
--     because read as zero it would make every failure retryable forever.

local describe, it, expect = lust.describe, lust.it, lust.expect

require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")
local Outcome = kernel.Outcome

--- An `err("state")` Outcome carrying the kernel's reading of a raise.
local function state(detail)
    return Outcome.err("state", detail)
end

--- The kernel's reading of a contended store: the one class it calls
--- retryable.
local function busy(extra)
    local detail = { kind = "busy", method = "reserve", retryable = true, message = "locked" }
    for k, v in pairs(extra or {}) do
        detail[k] = v
    end
    return state(detail)
end

--- The kernel's reading of a store that is gone: classified, not retryable.
local function storage(extra)
    local detail = { kind = "storage", method = "append", retryable = false, message = "the disk is gone" }
    for k, v in pairs(extra or {}) do
        detail[k] = v
    end
    return state(detail)
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.retry — construction", function()
    it("answers a predicate", function()
        expect(type(policy.retry({}))).to.be("function")
    end)

    it("closes `kinds` on the two vocabularies a failure can carry", function()
        -- Both lists are published by knl and read from there: the kernel's own
        -- classes and the adapter's classification of a call that did not come
        -- off. A word from neither is a typo, and a typo that silently never
        -- matched would be a retry policy that never retries.
        expect(function()
            policy.retry({ kinds = { "busy", "storage" } })
        end).to_not.fail()
        expect(function()
            policy.retry({ kinds = { "rate_limited", "overloaded" } })
        end).to_not.fail()
        expect(function()
            policy.retry({ kinds = { "busy", "transport" } })
        end).to_not.fail()
        expect(function()
            policy.retry({ kinds = { "429" } })
        end).to.fail()
        expect(function()
            policy.retry({ kinds = { "throttled" } })
        end).to.fail()
        expect(function()
            policy.retry({ kinds = "busy" })
        end).to.fail()
    end)

    it("insists on a whole number of attempts", function()
        for _, bad in ipairs({ 0, -2, 1.5 }) do
            expect(function()
                policy.retry({ max = bad })
            end).to.fail()
        end
        expect(function()
            policy.retry({ maximum = 3 })
        end).to.fail()
    end)
end)

describe("policy.retry — what is retried", function()
    it("defers to the kernel's judgement when no kinds are named", function()
        local again = policy.retry({ max = 3 })
        expect(again(busy(), 1)).to.be(true)
        expect(again(storage(), 1)).to.be(false)
    end)

    it("takes the naming as the whole answer when kinds are given", function()
        -- A caller may decide a dead store is worth one more try; it may also
        -- decide contention is not. Both are judgements the kernel does not
        -- make for anyone, and naming kinds is how they are said.
        local widened = policy.retry({ kinds = { "storage" }, max = 3 })
        expect(widened(storage(), 1)).to.be(true)
        expect(widened(busy(), 1)).to.be(false)
    end)

    it("never reads an HTTP status class", function()
        -- A provider's 503 is one word for several failures. The kernel
        -- classified none of these, so none of them is retried — and naming
        -- every kind there is does not change that, because a status is not a
        -- kind.
        local everything = policy.retry({ kinds = kernel.shapes.error_kinds, max = 5 })
        local defaulted = policy.retry({ max = 5 })
        for _, detail in ipairs({
            { retryable = false, message = "HTTP 503", status = 503 },
            { retryable = false, message = "HTTP 429", http_status = 429 },
            { retryable = false, message = "5xx from the provider", status_class = "5xx" },
        }) do
            expect(everything(state(detail), 1)).to.be(false)
            expect(defaulted(state(detail), 1)).to.be(false)
        end
    end)

    it("retries a call failure the adapter classified, and honours its retry_after", function()
        -- The gap this round closed. A call that did not come off used to
        -- carry a sentence, so `detail` was a string, so this predicate
        -- answered false for every provider failure — the class of failure
        -- most often worth asking again about. The adapter classifies it now
        -- (`knl.shapes.call_error`: kind / retryable / retry_after), and what
        -- it produces is read by the two fields this predicate has always
        -- decided on. Nothing here reads a status; the 429 rides along on the
        -- detail as a fact, and `rate_limited` is what is acted on.
        local again = policy.retry({ max = 3 })
        local ask, delay = again(
            Outcome.err("call", {
                kind = "rate_limited",
                retryable = true,
                retry_after = 30,
                message = "API error 429 (rate_limit)",
                status = 429,
            }),
            1
        )
        expect(ask).to.be(true)
        expect(delay).to.be(30)

        -- And one the adapter called not-retryable is still not retried.
        local refused_credentials =
            Outcome.err("call", { kind = "auth", retryable = false, message = "API error 401 (auth)", status = 401 })
        expect(again(refused_credentials, 1)).to.be(false)
    end)

    it("fires on a call-error kind a caller named, and only on that one", function()
        -- Naming is the whole answer in both vocabularies alike: `transport` is
        -- retryable by the adapter's own judgement, and a caller that asked for
        -- the rate limit and nothing else gets exactly that.
        local only_rate = policy.retry({ kinds = { "rate_limited" }, max = 3 })
        local limited = Outcome.err("call", {
            kind = "rate_limited",
            retryable = true,
            retry_after = 12,
            message = "API error 429 (rate_limit)",
            status = 429,
        })
        local dropped =
            Outcome.err("call", { kind = "transport", retryable = true, message = "connection reset by peer" })

        local ask, delay = only_rate(limited, 1)
        expect(ask).to.be(true)
        expect(delay).to.be(12)
        expect(only_rate(dropped, 1)).to.be(false)

        -- And a policy that named both asks again about either.
        local either = policy.retry({ kinds = { "rate_limited", "transport" }, max = 3 })
        expect(either(dropped, 1)).to.be(true)
    end)

    it("does not retry a failure whose detail is a sentence", function()
        -- `conf` / `filter` / `call` report a message, not a reading: there is
        -- no kind on them to decide from.
        local again = policy.retry({ max = 5 })
        expect(again(Outcome.err("call", "the provider said no"), 1)).to.be(false)
        expect(again(Outcome.err("conf", "fold failed: boom"), 1)).to.be(false)
        expect(again(Outcome.err("filter", "filter #1 returned nil"), 1)).to.be(false)
    end)

    it("never retries ok, refused or stopped", function()
        local again = policy.retry({ kinds = kernel.shapes.error_kinds, max = 5 })
        expect(again(Outcome.ok({ beat = "b1" }), 1)).to.be(false)
        expect(again(Outcome.refused("model", { beat = "b1" }), 1)).to.be(false)
        expect(again(Outcome.stopped("budget", "beats"), 1)).to.be(false)
    end)
end)

describe("policy.retry — the bound", function()
    it("counts attempts in total, the first one included", function()
        local again = policy.retry({ max = 3 })
        expect(again(busy(), 1)).to.be(true)
        expect(again(busy(), 2)).to.be(true)
        expect(again(busy(), 3)).to.be(false)
        expect(again(busy(), 4)).to.be(false)
    end)

    it("max = 1 is one attempt and no retry", function()
        expect(policy.retry({ max = 1 })(busy(), 1)).to.be(false)
    end)

    it("is loud when the loop forgets its count", function()
        local again = policy.retry({ max = 3 })
        for _, bad in ipairs({ 0, -1, 1.5 }) do
            expect(function()
                again(busy(), bad)
            end).to.fail()
        end
        expect(function()
            again(busy())
        end).to.fail()
    end)
end)

describe("policy.retry — the delay", function()
    it("hands back retry_after when the detail names one", function()
        local ask, delay = policy.retry({ max = 3 })(busy({ retry_after = 2.5 }), 1)
        expect(ask).to.be(true)
        expect(delay).to.be(2.5)
    end)

    it("hands back no delay when the detail names none", function()
        local ask, delay = policy.retry({ max = 3 })(busy(), 1)
        expect(ask).to.be(true)
        expect(delay).to.be(nil)
    end)

    it("does not read a retry_after that is not a number of seconds", function()
        local ask, delay = policy.retry({ max = 3 })(busy({ retry_after = "120" }), 1)
        expect(ask).to.be(true)
        expect(delay).to.be(nil)
    end)

    it("names no delay on a refusal to retry", function()
        local ask, delay = policy.retry({ max = 3 })(storage({ retry_after = 9 }), 1)
        expect(ask).to.be(false)
        expect(delay).to.be(nil)
    end)
end)

describe("policy.retry — in a caller's loop", function()
    it("bounds a run of contended reserves and reports the last outcome", function()
        -- The kernel makes exactly one attempt per beat and never retries on
        -- its own (knl's header); asking again is this predicate's answer and
        -- the loop's action.
        local reserved = 0
        local session = kernel.open({ owner = "spec", budget = { amount = 10, tag = "beats" } })
        session.reserve = function()
            reserved = reserved + 1
            error("knl: reserve: busy: locked")
        end
        local device = kernel.device({ llm = function() end })
        local again = policy.retry({ max = 3 })

        local attempt, outcome = 0, nil
        repeat
            outcome = kernel.beat(session, device)
            attempt = attempt + 1
        until not again(outcome, attempt)

        expect(attempt).to.be(3)
        expect(reserved).to.be(3)
        expect(Outcome.is_error(outcome)).to.be(true)
        expect(outcome.kind).to.be("state")
        expect(outcome.detail.kind).to.be("busy")
    end)
end)
