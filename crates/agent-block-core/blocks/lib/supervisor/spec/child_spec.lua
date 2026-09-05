-- child_spec.lua — unit tests for `supervisor.child`, the one child opened and
-- closed around a body.
--
-- Run via:
--   test_launch(code_file=".../supervisor/spec/child_spec.lua",
--               search_paths=[".../blocks/lib"])  -- so require("supervisor") resolves
--
-- What this proves:
--   1 THE ALLOCATION IS ON BOTH LEDGERS. The parent's records a reservation and
--     the child's a grant, for the same amount, and the balances moved by
--     exactly it — read back through `knl.views.ledger`, not off the handles;
--   2 A REFUSAL IS AN ANSWER, NOT A RAISE. `nil, err` with `err.kind ==
--     "refused"`, and nothing was opened: `knl.views.tree` shows the parent
--     alone;
--   3 A BODY THAT RAISES CLOSES THE CHILD FIRST, and the raise then goes on up
--     unchanged — the bracket's discipline, reached through it rather than
--     rewritten;
--   4 the body's values pass through, all of them, nil included;
--   5 an option this call does not know is refused before anything is opened.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("supervisor.spec.support")
local kernel = require("knl")
local supervisor = require("supervisor")

--- The ledger of one session, as `{ kind, amount }` pairs in seq order.
local function ledger(session)
    local entries = {}
    for _, row in ipairs(kernel.views.ledger(session)) do
        entries[#entries + 1] = row.kind .. ":" .. tostring(row.amount)
    end
    return table.concat(entries, ",")
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("supervisor.child — the allocation", function()
    it("moves units out of the parent and onto the child, in both ledgers", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        local seen
        supervisor.child(parent, { budget = { amount = 4 } }, function(child)
            seen = child
            expect(child:remaining()).to.be(4)
            expect(parent:remaining()).to.be(6)
            return true
        end)

        -- The parent paid: the grant that opened its account, then the
        -- reservation naming what left it.
        expect(ledger(parent)).to.be("budget_granted:10,budget_reserved:4")
        -- And the child's own ledger opens with the units that arrived.
        expect(ledger(seen)).to.be("budget_granted:4")
    end)

    it("takes the parent's unit when the allocation names none", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        supervisor.child(parent, { budget = { amount = 2 } }, function(child)
            expect(kernel.views.ledger(child)[1].tag).to.be("beats")
            return true
        end)
    end)

    it("carries a tag of its own when the allocation names one", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        supervisor.child(parent, { budget = { amount = 2, tag = "steps" } }, function(child)
            expect(kernel.views.ledger(child)[1].tag).to.be("steps")
            return true
        end)
    end)

    it("gives nothing back when the child closes (an allocation is a spend)", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        supervisor.child(parent, { budget = { amount = 4 } }, function()
            return true
        end)
        expect(parent:remaining()).to.be(6)
    end)
end)

describe("supervisor.child — a refusal is an answer", function()
    it("answers nil, err when the parent's balance will not cover it", function()
        local parent = support.session({ budget = { amount = 3, tag = "beats" } })
        local ran = false
        local value, err = supervisor.child(parent, { budget = { amount = 5 } }, function()
            ran = true
            return "unreachable"
        end)

        expect(value).to.be(nil)
        expect(type(err)).to.be("table")
        expect(err.kind).to.be("refused")
        expect(ran).to.be(false)
    end)

    it("opens nothing: the tree shows the parent alone, and the balance stands", function()
        local parent = support.session({ budget = { amount = 3, tag = "beats" } })
        supervisor.child(parent, { budget = { amount = 5 } }, function()
            return "unreachable"
        end)

        local rows = kernel.views.tree(parent)
        expect(#rows).to.be(1)
        expect(rows[1].session).to.be(parent:id())
        expect(parent:remaining()).to.be(3)
    end)

    it("leaves the refusal on the parent's ledger", function()
        local parent = support.session({ budget = { amount = 3, tag = "beats" } })
        supervisor.child(parent, { budget = { amount = 5 } }, function()
            return "unreachable"
        end)
        expect(ledger(parent)).to.be("budget_granted:3,budget_refused:5")
    end)
end)

describe("supervisor.child — a body that raises", function()
    it("closes the child with reason 'error', then re-raises unchanged", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        local seen
        local ok, err = pcall(supervisor.child, parent, { budget = { amount = 4 } }, function(child)
            seen = child
            error("the body went wrong", 0)
        end)

        expect(ok).to.be(false)
        expect(tostring(err)).to.be("the body went wrong")
        expect(support.close_reason(seen)).to.be("error")
    end)

    it("closes the child on the clean path too, with the bracket's own word", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        local seen
        supervisor.child(parent, { budget = { amount = 4 } }, function(child)
            seen = child
            return true
        end)
        expect(support.close_reason(seen)).to.be("scope_exit")
    end)

    it("records the child's close on the child's own stream, before the parent's", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        local seen
        supervisor.child(parent, { budget = { amount = 4 } }, function(child)
            seen = child
            support.seed(child, "go")
            return true
        end)
        expect(support.kinds(seen)).to.be("session_opened,budget_granted,msg_user,session_closed")

        parent:close("done")
        local rows = kernel.views.tree(parent)
        expect(#rows).to.be(2)
        for _, row in ipairs(rows) do
            expect(row.closed_epoch_ms ~= nil).to.be(true)
            -- Neither close found a child still running: the bracket had
            -- already ended the one below it.
            expect(row.open_children).to.be(nil)
        end
    end)
end)

describe("supervisor.child — what the body answers", function()
    it("hands every value back, in order", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        local a, b, c = supervisor.child(parent, { budget = { amount = 1 } }, function()
            return "one", 2, { three = true }
        end)
        expect(a).to.be("one")
        expect(b).to.be(2)
        expect(c.three).to.be(true)
    end)

    it("hands back nothing when the body returns nothing", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        expect(select("#", supervisor.child(parent, { budget = { amount = 1 } }, function() end))).to.be(0)
    end)
end)

describe("supervisor.child — construction", function()
    it("refuses an option it does not know, before anything is opened", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        expect(function()
            supervisor.child(parent, { budget = { amount = 1 }, owner = "someone" }, function() end)
        end).to.fail()
        expect(function()
            supervisor.child(parent, { budget = { amount = 1, desc = "why" } }, function() end)
        end).to.fail()
        -- Nothing opened on either attempt.
        expect(#kernel.views.tree(parent)).to.be(1)
        expect(parent:remaining()).to.be(10)
    end)

    it("insists on a whole allocation of at least one unit", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        for _, bad in ipairs({ 0, -1, 1.5 }) do
            expect(function()
                supervisor.child(parent, { budget = { amount = bad } }, function() end)
            end).to.fail()
        end
        expect(function()
            supervisor.child(parent, {}, function() end)
        end).to.fail()
    end)

    it("insists on a session and a body", function()
        local parent = support.session({ budget = { amount = 10, tag = "beats" } })
        expect(function()
            supervisor.child("not a session", { budget = { amount = 1 } }, function() end)
        end).to.fail()
        expect(function()
            supervisor.child(parent, { budget = { amount = 1 } }, "not a function")
        end).to.fail()
    end)
end)
