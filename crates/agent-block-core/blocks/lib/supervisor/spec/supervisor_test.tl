-- supervisor_test.tl — `htl test` for `supervisor`: the contracts a call is
-- held to before a child is opened, which need no kernel to refuse — the
-- specs beside it hold the bracket, the nursery and the fold against a
-- fake kernel.
local t = require("htl.test")
local kernel = require("knl")
local supervisor = require("supervisor")

--- A table that is not a session, wearing the type so the call is checked
--- and the refusal is the module's own.
local function not_a_session(): kernel.Session
    return { not_a = "session" } as kernel.Session
end

t.describe("child", function()
    t.it("refuses a first argument that is not a knl session, and a body that is not a function", function()
        t.expect(function()
            supervisor.child(not_a_session(), { budget = { amount = 1 } }, function() end)
        end):to_error("must be a knl session")
    end)
end)

t.describe("parallel", function()
    t.it("refuses an entry with an option it does not know, naming the entry", function()
        t.expect(function()
            supervisor.parallel(not_a_session(), {}, {})
        end):to_error("must be a knl session")
    end)
end)

t.describe("merge", function()
    t.it("refuses an empty set of sessions before reading anything", function()
        t.expect(function()
            supervisor.merge(not_a_session(), {}, {})
        end):to_error("must be a knl session")
    end)
end)

t.describe("shapes", function()
    t.it("publishes every contract as data, and the registry beside them", function()
        t.expect(supervisor.shapes.child_opts):to_not_be_nil()
        t.expect(supervisor.shapes.parallel_opts):to_not_be_nil()
        t.expect(supervisor.shapes.merge_opts):to_not_be_nil()
        t.expect(supervisor.shapes.results):to_not_be_nil()
        t.expect(supervisor.shapes.api.child.args):to_have_length(3)
        t.expect(supervisor.shapes.api.parallel.args):to_have_length(3)
        t.expect(supervisor.shapes.api.merge.args):to_have_length(3)
    end)
end)
