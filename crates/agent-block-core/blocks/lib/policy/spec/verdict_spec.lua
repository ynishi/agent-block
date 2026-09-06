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
--     the wrong shape, an option the factory does not know.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")

--- A session that records what it was appended, without a kernel behind it.
local function recorder()
    local s = { appended = {} }
    function s:append(event)
        self.appended[#self.appended + 1] = event
        return event
    end
    return s
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

        expect(#s.appended).to.be(2)
        expect(s.appended[1].kind).to.be("verify")
        expect(s.appended[1].beat).to.be("b1")
        expect(s.appended[1].data.ok).to.be(true)
        expect(s.appended[2].beat).to.be("b2")
        expect(s.appended[2].data.ok).to.be(false)
        expect(s.appended[2].data.stderr).to.be("E0308")
        expect(s.appended[2].data.exit_code).to.be(101)
    end)

    it("records under the kind it was given", function()
        local s = recorder()
        policy.verdict({
            kind = "acceptance",
            run = function()
                return { ok = true }
            end,
        })(s, { beat = "b1" })
        expect(s.appended[1].kind).to.be("acceptance")
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
        expect(s.appended[1].data.ok).to.be(true)

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
        expect(#s.appended).to.be(0)
    end)
end)

describe("policy.verdict — without a run", function()
    it("checks nothing, records nothing, and never ends the run", function()
        local s = recorder()
        local v = policy.verdict()(s, { beat = "b1" })
        expect(v.ok).to.be(false)
        expect(v.checked).to.be(false)
        expect(#s.appended).to.be(0)
        expect(policy.verdict({})(s, { beat = "b1" }).checked).to.be(false)
    end)
end)

describe("policy.verdict — driving a real beat", function()
    it("is what ends the loop, and the model saying it is done is not", function()
        local device = kernel.device({ llm = support.always(support.text("all done!")) })
        -- The model answers text and asks for nothing: on its own word the
        -- run is over. The check says otherwise for two beats.
        local greens = 0
        local verdict = policy.verdict({
            run = function()
                greens = greens + 1
                return { ok = greens >= 3, stderr = greens < 3 and "still red" or "" }
            end,
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
    end)
end)
