-- verdict_spec.lua — mlua-lspec unit tests for `policy.verdict`, the check a
-- loop runs after every beat.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/verdict_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- What this proves:
--   1 the check runs whatever the beat asked for — the model cannot decline
--     it, which is the whole reason it is not a tool;
--   2 its answer is recorded, pass or fail, stamped with the beat it judges,
--     so the log says what was checked and a stagnation signature can read
--     it;
--   3 a green with nothing changed is not a pass when `changed` is given,
--     and the verdict says why;
--   4 without `run` nothing is checked and nothing ends: `{ ok = false,
--     checked = false }`, no event, no call — the honest default rather
--     than a green nobody verified;
--   5 the bounds are loud: a `run` that is not a function, one that answers
--     the wrong shape, an option the factory does not know;
--   6 with a table `timeout`, the seconds handed to `run` are read off the
--     log — the gap between the kernel's stamps on a check and the record
--     before it, times `factor`, held between `floor` and `first` — and
--     nothing about it is kept in the factory, so a fresh verdict on the
--     same log hands out the same number; `budget` is refused, because
--     that word is the kernel's;
--   7 WHICH check is read is `measure`, the caller's choice: `"longest"`
--     (the default, and never shrinks), `"last"`, `"last_ok"`, `"first"`,
--     or a function of the caller's; a number for `timeout` is handed
--     whole every time and reads nothing;
--   8 a check that did not answer (`ran = false`) says so on the verdict
--     and in the log; how long it took is not read, and only `"longest"`
--     reads the seconds it was given, as a bound it exceeded.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")

--- A session, and the events of one kind it holds.
---
--- `support.session()` and not a table that answers `append`: `policy` asks
--- `knl.is_session`, which asks the handle for the whole of
--- `knl.shapes.session`. A stand-in narrower than that is not a session, and
--- a spec built on one would be pinning a check the kernel does not make.
local function recorder()
    return support.session()
end

