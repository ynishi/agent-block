-- job_test.tl — `htl test` for `job`: the pure half, `duration` / `decl` /
-- `tick`, typed against the module's own records. The specs beside it
-- (`job_spec.lua`) hold the record and the process; this holds the decision
-- a reader can check by hand.
local t = require("htl.test")
local job = require("job")

t.describe("duration", function()
    t.it("reads a number as seconds and a string as a number and a unit", function()
        t.expect(job.duration(90)):to_equal(90)
        t.expect(job.duration("2m")):to_equal(120)
        t.expect(job.duration("1.5h")):to_equal(5400)
        t.expect(job.duration("45")):to_equal(45)
    end)
    t.it("refuses what is not a positive duration, naming the caller", function()
        t.expect(function()
            job.duration(0, "job.decl: every")
        end):to_error("job.decl: every: a duration must be positive")
        t.expect(function()
            job.duration("soon")
        end):to_error("not a duration")
    end)
end)

t.describe("decl", function()
    t.it("normalizes durations, defaults the block to the name, and keeps the rest", function()
        local d = job.decl({ name = "drain", path = "/r/blocks/drain/init.lua", cwd = "/r", every = "2m", timeout = "10m" })
        t.expect(d.block):to_equal("drain")
        t.expect(d.every):to_equal(120)
        t.expect(d.timeout):to_equal(600)
        t.expect(d.prompt):to_be_nil()
    end)
    t.it("refuses an unknown field and a missing timeout by name", function()
        t.expect(function()
            job.decl({ name = "x", path = "p", cwd = "c", timeout = "1m", evry = "2m" })
        end):to_error("unknown field 'evry'")
        t.expect(function()
            job.decl({ name = "x", path = "p", cwd = "c" })
        end):to_error("'timeout' is required")
    end)
end)

t.describe("tick", function()
    local function decl(name: string, every: string): job.Decl
        return job.decl({ name = name, path = "p", cwd = "c", every = every, timeout = "1m" })
    end
    local function facts(): job.Facts
        return { ended = {}, live = {}, live_count = 0, requested = {}, stops = {} }
    end

    t.it("starts a job with no history at once, and again `every` seconds after its END", function()
        local jobs = { decl("a", "2m") }
        local plan = job.tick(jobs, facts(), 1000, { max_runs = 4 })
        t.expect(#plan.start):to_equal(1)
        local first = plan.start[1]
        t.expect(first and first.reason):to_equal("due")

        local f = facts()
        f.ended.a = 1000
        t.expect(#job.tick(jobs, f, 1100, { max_runs = 4 }).start):to_equal(0)
        t.expect(#job.tick(jobs, f, 1120, { max_runs = 4 }).start):to_equal(1)
    end)

    t.it("never starts a job with a live run, and says so only when asked", function()
        local jobs = { decl("a", "2m") }
        local f = facts()
        f.live.a = { run_id = "a-1", seq = 1, started = 900 }
        f.live_count = 1
        local plan = job.tick(jobs, f, 5000, { max_runs = 4 })
        t.expect(#plan.start):to_equal(0)
        local skipped = plan.skip[1]
        t.expect(skipped and skipped.reason):to_equal("overlapping")
        t.expect(skipped and skipped.requested):to_be_nil()

        f.requested = { { job = "a", seq = 7, by = "me" } }
        plan = job.tick(jobs, f, 5000, { max_runs = 4 })
        skipped = plan.skip[1]
        t.expect(skipped and skipped.requested):to_equal(7)
    end)

    t.it("caps the host's runs, favouring the jobs declared first", function()
        local jobs = { decl("a", "1m"), decl("b", "1m"), decl("c", "1m") }
        local plan = job.tick(jobs, facts(), 0, { max_runs = 2 })
        t.expect(#plan.start):to_equal(2)
        local second = plan.start[2]
        t.expect(second and second.decl.name):to_equal("b")
        local skipped = plan.skip[1]
        t.expect(skipped and skipped.job):to_equal("c")
        t.expect(skipped and skipped.reason):to_equal("max_runs")
    end)

    t.it("starts on request regardless of `every`, and a job with no `every` only then", function()
        local jobs = { job.decl({ name = "manual", path = "p", cwd = "c", timeout = "1m" }) }
        t.expect(#job.tick(jobs, facts(), 0, { max_runs = 1 }).start):to_equal(0)
        local f = facts()
        f.requested = { { job = "manual", seq = 3 } }
        local plan = job.tick(jobs, f, 0, { max_runs = 1 })
        local first = plan.start[1]
        t.expect(first and first.reason):to_equal("requested")
        t.expect(first and first.requested):to_equal(3)
    end)

    t.it("refuses a tick with no max_runs", function()
        t.expect(function()
            job.tick({}, facts(), 0, {} as job.TickOpts)
        end):to_error("max_runs is required")
    end)
end)
