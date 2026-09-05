-- stagnation_spec.lua — mlua-lspec unit tests for `policy.stagnation`, the
-- predicate a caller's loop asks between beats.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/stagnation_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- What this proves:
--   1 THE TWO REASONS ARE INDEPENDENT. "repeated" is about a beat doing the
--     same thing again and reads signatures; "no_progress" is about a beat
--     doing nothing and reads what was written. A run of repeating tool calls
--     never answers "no_progress" (it made progress), and a run of empty beats
--     never answers "repeated" (it has no signature to repeat);
--   2 the default signature is the tool name and its input, and nothing that
--     changes on its own: two beats calling the same tool with the same
--     arguments repeat even though their call ids, beat ids, seq numbers and
--     token counts all differ;
--   3 the thresholds are parameters — the defaults are a starting point, and a
--     spec that pinned 3 as truth would make them one;
--   4 `signature` is the caller's, and it is what decides what "the same"
--     means for the channel being run;
--   5 the predicate holds no counters: it derives every verdict from the log,
--     so asking twice answers twice the same and a freshly built predicate
--     agrees with one that has been asked before.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")
local Outcome = kernel.Outcome

--- Run `n` beats of `device` on a fresh session and hand the session back.
local function run(device_of, n)
    local session = support.session({ budget = { amount = 1000, tag = "beats" } })
    support.seed(session, "q")
    local device = device_of(session)
    for _ = 1, n do
        kernel.beat(session, device)
    end
    return session
end

--- A device that calls one tool with one input, over and over. The call id
--- changes every beat, which is exactly what the signature must ignore.
local function repeating_tool(input)
    local at = 0
    return function(_session)
        return kernel.device({
            llm = function(_request)
                at = at + 1
                return support.calls("call-" .. at, "search", input or { q = "same" })
            end,
            tools = support.tool("search", "no results"),
        })
    end
end

--- A device that answers with nothing at all: an empty content array, no tool
--- call, nothing written but the response envelope.
local function empty_answers()
    return function(_session)
        return kernel.device({ llm = support.always(support.answer({})) })
    end
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.stagnation — construction", function()
    it("answers a predicate over a session", function()
        local stalled = policy.stagnation({})
        expect(type(stalled)).to.be("function")
        expect(stalled(support.session())).to.be(nil)
    end)

    it("insists a repetition take at least two beats", function()
        expect(function()
            policy.stagnation({ same = 1 })
        end).to.fail()
        expect(function()
            policy.stagnation({ same = 2.5 })
        end).to.fail()
        expect(function()
            policy.stagnation({ no_progress = 0 })
        end).to.fail()
        expect(function()
            policy.stagnation({ signature = "not a function" })
        end).to.fail()
    end)

    it("refuses an option it does not know, a session most of all", function()
        expect(function()
            policy.stagnation({ smae = 3 })
        end).to.fail()
        local ok, err = pcall(policy.stagnation, { session = support.session() })
        expect(ok).to.be(false)
        expect(tostring(err):find("an argument", 1, true) ~= nil).to.be(true)
    end)
end)