local function recorded(s, kind)
    local out = {}
    for _, ev in ipairs(s:events()) do
        if ev.kind == (kind or "verify") then
            out[#out + 1] = ev
        end
    end
    return out
end

describe("policy.verdict — construction", function()
    it("answers a check", function()
        expect(type(policy.verdict({ run = function() end }))).to.be("function")
        expect(type(policy.verdict())).to.be("function")
    end)

    it("refuses a run that is not a function, a bad kind, and an unknown option", function()
        expect(function()
            policy.verdict({ run = "build" })
        end).to.fail()
        expect(function()
            policy.verdict({ run = function() end, changed = 1 })
        end).to.fail()
        expect(function()
            policy.verdict({ run = function() end, kind = "" })
        end).to.fail()
        expect(function()
            policy.verdict({ runner = function() end })
        end).to.fail()
    end)
end)

describe("policy.verdict — the check", function()
    it("runs after the beat whatever the model asked for", function()
        local calls = 0
        local verdict = policy.verdict({
            run = function()
                calls = calls + 1
                return { ok = false, stderr = "boom" }
            end,
        })
        local s = recorder()
        -- A beat that called no tool at all, and one that called several:
        -- the check does not care, which is the point.
        verdict(s, { beat = "b1" })
        verdict(s, { beat = "b2", tools = { { name = "edit" } } })
        expect(calls).to.be(2)
    end)

    it("records its answer, pass or fail, stamped with the beat it judges", function()
        local s = recorder()
        local ok_verdict = policy.verdict({
            run = function()
                return { ok = true, stdout = "fine", exit_code = 0 }
            end,
        })
        ok_verdict(s, { beat = "b1" })
        local bad_verdict = policy.verdict({
            run = function()
                return { ok = false, stderr = "E0308", exit_code = 101 }
            end,
        })
        bad_verdict(s, { beat = "b2" })

        local kept = recorded(s)
        expect(#kept).to.be(2)
        expect(kept[1].kind).to.be("verify")
        expect(kept[1].meta.beat).to.be("b1")
        expect(kept[1].data.ok).to.be(true)
        expect(kept[2].meta.beat).to.be("b2")
        expect(kept[2].data.ok).to.be(false)
        expect(kept[2].data.stderr).to.be("E0308")
        expect(kept[2].data.exit_code).to.be(101)
    end)

    it("records under the kind it was given", function()
        local s = recorder()
        policy.verdict({
            kind = "acceptance",
            run = function()
                return { ok = true }
            end,
        })(s, { beat = "b1" })
        expect(#recorded(s, "acceptance")).to.be(1)
    end)

    it("answers ok on a green, and carries the run's own result", function()
        local v = policy.verdict({
            run = function()
                return { ok = true, stdout = "5 passed" }
            end,
        })(recorder(), { beat = "b1" })
        expect(v.ok).to.be(true)
        expect(v.checked).to.be(true)
        expect(v.result.stdout).to.be("5 passed")
    end)

    it("withholds a green that changed nothing, and says why", function()
        local moved = false
        local verdict = policy.verdict({
            run = function()
                return { ok = true }
            end,
            changed = function()
                return moved
            end,
        })
        local s = recorder()
        local before = verdict(s, { beat = "b1" })
        expect(before.ok).to.be(false)
        expect(before.checked).to.be(true)
        expect(before.changed).to.be(false)
        expect(before.reason).to.be("unchanged")
        -- The check still ran and is still on the record: it passed, and
        -- that is a fact whether or not it ended the run.
        expect(recorded(s)[1].data.ok).to.be(true)

        moved = true
        local after = verdict(s, { beat = "b2" })
        expect(after.ok).to.be(true)
        expect(after.changed).to.be(true)
    end)

    it("does not ask `changed` when the check itself failed", function()
        local asked = 0
        policy.verdict({
            run = function()
                return { ok = false }
            end,
            changed = function()
                asked = asked + 1
                return true
            end,
        })(recorder(), { beat = "b1" })
        expect(asked).to.be(0)
    end)

    it("refuses a run that answers the wrong shape", function()
        local s = recorder()
        expect(function()
            policy.verdict({
                run = function()
                    return "green"
                end,
            })(s, { beat = "b1" })
        end).to.fail()
        expect(function()
            policy.verdict({
                run = function()
                    return { exit_code = 0 }
                end,
            })(s, { beat = "b1" })
        end).to.fail()
        expect(#recorded(s)).to.be(0)
    end)
end)

describe("policy.verdict — without a run", function()
    it("checks nothing, records nothing, and never ends the run", function()
        local s = recorder()
        local v = policy.verdict()(s, { beat = "b1" })
        expect(v.ok).to.be(false)
        expect(v.checked).to.be(false)
        expect(#recorded(s)).to.be(0)
        expect(policy.verdict({})(s, { beat = "b1" }).checked).to.be(false)
    end)
end)

describe("policy.verdict — driving a real beat", function()
    it("is what ends the loop, and the model saying it is done is not", function()
        local device = kernel.device({ llm = support.always(support.text("all done!")) })
        -- The model answers text and asks for nothing: on its own word the
        -- run is over. The check says otherwise for two beats.
        local greens, handed = 0, {}
        local verdict = policy.verdict({
            run = function(timeout)
                greens = greens + 1
                handed[#handed + 1] = timeout
                return { ok = greens >= 3, stderr = greens < 3 and "still red" or "" }
            end,
            timeout = { first = 900, floor = 1 },
        })

        local beats, ended = 0, nil
        kernel.session({ owner = "u", budget = { amount = 10 } }, function(s)
            s:append({ kind = "msg_user", data = { content = "make it pass" } })
            while true do
                local out = kernel.beat(s, device)
                if out.status ~= "ok" then
                    ended = out.status
                    break
                end
                beats = beats + 1
                local v = verdict(s, out.out)
                if v.ok then
                    ended = "verdict"
                    break
                end
                s:append({ kind = "msg_user", data = { content = "not yet" } })
            end
            -- Every beat left a verify behind it.
            local events = s:events()
            local verifies = 0
            for _, ev in ipairs(events) do
                if ev.kind == "verify" then
                    verifies = verifies + 1
                end
            end
            expect(verifies).to.be(beats)
        end)

        expect(ended).to.be("verdict")
        expect(beats).to.be(3)
        -- A real session stamps `epoch_ms`, so the first check is handed
        -- `first` and every one after it a number read off the log: at least
        -- the floor, never more than `first`.
        expect(handed[1]).to.be(900)
        expect(type(handed[2])).to.be("number")
        expect(handed[2] >= 1 and handed[2] <= 900).to.be(true)
        expect(type(handed[3])).to.be("number")
    end)
end)

describe("policy.verdict — the seconds a check may take", function()
    -- History, written rather than waited for. The stand-in passes
    -- `epoch_ms` through untouched, so a spec can record a check that took
    -- `secs` the way the kernel would have stamped it: a record before it,
    -- and the check's own record `secs` later.
    local function checked_in(s, at_ms, secs, data)
        s:append({ kind = "msg_user", epoch_ms = at_ms, data = { content = "go" } })
        s:append({ kind = "verify", epoch_ms = at_ms + secs * 1000, data = data or { ok = false } })
        return s
    end

    -- What a fresh verdict hands `run` on `s`. Fresh on purpose: the number
    -- has to come from the log, not from a verdict that was there when the
    -- history was made.
    local function handed(s, timeout)
        local seen
        policy.verdict({
            run = function(t)
                seen = t
                return { ok = false }
            end,
            timeout = timeout,
        })(s, {})
        return seen
    end

    it("hands the first check the whole of `first`", function()
        expect(handed(recorder(), { first = 900 })).to.be(900)
    end)

    it("reads the next off what the last answered check took", function()
        local s = checked_in(recorder(), 1000, 30)
        expect(handed(s, { first = 900, factor = 3, floor = 1 })).to.be(90)
    end)

    it("reads the latest check, not the first", function()
        local s = checked_in(recorder(), 1000, 30)
        checked_in(s, 100000, 60)
        expect(handed(s, { first = 900, factor = 3, floor = 1 })).to.be(180)
    end)

    it("holds a fast check up to `floor`", function()
        local s = checked_in(recorder(), 1000, 2)
        expect(handed(s, { first = 900, factor = 3, floor = 60 })).to.be(60)
    end)

    it("holds a slow check down to `first`", function()
        local s = checked_in(recorder(), 1000, 600)
        expect(handed(s, { first = 900, factor = 3, floor = 60 })).to.be(900)
    end)

    it("keeps nothing in the factory: the same log hands out the same number", function()
        local s = checked_in(recorder(), 1000, 30)
        local timeout = { first = 900, factor = 3, floor = 1 }
        expect(handed(s, timeout)).to.be(handed(s, timeout))
    end)

    it("keeps `first` while no check has answered", function()
        -- Cut off at 900 and never answered: there is nothing to read, so the
        -- next one may not be shortened on the strength of it.
        local s = checked_in(recorder(), 1000, 900, { ok = false, ran = false })
        expect(handed(s, { first = 900, factor = 3, floor = 1 })).to.be(900)
    end)

    it("keeps `first` when the log carries no clock", function()
        -- The stand-in stamps `seq` and nothing else, so a check it recorded
        -- has no `epoch_ms` and measures nothing.
        local s = recorder()
        local timeout = { first = 900, factor = 3, floor = 1 }
        handed(s, timeout)
        expect(#recorded(s)).to.be(1)
        expect(handed(s, timeout)).to.be(900)
    end)

    it("records what it handed the check", function()
        local s = checked_in(recorder(), 1000, 30)
        handed(s, { first = 900, factor = 3, floor = 1 })
        expect(recorded(s)[2].data.timeout_s).to.be(90)
    end)

    it("reads no log, and hands run nothing, when no timeout was given", function()
        local s = recorder()
        local reads, events = 0, s.events
        s.events = function(self)
            reads = reads + 1
            return events(self)
        end
        local seen = "untouched"
        policy.verdict({
            run = function(t)
                seen = t
                return { ok = true }
            end,
        })(s, {})
        expect(seen).to.be(nil)
        expect(reads).to.be(0)
        expect(recorded(s)[1].data.timeout_s).to.be(nil)
    end)

    it("refuses a timeout without a positive `first`", function()
        expect(function()
            policy.verdict({ run = function() end, timeout = {} })
        end).to.fail()
        expect(function()
            policy.verdict({ run = function() end, timeout = { first = 0 } })
        end).to.fail()
        expect(function()
            policy.verdict({ run = function() end, timeout = { first = 10, factor = -1 } })
        end).to.fail()
    end)

    it("refuses `budget`: that word is the kernel's quota, not a check's seconds", function()
        expect(function()
            policy.verdict({ run = function() end, budget = { first = 900 } })
        end).to.fail()
    end)

    it("refuses an option in `timeout` it does not know", function()
        expect(function()
            policy.verdict({ run = function() end, timeout = { first = 10, celing = 5 } })
        end).to.fail()
    end)
end)

describe("policy.verdict — which check `measure` reads", function()
    local function checked_in(s, at_ms, secs, data)
        s:append({ kind = "msg_user", epoch_ms = at_ms, data = { content = "go" } })
        s:append({ kind = "verify", epoch_ms = at_ms + secs * 1000, data = data or { ok = false } })
        return s
    end

    local function handed(s, timeout)
        local seen
        policy.verdict({
            run = function(t)
                seen = t
                return { ok = false }
            end,
            timeout = timeout,
        })(s, {})
        return seen
    end

    -- A slow pass, then a quick failure: the shape the default exists for.
    local function slow_then_quick()
        local s = checked_in(recorder(), 1000, 60, { ok = true })
        return checked_in(s, 100000, 5, { ok = false })
    end

    it("reads the longest by default, so a quick failure cannot shrink the window", function()
        expect(handed(slow_then_quick(), { first = 900, factor = 3, floor = 1 })).to.be(180)
    end)

    it('"last" reads the latest answered check, pass or fail', function()
        expect(handed(slow_then_quick(), { first = 900, factor = 3, floor = 1, measure = "last" })).to.be(15)
    end)

    it('"last_ok" reads the latest check that passed', function()
        expect(handed(slow_then_quick(), { first = 900, factor = 3, floor = 1, measure = "last_ok" })).to.be(180)
    end)

    it('"last_ok" keeps `first` while no check has passed', function()
        local s = checked_in(recorder(), 1000, 5, { ok = false })
        expect(handed(s, { first = 900, factor = 3, floor = 1, measure = "last_ok" })).to.be(900)
    end)

    it('"first" reads the first answered check for the rest of the run', function()
        local s = checked_in(recorder(), 1000, 30)
        checked_in(s, 100000, 60)
        expect(handed(s, { first = 900, factor = 3, floor = 1, measure = "first" })).to.be(90)
    end)

    it('"longest" counts a cut-off check as the seconds it was given', function()
        -- Answered in 10, then cut off at 100: the check takes more than
        -- 100, and the next window is read off that bound, not off the 10.
        local s = checked_in(recorder(), 1000, 10, { ok = false })
        checked_in(s, 100000, 100, { ok = false, ran = false, timeout_s = 100 })
        expect(handed(s, { first = 900, factor = 3, floor = 1 })).to.be(300)
    end)

    it('"last" does not read a cut-off check', function()
        local s = checked_in(recorder(), 1000, 10, { ok = false })
        checked_in(s, 100000, 100, { ok = false, ran = false, timeout_s = 100 })
        expect(handed(s, { first = 900, factor = 3, floor = 1, measure = "last" })).to.be(30)
    end)

    it("takes the caller's own function, handed the log and the kind", function()
        local s = checked_in(recorder(), 1000, 10)
        local got_kind, got_n
        local timeout = {
            first = 900,
            factor = 3,
            floor = 1,
            measure = function(events, kind)
                got_kind, got_n = kind, #events
                return 40
            end,
        }
        expect(handed(s, timeout)).to.be(120)
        expect(got_kind).to.be("verify")
        expect(got_n).to.be(2)
    end)

    it("keeps `first` when the caller's function answers nil", function()
        local s = checked_in(recorder(), 1000, 10)
        expect(handed(s, {
            first = 900,
            measure = function()
                return nil
            end,
        })).to.be(900)
    end)

    it("hands a number whole every time, and reads no log for it", function()
        local s = checked_in(recorder(), 1000, 10)
        local reads, events = 0, s.events
        s.events = function(self)
            reads = reads + 1
            return events(self)
        end
        expect(handed(s, 300)).to.be(300)
        expect(handed(s, 300)).to.be(300)
        expect(reads).to.be(0)
    end)

    it("refuses a measure it does not know", function()
        expect(function()
            policy.verdict({ run = function() end, timeout = { first = 10, measure = "quickest" } })
        end).to.fail()
        expect(function()
            policy.verdict({ run = function() end, timeout = -1 })
        end).to.fail()
    end)
end)

describe("policy.verdict — a check that did not answer", function()
    it("says so on the verdict and in the log", function()
        local s = recorder()
        local v = policy.verdict({
            run = function()
                return { ok = false, ran = false, stderr = "timeout after 300s" }
            end,
        })
        local got = v(s, {})
        expect(got.checked).to.be(true)
        expect(got.ran).to.be(false)
        expect(recorded(s)[1].data.ran).to.be(false)
    end)

    it("counts a result with no `ran` as answered", function()
        local s = recorder()
        local v = policy.verdict({
            run = function()
                return { ok = false }
            end,
        })
        expect(v(s, {}).ran).to.be(true)
        expect(recorded(s)[1].data.ran).to.be(true)
    end)
end)
