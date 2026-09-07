-- job_spec.lua — mlua-lspec unit tests for `job`, the thin job manager.
--
-- Run via:
--   test_launch(code_file=".../job/spec/job_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("job") resolves
--
-- What this proves:
--   1 a declaration is checked and normalized: durations become seconds, the
--     block name defaults to the job name, an unknown field is refused;
--   2 `tick` is a pure decision: due once when there is no history, due
--     again `every` seconds after the previous END, never while a run is
--     live (skipped as `overlapping`), capped by `max_runs`, and started on
--     request regardless of `every`;
--   3 `run` writes the start before the process and the end after it, and
--     reads the outcome off what the process answered (exit 0 / non-zero /
--     timeout / could not start);
--   4 the facts `read` answers come off the log alone — the same log gives
--     the same facts to a fresh reader — and `reconcile` closes what a
--     manager that is gone left open;
--   5 the command a run is: the block's script, its project root, its own
--     log, and a prompt that survives the shell.

local describe, it, expect = lust.describe, lust.it, lust.expect

local job = require("job")

--- A session that answers `job`'s four reads the way the kernel's SQL would,
--- computed over what was appended. Kept beside the spec on purpose: the
--- rules it encodes (a request is answered by a later start of the same
--- job; a stop is pending until its run ends; the last end per job) are the
--- statements' meaning, and a spec that pins the meaning in Lua is what
--- says the SQL is right.
local function fake_session()
    local s = { _events = {}, _seq = 0 }
    function s:append(ev)
        assert(type(ev) == "table" and type(ev.kind) == "string", "kind is required")
        self._seq = self._seq + 1
        ev.seq = self._seq
        if ev.epoch_ms == nil then
            ev.epoch_ms = self._seq * 1000
        end
        self._events[#self._events + 1] = ev
        return ev.seq
    end
    function s:events()
        return self._events
    end
    local function of_kind(kind)
        local out = {}
        for _, ev in ipairs(s._events) do
            if ev.kind == kind then
                out[#out + 1] = ev
            end
        end
        return out
    end
    local function ended_ids()
        local ids = {}
        for _, ev in ipairs(of_kind("run_ended")) do
            ids[ev.data.run_id] = true
        end
        return ids
    end
    local function run_rows(params, only_job, only_run)
        local ends = {}
        for _, ev in ipairs(of_kind("run_ended")) do
            ends[ev.data.run_id] = ev
        end
        local out = {}
        local starts = of_kind("run_started")
        for i = #starts, 1, -1 do
            local st = starts[i]
            local e = ends[st.data.run_id]
            if (only_job == nil or st.data.job == only_job) and (only_run == nil or st.data.run_id == only_run) then
                out[#out + 1] = {
                    job = st.data.job,
                    run_id = st.data.run_id,
                    started_ms = st.epoch_ms,
                    log = st.data.log,
                    ended_ms = e and e.epoch_ms or nil,
                    outcome = e and e.data.outcome or nil,
                    exit_code = e and e.data.exit_code or nil,
                    took_s = e and e.data.took_s or nil,
                    error = e and e.data.error or nil,
                    stderr = e and e.data.stderr or nil,
                }
            end
            if params and params.limit and #out >= params.limit then
                break
            end
        end
        return out
    end
    function s:query(sql, params, opts)
        self.last_opts = opts
        local rows = {}
        if sql:find(":run_id") then
            rows = run_rows(params, nil, params.run_id)
        elseif sql:find("LIMIT :limit") then
            rows = run_rows(params, sql:find(":job") and params.job or nil, nil)
        elseif sql:find("MAX%(epoch_ms%)") then
            local last = {}
            for _, ev in ipairs(of_kind("run_ended")) do
                if last[ev.data.job] == nil or ev.epoch_ms > last[ev.data.job] then
                    last[ev.data.job] = ev.epoch_ms
                end
            end
            for j, ms in pairs(last) do
                rows[#rows + 1] = { job = j, ended_ms = ms }
            end
        elseif sql:find("kind = 'run_started'") and sql:find("NOT EXISTS") and not sql:find("run_requested") then
            local ended = ended_ids()
            for _, ev in ipairs(of_kind("run_started")) do
                if not ended[ev.data.run_id] then
                    rows[#rows + 1] =
                        { job = ev.data.job, run_id = ev.data.run_id, seq = ev.seq, started_ms = ev.epoch_ms }
                end
            end
        elseif sql:find("kind = 'run_requested'") then
            for _, r in ipairs(of_kind("run_requested")) do
                local answered = false
                for _, ev in ipairs(s._events) do
                    if
                        (ev.kind == "run_started" or ev.kind == "run_skipped")
                        and ev.seq > r.seq
                        and ev.data.job == r.data.job
                    then
                        answered = true
                    end
                end
                if not answered then
                    rows[#rows + 1] = { job = r.data.job, seq = r.seq, by = r.data.by }
                end
            end
        elseif sql:find("kind = 'run_stop_requested'") then
            local ended = ended_ids()
            for _, r in ipairs(of_kind("run_stop_requested")) do
                if not ended[r.data.run_id] then
                    rows[#rows + 1] = { run_id = r.data.run_id, seq = r.seq, by = r.data.by }
                end
            end
        else
            error("fake session: unexpected statement: " .. sql)
        end
        return rows, false
    end
    return s
end

local function decl(name, extra)
    local t = { name = name, path = "/repo/blocks/" .. name .. "/init.lua", cwd = "/repo" }
    for k, v in pairs(extra or {}) do
        t[k] = v
    end
    return job.decl(t)
end

local function names(list)
    local out = {}
    for _, item in ipairs(list) do
        out[#out + 1] = item.decl and item.decl.name or item.job
    end
    return table.concat(out, ",")
end

--- An exec that answers `result` and remembers what it was asked.
local function answering(result)
    local seen = {}
    return function(cmd, opts)
        seen[#seen + 1] = { cmd = cmd, opts = opts }
        return result
    end, seen
end

describe("job.decl — a declaration, checked", function()
    it("normalizes durations and defaults the block to the name", function()
        local d = decl("drain", { every = "2m", timeout = "10m" })
        expect(d.block).to.be("drain")
        expect(d.every).to.be(120)
        expect(d.timeout).to.be(600)
    end)

    it("takes seconds as a number and defaults the timeout", function()
        local d = decl("drain", { every = 90 })
        expect(d.every).to.be(90)
        expect(d.timeout).to.be(600)
    end)

    it("refuses an unknown field, a missing path, and a bad duration", function()
        expect(function()
            job.decl({ name = "x", path = "/p", cwd = "/c", evry = "2m" })
        end).to.fail()
        expect(function()
            job.decl({ name = "x", cwd = "/c" })
        end).to.fail()
        expect(function()
            job.decl({ name = "x", path = "/p", cwd = "/c", every = "soon" })
        end).to.fail()
        expect(function()
            job.decl({ name = "x", path = "/p", cwd = "/c", every = 0 })
        end).to.fail()
    end)

    it("reads every unit", function()
        expect(job.duration("30s")).to.be(30)
        expect(job.duration("2m")).to.be(120)
        expect(job.duration("1h")).to.be(3600)
        expect(job.duration("1d")).to.be(86400)
        expect(job.duration("45")).to.be(45)
        expect(job.duration("1.5m")).to.be(90)
    end)
end)

describe("job.tick — the decision", function()
    local empty = { ended = {}, live = {}, live_count = 0, requested = {}, stops = {} }

    it("starts a job with no history at once", function()
        local plan = job.tick({ decl("a", { every = "2m" }) }, empty, 1000)
        expect(names(plan.start)).to.be("a")
        expect(plan.start[1].reason).to.be("due")
        expect(#plan.skip).to.be(0)
    end)

    it("counts `every` from the previous end", function()
        local jobs = { decl("a", { every = "2m" }) }
        local facts = { ended = { a = 1000 }, live = {}, live_count = 0, requested = {}, stops = {} }
        expect(#job.tick(jobs, facts, 1119).start).to.be(0)
        expect(names(job.tick(jobs, facts, 1120).start)).to.be("a")
    end)

    it("never starts a job that is live, and says so", function()
        local jobs = { decl("a", { every = "2m" }) }
        local facts = {
            ended = {},
            live = { a = { run_id = "a-1", seq = 1, started = 900 } },
            live_count = 1,
            requested = {},
            stops = {},
        }
        local plan = job.tick(jobs, facts, 5000)
        expect(#plan.start).to.be(0)
        expect(plan.skip[1].job).to.be("a")
        expect(plan.skip[1].reason).to.be("overlapping")
    end)

    it("caps the manager at max_runs, in declaration order", function()
        local jobs = { decl("a", { every = "1m" }), decl("b", { every = "1m" }), decl("c", { every = "1m" }) }
        local plan = job.tick(jobs, empty, 1000, { max_runs = 2 })
        expect(names(plan.start)).to.be("a,b")
        expect(plan.skip[1].job).to.be("c")
        expect(plan.skip[1].reason).to.be("max_runs")
    end)

    it("counts runs already live against the cap", function()
        local jobs = { decl("a", { every = "1m" }), decl("b", { every = "1m" }) }
        local facts = {
            ended = {},
            live = { z = { run_id = "z-1", seq = 1, started = 1 } },
            live_count = 1,
            requested = {},
            stops = {},
        }
        local plan = job.tick(jobs, facts, 1000, { max_runs = 2 })
        expect(names(plan.start)).to.be("a")
        expect(plan.skip[1].reason).to.be("max_runs")
    end)

    it("starts a requested job regardless of `every`, and says which request", function()
        local jobs = { decl("a", { every = "1h" }), decl("b") }
        local facts = {
            ended = { a = 1000 },
            live = {},
            live_count = 0,
            requested = { { job = "a", seq = 7, by = "http" }, { job = "b", seq = 9, by = "cli" } },
            stops = {},
        }
        local plan = job.tick(jobs, facts, 1001)
        expect(names(plan.start)).to.be("a,b")
        expect(plan.start[1].reason).to.be("requested")
        expect(plan.start[1].requested).to.be(7)
        expect(plan.start[2].requested).to.be(9)
    end)

    it("says which request a refused start answers", function()
        local jobs = { decl("a", { every = "1h" }) }
        local facts = {
            ended = {},
            live = { a = { run_id = "a-1", seq = 1, started = 900 } },
            live_count = 1,
            requested = { { job = "a", seq = 7, by = "http" } },
            stops = {},
        }
        local plan = job.tick(jobs, facts, 1000)
        expect(plan.skip[1].reason).to.be("overlapping")
        expect(plan.skip[1].requested).to.be(7)
        -- Due by the interval alone, nothing to answer.
        local plan2 =
            job.tick(jobs, { ended = {}, live = facts.live, live_count = 1, requested = {}, stops = {} }, 1000)
        expect(plan2.skip[1].requested).to.be(nil)
    end)

    it("leaves a job without `every` alone until asked", function()
        local plan = job.tick({ decl("manual") }, empty, 1000)
        expect(#plan.start).to.be(0)
        expect(#plan.skip).to.be(0)
    end)

    it("reads nothing but its arguments: the same inputs give the same plan", function()
        local jobs = { decl("a", { every = "2m" }), decl("b", { every = "2m" }) }
        local facts = { ended = { a = 1000 }, live = {}, live_count = 0, requested = {}, stops = {} }
        expect(names(job.tick(jobs, facts, 1130).start)).to.be(names(job.tick(jobs, facts, 1130).start))
    end)
end)

describe("job.run — a record, a process, a record", function()
    it("records the start before the process and the end after it", function()
        local s = fake_session()
        local order = {}
        local exec = function()
            order[#order + 1] = "exec:" .. #s:events()
            return { ok = true, code = 0, stdout = "", stderr = "" }
        end
        local got = job.run(s, decl("a", { every = "1m" }), {
            log = "/logs/a.db",
            exec = exec,
            now = function()
                return 100
            end,
        })
        expect(order[1]).to.be("exec:1")
        expect(#s:events()).to.be(2)
        expect(s:events()[1].kind).to.be("run_started")
        expect(s:events()[1].data.job).to.be("a")
        expect(s:events()[1].data.log).to.be("/logs/a.db")
        expect(s:events()[1].data.timeout_s).to.be(600)
        expect(s:events()[2].kind).to.be("run_ended")
        expect(s:events()[2].data.run_id).to.be(got.run_id)
        expect(got.outcome).to.be("ok")
        expect(got.exit_code).to.be(0)
    end)

    it("reads failed, timeout, and could-not-start off what the process answered", function()
        local function outcome(result)
            local s = fake_session()
            return job.run(s, decl("a"), {
                log = "/l",
                exec = answering(result),
                now = function()
                    return 0
                end,
            }),
                s
        end
        local got = outcome({ ok = true, code = 3, stdout = "", stderr = "boom" })
        expect(got.outcome).to.be("failed")
        expect(got.exit_code).to.be(3)
        got = outcome({ ok = false, error = "timeout after 600s", timed_out = true })
        expect(got.outcome).to.be("timeout")
        got = outcome({ ok = false, error = "timeout after 600s" })
        expect(got.outcome).to.be("timeout")
        got = outcome({ ok = false, error = "exec error: not found" })
        expect(got.outcome).to.be("failed")
        expect(got.error).to.be("exec error: not found")
        -- Ended by a signal: no exit code, which `sh.exec` answers as -1.
        got = outcome({ ok = true, code = -1, stdout = "", stderr = "" })
        expect(got.outcome).to.be("stopped")
        expect(got.exit_code).to.be(-1)
        local _, s = outcome({ ok = true, code = 3, stdout = "", stderr = "boom" })
        expect(s:events()[2].data.stderr).to.be("boom")
        expect(s:events()[2].data.outcome).to.be("failed")
    end)

    it("hands the process the block's root and its timeout", function()
        local exec, seen = answering({ ok = true, code = 0, stdout = "", stderr = "" })
        job.run(fake_session(), decl("a", { timeout = "30s" }), {
            log = "/l",
            exec = exec,
            now = function()
                return 0
            end,
        })
        expect(seen[1].opts.cwd).to.be("/repo")
        expect(seen[1].opts.timeout).to.be(30)
    end)

    it("carries the request it answers", function()
        local s = fake_session()
        job.run(s, decl("a"), {
            log = "/l",
            exec = answering({ ok = true, code = 0 }),
            requested = 7,
            now = function()
                return 0
            end,
        })
        expect(s:events()[1].data.requested).to.be(7)
    end)

    it("refuses to run without a log path", function()
        expect(function()
            job.run(fake_session(), decl("a"), { exec = answering({ ok = true, code = 0 }) })
        end).to.fail()
    end)
end)

describe("job.command — what a run is", function()
    it("is the script in its project root with its own log, prompt through the environment", function()
        local d = decl("a", { prompt = "it's due", context = "ctx" })
        local cmd = job.command(d, "/logs/a-1.db", { bin = "/usr/bin/agent-block" })
        expect(cmd).to.be(
            "AGENT_BLOCK_KNL_PATH='/logs/a-1.db' AGENT_BLOCK_PROMPT='it'\\''s due' AGENT_BLOCK_CONTEXT='ctx' "
                .. "'/usr/bin/agent-block' -s '/repo/blocks/a/init.lua' -p '/repo'"
        )
    end)

    it("leaves prompt and context out when the declaration has none", function()
        local cmd = job.command(decl("a"), "/l")
        expect(cmd).to.be("AGENT_BLOCK_KNL_PATH='/l' 'agent-block' -s '/repo/blocks/a/init.lua' -p '/repo'")
    end)
end)

describe("job.read — the facts, off the log", function()
    local ok_exec = answering({ ok = true, code = 0, stdout = "", stderr = "" })
    local clock = function()
        return 50
    end

    it("answers the last end per job, and nothing for a job that never ran", function()
        local s = fake_session()
        job.run(s, decl("a"), { log = "/l", exec = ok_exec, now = clock })
        job.run(s, decl("a"), { log = "/l", exec = ok_exec, now = clock })
        local facts = job.read(s)
        expect(facts.ended.a).to.be(4)
        expect(facts.ended.b).to.be(nil)
        expect(facts.live_count).to.be(0)
    end)

    it("answers a started run without an end as live", function()
        local s = fake_session()
        s:append({ kind = "run_started", data = { job = "a", run_id = "a-1" } })
        local facts = job.read(s)
        expect(facts.live.a.run_id).to.be("a-1")
        expect(facts.live_count).to.be(1)
    end)

    it("answers a request until a later start of the same job answers it", function()
        local s = fake_session()
        job.request(s, "a", "http")
        expect(#job.read(s).requested).to.be(1)
        expect(job.read(s).requested[1].by).to.be("http")
        s:append({ kind = "run_started", data = { job = "b", run_id = "b-1" } })
        expect(#job.read(s).requested).to.be(1)
        s:append({ kind = "run_started", data = { job = "a", run_id = "a-1" } })
        expect(#job.read(s).requested).to.be(0)
    end)

    it("takes a recorded skip as the answer to a request", function()
        local s = fake_session()
        job.request(s, "a", "http")
        job.skipped(s, { job = "a", reason = "overlapping", requested = 1 })
        expect(#job.read(s).requested).to.be(0)
        local last = s:events()[#s:events()]
        expect(last.kind).to.be("run_skipped")
        expect(last.data.requested).to.be(1)
    end)

    it("answers the runs newest first, each start joined to its end", function()
        local s = fake_session()
        job.run(s, decl("a"), { log = "/l1", exec = ok_exec, now = clock })
        s:append({ kind = "run_started", data = { job = "b", run_id = "b-1", log = "/l2" } })
        local rows = job.runs(s)
        expect(#rows).to.be(2)
        expect(rows[1].run_id).to.be("b-1")
        expect(rows[1].outcome).to.be(nil)
        expect(rows[2].job).to.be("a")
        expect(rows[2].outcome).to.be("ok")
        expect(rows[2].log).to.be("/l1")
        expect(#job.runs(s, { job = "a" })).to.be(1)
        expect(#job.runs(s, { limit = 1 })).to.be(1)
        expect(job.run_of(s, "b-1").job).to.be("b")
        expect(job.run_of(s, "nope")).to.be(nil)
    end)

    it("answers a stop until the run it names has ended", function()
        local s = fake_session()
        s:append({ kind = "run_started", data = { job = "a", run_id = "a-1" } })
        job.stop(s, "a-1", "cli")
        expect(job.read(s).stops[1].run_id).to.be("a-1")
        s:append({ kind = "run_ended", data = { job = "a", run_id = "a-1", outcome = "stopped" } })
        expect(#job.read(s).stops).to.be(0)
    end)

    it("reads across the set of sessions it is given, and the own stream otherwise", function()
        local s = fake_session()
        job.read(s, { sessions = { "s-1", "s-2" } })
        expect(#s.last_opts.sessions).to.be(2)
        expect(s.last_opts.sessions[2]).to.be("s-2")
        job.runs(s, { sessions = { "s-1" } })
        expect(s.last_opts.sessions[1]).to.be("s-1")
        job.run_of(s, "x", { sessions = { "s-3" } })
        expect(s.last_opts.sessions[1]).to.be("s-3")
        job.read(s)
        expect(s.last_opts.sessions).to.be(nil)
    end)

    it("gives a fresh reader the same facts", function()
        local s = fake_session()
        job.run(s, decl("a"), { log = "/l", exec = ok_exec, now = clock })
        s:append({ kind = "run_started", data = { job = "b", run_id = "b-1" } })
        local one, two = job.read(s), job.read(s)
        expect(one.ended.a).to.be(two.ended.a)
        expect(one.live.b.run_id).to.be(two.live.b.run_id)
    end)
end)

describe("job.reconcile — what a manager that is gone left open", function()
    it("closes every live run as lost and answers how many", function()
        local s = fake_session()
        s:append({ kind = "run_started", data = { job = "a", run_id = "a-1" } })
        s:append({ kind = "run_started", data = { job = "b", run_id = "b-1" } })
        s:append({ kind = "run_ended", data = { job = "b", run_id = "b-1", outcome = "ok" } })
        expect(job.reconcile(s, 10)).to.be(1)
        local last = s:events()[#s:events()]
        expect(last.kind).to.be("run_ended")
        expect(last.data.run_id).to.be("a-1")
        expect(last.data.outcome).to.be("lost")
        expect(job.read(s).live_count).to.be(0)
    end)

    it("does nothing on a clean log", function()
        local s = fake_session()
        expect(job.reconcile(s, 10)).to.be(0)
        expect(#s:events()).to.be(0)
    end)
end)

describe("job — the loop, end to end on a fake log", function()
    it("read → tick → run → read, twice, is one run per interval", function()
        local s = fake_session()
        local jobs = { decl("a", { every = "2m" }) }
        local exec, seen = answering({ ok = true, code = 0, stdout = "", stderr = "" })
        local t = 1000
        local clock = function()
            return t
        end
        local function step()
            local plan = job.tick(jobs, job.read(s), t)
            for _, item in ipairs(plan.start) do
                job.run(s, item.decl, { log = "/l", exec = exec, now = clock, requested = item.requested })
            end
            return plan
        end
        step()
        expect(#seen).to.be(1)
        -- The fake stamps ends at seq*1000 ms: a-1 ended at 2 s. 60 s later
        -- is not 120 s after the end; 130 s is.
        t = 60
        step()
        expect(#seen).to.be(1)
        t = 130
        step()
        expect(#seen).to.be(2)
        job.request(s, "a", "http")
        t = 131
        local plan = step()
        expect(#seen).to.be(3)
        expect(plan.start[1].reason).to.be("requested")
        expect(#job.read(s).requested).to.be(0)
    end)
end)
