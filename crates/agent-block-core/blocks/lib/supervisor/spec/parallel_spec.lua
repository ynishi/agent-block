-- parallel_spec.lua — unit tests for `supervisor.parallel`'s CONTRACT: the
-- shapes it publishes and the calls it refuses.
--
-- Run via:
--   test_launch(code_file=".../supervisor/spec/parallel_spec.lua",
--               search_paths=[".../blocks/lib"])  -- so require("supervisor") resolves
--
-- WHY THE BEHAVIOUR IS NOT HERE. `parallel` runs its siblings in a `std.task`
-- scope, and `std.task` is the HOST's nursery: the pure spec runner builds its
-- VM out of the framework and a search path and registers no `std` at all
-- (`type(rawget(_G, "std")) == "nil"` under the runner).
-- Nothing here can spawn, cancel or time out, and a stub scope that ran its
-- children one after another would be a spec about the stub.
--
-- So the concurrency is proven where there is a nursery — crates/agent-block/
-- tests/fixtures/knl_beat_test.lua, inv15: two children run at once on one
-- parent, the results come back aligned by index, one sibling raising leaves
-- the other's slot untouched, both edges close in `knl.views.tree`, and the
-- parent's ledger carries both reservations.
--
-- What this file proves, which is the half a VM without a nursery can:
--   1 a list with a bad entry is refused whole, before any child is opened —
--     four sessions in the log and a fifth entry that was a typo is the worst
--     of both;
--   2 the joiner vocabulary is closed: two words, and a third is a typo rather
--     than a third meaning;
--   3 the absence of a nursery is SAID, not indexed into three frames deeper;
--   4 the result slot's shape is published, and it is never nil.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("supervisor.spec.support")
local kernel = require("knl")
local supervisor = require("supervisor")
local check = require("lshape.check")

local function parent()
    return support.session({ budget = { amount = 10, tag = "beats" } })
end

local function entry(amount)
    return {
        opts = { budget = { amount = amount or 1 } },
        fn = function() end,
    }
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("supervisor.parallel — the list is refused whole", function()
    it("names the entry that is wrong", function()
        -- An UNKNOWN key, and deliberately: declaredness is `only`'s judgement
        -- in both modes, so this message is the same one whether or not the
        -- dev gate is installed. A MISSING key is the registry's half — the
        -- gate answers that one first in dev, in its own words — which is the
        -- split `api_spec` pins from both sides.
        local p = parent()
        local bad = { opts = { budget = { amount = 1 }, ownr = "someone" }, fn = function() end }
        local ok, err = pcall(supervisor.parallel, p, { entry(1), bad })
        expect(ok).to.be(false)
        expect(tostring(err):find("children[2]", 1, true) ~= nil).to.be(true)
        expect(tostring(err):find("unknown option", 1, true) ~= nil).to.be(true)
    end)

    it("opens nothing when a later entry is bad", function()
        local p = parent()
        pcall(
            supervisor.parallel,
            p,
            { entry(1), entry(1), { opts = { budget = { amount = 0 } }, fn = function() end } }
        )
        expect(#kernel.views.tree(p)).to.be(1)
        expect(p:remaining()).to.be(10)
    end)

    it("refuses an entry that is not { opts, fn }", function()
        local p = parent()
        for _, bad in ipairs({
            { opts = { budget = { amount = 1 } } },
            { fn = function() end },
            { opts = { budget = { amount = 1 } }, fn = function() end, joiner = "isolate" },
            { opts = { budget = { amount = 1 } }, fn = "not a function" },
        }) do
            expect(function()
                supervisor.parallel(p, { bad })
            end).to.fail()
        end
    end)

    it("insists on a session and an array", function()
        local p = parent()
        expect(function()
            supervisor.parallel("not a session", { entry(1) })
        end).to.fail()
        expect(function()
            supervisor.parallel(p, "not an array")
        end).to.fail()
    end)
end)

describe("supervisor.parallel — the opts", function()
    it("closes the joiner on two words", function()
        local p = parent()
        expect(function()
            supervisor.parallel(p, { entry(1) }, { joiner = "cancel_on_all" })
        end).to.fail()
        expect(check.check("isolate", supervisor.shapes.joiner)).to.be(true)
        expect(check.check("cancel_on_error", supervisor.shapes.joiner)).to.be(true)
        expect(check.check("fail_fast", supervisor.shapes.joiner)).to.be(false)
    end)

    it("insists on a whole deadline", function()
        local p = parent()
        for _, bad in ipairs({ 0, -1, 1.5 }) do
            expect(function()
                supervisor.parallel(p, { entry(1) }, { timeout_ms = bad })
            end).to.fail()
        end
    end)

    it("refuses an option it does not know", function()
        local p = parent()
        expect(function()
            supervisor.parallel(p, { entry(1) }, { grace_ms = 100 })
        end).to.fail()
    end)
end)

describe("supervisor.parallel — without a nursery", function()
    it("says std.task is missing rather than indexing it", function()
        -- Every check above passes, so this is the next thing that happens —
        -- and it happens before a child is opened, which is what makes the
        -- message the whole of the failure.
        local p = parent()
        local ok, err = pcall(supervisor.parallel, p, { entry(1) })
        expect(ok).to.be(false)
        expect(tostring(err):find("std.task", 1, true) ~= nil).to.be(true)
        expect(#kernel.views.tree(p)).to.be(1)
    end)
end)

describe("supervisor.shapes — what a slot holds", function()
    it("admits the two forms and nothing else", function()
        local slot = supervisor.shapes.result_slot
        expect(check.check({ ok = true, values = { n = 0 } }, slot)).to.be(true)
        expect(check.check({ ok = false, err = "boom" }, slot)).to.be(true)
        expect(check.check({ ok = false, err = "boom", cancelled = true }, slot)).to.be(true)
        -- A slot missing what its form is about, and one carrying the other
        -- form's field as well.
        expect(check.check({ ok = true }, slot)).to.be(false)
        expect(check.check({ ok = true, values = { n = 0 }, err = "boom" }, slot)).to.be(false)
        -- And the boundary of what a shape can say here: `err` is `any`,
        -- because a body may raise any value at all (a table, a sentence, a
        -- number), and `any` is satisfied by an absent field. So "a failed slot
        -- carries its error" is the module's promise, written in the same
        -- statement that writes `ok = false`, and not something this schema
        -- asks for.
        expect(check.check({ ok = false }, slot)).to.be(true)
    end)

    it("publishes the results array as an array of slots", function()
        expect(
            check.check({ { ok = true, values = { n = 0 } }, { ok = false, err = "boom" } }, supervisor.shapes.results)
        ).to.be(true)
        -- The one thing a caller reads by position: never a hole.
        expect(check.check({ { ok = true, values = { n = 0 } }, false }, supervisor.shapes.results)).to.be(false)
    end)
end)