describe("policy.stagnation — repeated", function()
    it("fires when the last `same` beats make one call", function()
        local session = run(repeating_tool(), 3)
        expect(policy.stagnation({ same = 2 })(session)).to.be("repeated")
        expect(policy.stagnation({ same = 3 })(session)).to.be("repeated")
        -- and not before there are enough beats to be a pattern
        expect(policy.stagnation({ same = 4 })(session)).to.be(nil)
    end)

    it("ignores the ids, the counts and the ordering of the input", function()
        -- Every beat above minted a fresh call id and a fresh beat id, and the
        -- fake reports usage on every response. If any of that reached the
        -- signature, nothing would ever repeat.
        local session = run(repeating_tool({ q = "same", page = 1, deep = { b = 2, a = 1 } }), 3)
        expect(policy.stagnation({ same = 3 })(session)).to.be("repeated")
        expect(#support.beat_ids(session)).to.be(3)
    end)

    it("does not fire when the input changes", function()
        local at = 0
        local session = run(function(_s)
            return kernel.device({
                llm = function(_request)
                    at = at + 1
                    return support.calls("c" .. at, "search", { q = "page " .. at })
                end,
                tools = support.tool("search", "no results"),
            })
        end, 3)
        expect(policy.stagnation({ same = 2 })(session)).to.be(nil)
    end)

    it("does not fire when the tool changes", function()
        local at = 0
        local session = run(function(_s)
            return kernel.device({
                llm = function(_request)
                    at = at + 1
                    return support.calls("c" .. at, at == 1 and "search" or "read", { q = "x" })
                end,
                tools = {
                    search = support.tool("search", "r").search,
                    read = support.tool("read", "r").read,
                },
            })
        end, 2)
        expect(policy.stagnation({ same = 2 })(session)).to.be(nil)
    end)

    it("takes the caller's signature when one is given", function()
        -- Two beats answering the same TEXT: the default has no signature for
        -- either (no tool call), and a channel that repeats this way says so
        -- for itself.
        local session = run(function(_s)
            return kernel.device({ llm = support.always(support.text("I cannot help with that.")) })
        end, 2)
        expect(policy.stagnation({ same = 2 })(session)).to.be(nil)

        local by_text = policy.stagnation({
            same = 2,
            signature = function(beat)
                for _, ev in ipairs(beat.events) do
                    if ev.kind == "llm_response" then
                        return tostring((ev.data.content[1] or {}).text)
                    end
                end
                return nil
            end,
        })
        expect(by_text(session)).to.be("repeated")
    end)

    it("is loud about a signature that answers neither a string nor nil", function()
        local session = run(repeating_tool(), 2)
        local broken = policy.stagnation({
            same = 2,
            signature = function()
                return 42
            end,
        })
        expect(function()
            broken(session)
        end).to.fail()
    end)
end)

describe("policy.stagnation — no_progress", function()
    it("fires when the last `no_progress` beats wrote nothing", function()
        local session = run(empty_answers(), 2)
        expect(policy.stagnation({ no_progress = 2 })(session)).to.be("no_progress")
        expect(policy.stagnation({ no_progress = 3 })(session)).to.be(nil)
    end)

    it("counts whitespace as nothing", function()
        local session = run(function(_s)
            return kernel.device({ llm = support.always(support.text("   \n  ")) })
        end, 2)
        expect(policy.stagnation({ no_progress = 2 })(session)).to.be("no_progress")
    end)

    it("does not fire while the model is still answering", function()
        local session = run(function(_s)
            return kernel.device({ llm = support.always(support.text("here is the answer")) })
        end, 3)
        expect(policy.stagnation({ no_progress = 2 })(session)).to.be(nil)
    end)

    it("counts a beat whose call did not come off as no progress", function()
        local session = run(function(_s)
            return kernel.device({ llm = support.fails("network down") })
        end, 2)
        expect(policy.stagnation({ no_progress = 2 })(session)).to.be("no_progress")
    end)
end)

describe("policy.stagnation — the two reasons are independent", function()
    it("a repeating tool call is never no_progress: it made progress", function()
        local session = run(repeating_tool(), 3)
        expect(policy.stagnation({ same = 99, no_progress = 2 })(session)).to.be(nil)
        expect(policy.stagnation({ same = 2, no_progress = 2 })(session)).to.be("repeated")
    end)

    it("an empty run is never repeated: it has no signature to repeat", function()
        local session = run(empty_answers(), 3)
        expect(policy.stagnation({ same = 2, no_progress = 99 })(session)).to.be(nil)
        expect(policy.stagnation({ same = 2, no_progress = 2 })(session)).to.be("no_progress")
    end)
end)

describe("policy.stagnation — no counters are held", function()
    it("answers the same thing however often it is asked", function()
        local session = run(repeating_tool(), 3)
        local stalled = policy.stagnation({ same = 3 })
        expect(stalled(session)).to.be("repeated")
        expect(stalled(session)).to.be("repeated")
        expect(stalled(session)).to.be("repeated")
    end)

    it("a predicate built after the fact reaches the same verdict", function()
        -- The whole judgement is in the log, which is what lets a resumed
        -- session be judged on a history this process did not watch happen.
        local session = run(repeating_tool(), 3)
        expect(policy.stagnation({ same = 3 })(session)).to.be("repeated")
        expect(policy.stagnation({ same = 3 })(session)).to.be("repeated")
    end)

    it("one predicate judges two sessions apart", function()
        local stalled = policy.stagnation({ same = 2, no_progress = 2 })
        local looping = run(repeating_tool(), 2)
        local working = run(function(_s)
            return kernel.device({ llm = support.always(support.text("progress")) })
        end, 2)
        expect(stalled(looping)).to.be("repeated")
        expect(stalled(working)).to.be(nil)
    end)
end)

describe("policy.stagnation — in a caller's loop", function()
    it("stops a loop that would otherwise run to the budget", function()
        local session = support.session({ budget = { amount = 50, tag = "beats" } })
        support.seed(session, "q")
        local at = 0
        local device = kernel.device({
            llm = function(_request)
                at = at + 1
                return support.calls("c" .. at, "search", { q = "same" })
            end,
            tools = support.tool("search", "no results"),
        })
        local stalled = policy.stagnation({ same = 2 })

        local beats, why = 0, nil
        while beats < 20 do
            local outcome = kernel.beat(session, device)
            beats = beats + 1
            if not Outcome.is_ok(outcome) then
                break
            end
            why = stalled(session)
            if why ~= nil then
                break
            end
        end

        expect(why).to.be("repeated")
        expect(beats).to.be(2)
        expect(session:remaining()).to.be(48)
    end)
end)
