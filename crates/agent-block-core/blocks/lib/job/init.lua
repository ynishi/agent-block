--- job — a thin job manager: what to run, when, and the record of it.
---
--- What this is
---   A JOB is a declaration a lane leaves beside its block: run this block,
---   this often, for at most this long. A RUN is one execution of it, and a
---   run is a fact — two records on a session log, one when it starts and
---   one when it ends. This module decides which jobs are due, starts the
---   runs, and writes the facts. It holds no state of its own: the
---   declarations are files (read by whoever runs the loop), and everything
---   about a run is derived from the log on every call, so a manager that
---   restarts does not start counting again and two readers of one log
---   reach the same answer. That is the same line `policy` and `supervisor`
---   draw, for the same reason.
---
---   The manager is thin on purpose. It does four things — read the
---   declarations, decide, start a process and record it, read the record
---   back — and nothing else: no retry policy, no per-lane branching, no
---   reading of what a run produced. A run is `agent-block -s <block>` in a
---   process of its own, with its own environment (the block's project
---   `.env`) and its own session log, which is what the MCP guide asks for
---   long work and what one process per run gives for free.
---
--- The three, and what each one is for
---
---     job.read      the facts a decision needs, off the log by SQL
---     job.tick      the decision: which jobs start now, which are skipped
---     job.run       one run: a record, a process, a record
---
---   `tick` is a pure function of (declarations, facts, now) and never
---   touches a session; `read` is the only reader and `run` the only writer.
---   The loop that composes them is the caller's (`serve` in the CLI), as
---   every loop in this tree is.
---
--- The record, exactly
---
---   Every kind here is the caller's, appended with `session:append` like any
---   other, and `data` is what each says:
---
---     run_requested       { job, by }                    someone asked for a run now
---     run_stop_requested  { run_id, by }                 someone asked a run to stop
---     run_started         { job, run_id, block, cwd, log, timeout_s, requested? }
---     run_ended           { job, run_id, outcome, exit_code?, took_s, error? }
---     run_skipped         { job, reason, requested? }    a start that was asked for and refused
---
---   `outcome` is one of `ok` (exit 0) / `failed` (exit ≠ 0, or the process
---   could not be started) / `timeout` (the group was killed at `timeout`) /
---   `stopped` (ended by a signal: a stop request, or the manager leaving
---   and taking its runs with it) / `lost` (the manager that started it is
---   gone; written by `reconcile` on the next start). A request is PENDING
---   until a `run_started` or a `run_skipped` for the same job follows it in
---   the log — either is its answer; a stop is pending until the run it
---   names has ended. Reading those as "the latest fact wins" is what lets a
---   request be a record rather than a queue.
---
---   Only a refused REQUEST is recorded as `run_skipped`. A job that is due
---   by its interval while its previous run is still live is not an event:
---   the log already says the run is live, and a tick every few seconds
---   would otherwise write the same non-fact until it ended. The loop that
---   consumes a plan writes the skip when `requested` is set and nothing
---   when it is not.
---
--- When a job is due
---
---   `every` counts from the END of the previous run, not its start: a run
---   that overran its interval does not owe a second run the moment it ends,
---   and there is no queue of missed ticks to hold. A job with no run on the
---   log is due at once. A job with a live run is never due — one live run
---   per job, and a tick that finds one records `run_skipped` with
---   `overlapping` — and a manager-wide cap (`max_runs`) skips the rest with
---   `max_runs`. A pending request makes a job due regardless of `every`.
---
--- What this is not
---   Not a scheduler with a clock of its own: the caller says what time it
---   is, so a tick is testable and the log is the only memory. Not a
---   supervisor: a run is a process, not a child session, and the kernel's
---   tree is not involved. Not a budget: a run has a time limit, which is a
---   limit and not a quota (the kernel's header says why the two are kept
---   apart, and this module keeps the word out).

local M = {}

local DEFAULT_TIMEOUT_S = 600
local DEFAULT_MAX_RUNS = 4
local DEFAULT_BIN = "agent-block"
local STDERR_TAIL_BYTES = 4096

-- ============================================================
-- duration — "2m" is 120 seconds
-- ============================================================

local UNITS = { s = 1, m = 60, h = 3600, d = 86400 }

--- Seconds from a duration: a number is seconds already; a string is a
--- number and a unit (`s` / `m` / `h` / `d`), or a bare number of seconds.
---
--- @param v number|string
--- @param who string  for the message
--- @return number seconds
function M.duration(v, who)
    who = who or "job.duration"
    if type(v) == "number" then
        if v <= 0 then
            error(who .. ": a duration must be positive, got " .. tostring(v), 2)
        end
        return v
    end
    if type(v) ~= "string" then
        error(who .. ": a duration is a number of seconds or a string like '2m', got " .. tostring(v), 2)
    end
    local num, unit = v:match("^%s*(%d+%.?%d*)%s*([smhd]?)%s*$")
    if num == nil then
        error(who .. ": not a duration: '" .. v .. "' (a number and one of s/m/h/d)", 2)
    end
    local secs = tonumber(num) * UNITS[unit ~= "" and unit or "s"]
    if secs <= 0 then
        error(who .. ": a duration must be positive, got '" .. v .. "'", 2)
    end
    return secs
end

-- ============================================================
-- decl — one job, checked
-- ============================================================

--- A declaration, checked and normalized: durations become seconds, the
--- block name defaults to the job name, and anything this module does not
--- know is refused rather than ignored — a misspelt `evry` that silently
--- meant "never" is the failure a declaration can least afford.
---
---     job.decl({ name = "drain", path = "/repo/blocks/drain/init.lua",
---                cwd = "/repo", every = "2m", timeout = "10m" })
---
--- `path` is the script and `cwd` the project root a run is started in,
--- which is where `agent-block` loads `.env` from; both come from the
--- registry that found the block, not from the declaration file.
---
--- @param t table  { name, path, cwd, every?, timeout?, prompt?, context?, block? }
--- @return table decl  { name, block, path, cwd, every?, timeout, prompt?, context? }
function M.decl(t)
    if type(t) ~= "table" then
        error("job.decl: a declaration is a table", 2)
    end
    local known = {
        name = true,
        block = true,
        path = true,
        cwd = true,
        every = true,
        timeout = true,
        prompt = true,
        context = true,
    }
    for k in pairs(t) do
        if not known[k] then
            error("job.decl: unknown field '" .. tostring(k) .. "' in job '" .. tostring(t.name) .. "'", 2)
        end
    end
    for _, k in ipairs({ "name", "path", "cwd" }) do
        if type(t[k]) ~= "string" or t[k] == "" then
            error("job.decl: '" .. k .. "' must be a non-empty string (job '" .. tostring(t.name) .. "')", 2)
        end
    end
    for _, k in ipairs({ "block", "prompt", "context" }) do
        if t[k] ~= nil and type(t[k]) ~= "string" then
            error("job.decl: '" .. k .. "' must be a string (job '" .. t.name .. "')", 2)
        end
    end
    return {
        name = t.name,
        block = t.block or t.name,
        path = t.path,
        cwd = t.cwd,
        every = t.every ~= nil and M.duration(t.every, "job.decl: every") or nil,
        timeout = t.timeout ~= nil and M.duration(t.timeout, "job.decl: timeout") or DEFAULT_TIMEOUT_S,
        prompt = t.prompt,
        context = t.context,
    }
end

-- ============================================================
-- read — the facts, off the log
-- ============================================================

--- The last end per job: `{ job, ended_ms }`.
local ENDED_SQL = [[
SELECT json_extract(data, '$.job') AS job,
       MAX(epoch_ms)               AS ended_ms
  FROM events
 WHERE stream IN $sessions
   AND kind = 'run_ended'
 GROUP BY job
]]

--- The runs that started and have not ended: `{ job, run_id, seq, started_ms }`.
local LIVE_SQL = [[
SELECT json_extract(s.data, '$.job')    AS job,
       json_extract(s.data, '$.run_id') AS run_id,
       s.seq                            AS seq,
       s.epoch_ms                       AS started_ms
  FROM events AS s
 WHERE s.stream IN $sessions
   AND s.kind = 'run_started'
   AND NOT EXISTS (
         SELECT 1 FROM events AS e
          WHERE e.stream IN $sessions
            AND e.kind = 'run_ended'
            AND json_extract(e.data, '$.run_id') = json_extract(s.data, '$.run_id'))
 ORDER BY s.seq
]]

--- The requests nothing has answered yet: `{ job, seq, by }`. A request is
--- answered by any `run_started` or `run_skipped` for the same job that
--- comes after it.
local REQUESTED_SQL = [[
SELECT json_extract(r.data, '$.job') AS job,
       r.seq                         AS seq,
       json_extract(r.data, '$.by')  AS by
  FROM events AS r
 WHERE r.stream IN $sessions
   AND r.kind = 'run_requested'
   AND NOT EXISTS (
         SELECT 1 FROM events AS s
          WHERE s.stream IN $sessions
            AND s.kind IN ('run_started', 'run_skipped')
            AND s.seq > r.seq
            AND json_extract(s.data, '$.job') = json_extract(r.data, '$.job'))
 ORDER BY r.seq
]]

--- The runs, newest first, each start joined to its end if it has one:
--- `{ job, run_id, started_ms, log, ended_ms?, outcome?, exit_code?, took_s?, error? }`.
--- Two statements rather than one with an optional filter, because a
--- parameter that may be NULL is not a shape the binding promises.
local RUNS_SQL = [[
SELECT json_extract(s.data, '$.job')       AS job,
       json_extract(s.data, '$.run_id')    AS run_id,
       s.epoch_ms                          AS started_ms,
       json_extract(s.data, '$.log')       AS log,
       e.epoch_ms                          AS ended_ms,
       json_extract(e.data, '$.outcome')   AS outcome,
       json_extract(e.data, '$.exit_code') AS exit_code,
       json_extract(e.data, '$.took_s')    AS took_s,
       json_extract(e.data, '$.error')     AS error
  FROM events AS s
  LEFT JOIN events AS e
    ON e.stream IN $sessions
   AND e.kind = 'run_ended'
   AND json_extract(e.data, '$.run_id') = json_extract(s.data, '$.run_id')
 WHERE s.stream IN $sessions
   AND s.kind = 'run_started'
 ORDER BY s.seq DESC
 LIMIT :limit
]]

local RUNS_OF_JOB_SQL = [[
SELECT json_extract(s.data, '$.job')       AS job,
       json_extract(s.data, '$.run_id')    AS run_id,
       s.epoch_ms                          AS started_ms,
       json_extract(s.data, '$.log')       AS log,
       e.epoch_ms                          AS ended_ms,
       json_extract(e.data, '$.outcome')   AS outcome,
       json_extract(e.data, '$.exit_code') AS exit_code,
       json_extract(e.data, '$.took_s')    AS took_s,
       json_extract(e.data, '$.error')     AS error
  FROM events AS s
  LEFT JOIN events AS e
    ON e.stream IN $sessions
   AND e.kind = 'run_ended'
   AND json_extract(e.data, '$.run_id') = json_extract(s.data, '$.run_id')
 WHERE s.stream IN $sessions
   AND s.kind = 'run_started'
   AND json_extract(s.data, '$.job') = :job
 ORDER BY s.seq DESC
 LIMIT :limit
]]

local RUN_SQL = [[
SELECT json_extract(s.data, '$.job')       AS job,
       json_extract(s.data, '$.run_id')    AS run_id,
       s.epoch_ms                          AS started_ms,
       json_extract(s.data, '$.log')       AS log,
       e.epoch_ms                          AS ended_ms,
       json_extract(e.data, '$.outcome')   AS outcome,
       json_extract(e.data, '$.exit_code') AS exit_code,
       json_extract(e.data, '$.took_s')    AS took_s,
       json_extract(e.data, '$.error')     AS error,
       json_extract(e.data, '$.stderr')    AS stderr
  FROM events AS s
  LEFT JOIN events AS e
    ON e.stream IN $sessions
   AND e.kind = 'run_ended'
   AND json_extract(e.data, '$.run_id') = json_extract(s.data, '$.run_id')
 WHERE s.stream IN $sessions
   AND s.kind = 'run_started'
   AND json_extract(s.data, '$.run_id') = :run_id
 LIMIT 1
]]

--- The stops whose run is still live: `{ run_id, seq, by }`.
local STOPS_SQL = [[
SELECT json_extract(r.data, '$.run_id') AS run_id,
       r.seq                            AS seq,
       json_extract(r.data, '$.by')     AS by
  FROM events AS r
 WHERE r.stream IN $sessions
   AND r.kind = 'run_stop_requested'
   AND NOT EXISTS (
         SELECT 1 FROM events AS e
          WHERE e.stream IN $sessions
            AND e.kind = 'run_ended'
            AND json_extract(e.data, '$.run_id') = json_extract(r.data, '$.run_id'))
 ORDER BY r.seq
]]

--- Rows of one statement, or a raise that says the read was cut short — a
--- decision made over a page of the facts is a wrong decision that looks
--- right, which is worse than no tick.
--- The query options a read passes through: the set of streams. `$sessions`
--- in every statement above is what the kernel expands this to, and with no
--- set it is the session's own stream.
local function span(opts)
    return { sessions = opts and opts.sessions or nil }
end

local function rows_of(session, sql, who, opts)
    local rows, truncated = session:query(sql, nil, span(opts))
    if truncated then
        error(who .. ": the read hit the kernel's row cap; the log is larger than one decision can take in", 0)
    end
    return rows or {}
end

--- The facts a tick needs, read off the log.
---
--- Reading across restarts
---
--- A session the kernel closed cannot be resumed (`knl.resume` refuses it),
--- and the host closes the manager's session when the process ends. So a
--- manager that has restarted has more than one stream behind it, and the
--- record is the set of them: every read here takes `opts.sessions` — the
--- ids of all the manager's sessions, the current one included — and reads
--- across the set with the kernel's own `$sessions`, while every write goes
--- to the current session. Facts do not move between streams; the set is
--- what makes `every` count from a run the previous process ended and
--- `reconcile` find what it left live.
---
--- @param session userdata|table  the manager's current knl session
--- @param opts table|nil  { sessions? }
--- @return table facts  { ended = { [job] = seconds }, live = { [job] = { run_id, seq, started } },
---                        live_count, requested = { { job, seq, by }, ... }, stops = { { run_id, seq, by }, ... } }
function M.read(session, opts)
    local facts = { ended = {}, live = {}, live_count = 0, requested = {}, stops = {} }
    for _, r in ipairs(rows_of(session, ENDED_SQL, "job.read", opts)) do
        if r.job ~= nil and type(r.ended_ms) == "number" then
            facts.ended[r.job] = r.ended_ms / 1000
        end
    end
    for _, r in ipairs(rows_of(session, LIVE_SQL, "job.read", opts)) do
        if r.job ~= nil then
            facts.live[r.job] = { run_id = r.run_id, seq = r.seq, started = (r.started_ms or 0) / 1000 }
            facts.live_count = facts.live_count + 1
        end
    end
    for _, r in ipairs(rows_of(session, REQUESTED_SQL, "job.read", opts)) do
        facts.requested[#facts.requested + 1] = { job = r.job, seq = r.seq, by = r.by }
    end
    for _, r in ipairs(rows_of(session, STOPS_SQL, "job.read", opts)) do
        facts.stops[#facts.stops + 1] = { run_id = r.run_id, seq = r.seq, by = r.by }
    end
    return facts
end

--- The runs, newest first — every one, or one job's.
---
--- @param session userdata|table
--- @param opts table|nil  { job?, limit?, sessions? }
--- @return table rows  `{ job, run_id, started_ms, log, ended_ms?, outcome?, exit_code?, took_s?, error? }`
function M.runs(session, opts)
    opts = opts or {}
    local limit = opts.limit or 50
    if opts.job ~= nil then
        local rows = session:query(RUNS_OF_JOB_SQL, { job = opts.job, limit = limit }, span(opts))
        return rows or {}
    end
    local rows = session:query(RUNS_SQL, { limit = limit }, span(opts))
    return rows or {}
end

--- One run by id, with the tail of its stderr, or nil.
---
--- @param session userdata|table
--- @param run_id string
--- @param opts table|nil  { sessions? }
--- @return table|nil row
function M.run_of(session, run_id, opts)
    local rows = session:query(RUN_SQL, { run_id = run_id }, span(opts))
    return rows and rows[1] or nil
end

-- ============================================================
-- tick — the decision
-- ============================================================

--- Which jobs start now, and which do not, given the declarations, the
--- facts `read` answered, and the time. Pure: nothing here reads a clock or
--- a log, so a spec can hand it any history and any moment.
---
--- Order is the declarations' order, so `max_runs` favours the jobs listed
--- first; a job declared without `every` starts only on request.
---
--- @param jobs table  array of decls (`job.decl`)
--- @param facts table  from `job.read`
--- @param now number  seconds
--- @param opts table|nil  { max_runs? }
--- @return table plan  { start = { { decl, reason, requested? }, ... }, skip = { { job, reason, requested? }, ... } }
function M.tick(jobs, facts, now, opts)
    opts = opts or {}
    local max_runs = opts.max_runs or DEFAULT_MAX_RUNS
    local plan = { start = {}, skip = {} }
    local live = facts.live_count or 0

    -- The newest pending request per job; older ones are answered by the
    -- same start, which is what "a request is a record" means.
    local requested = {}
    for _, r in ipairs(facts.requested or {}) do
        requested[r.job] = r
    end

    for _, decl in ipairs(jobs) do
        local reason
        local request = requested[decl.name]
        if request ~= nil then
            reason = "requested"
        elseif decl.every ~= nil then
            local ended = facts.ended[decl.name]
            if ended == nil or now - ended >= decl.every then
                reason = "due"
            end
        end

        if reason ~= nil then
            local requested_seq = request and request.seq or nil
            if facts.live[decl.name] ~= nil then
                plan.skip[#plan.skip + 1] = { job = decl.name, reason = "overlapping", requested = requested_seq }
            elseif live >= max_runs then
                plan.skip[#plan.skip + 1] = { job = decl.name, reason = "max_runs", requested = requested_seq }
            else
                plan.start[#plan.start + 1] =
                    { decl = decl, reason = reason, requested = request and request.seq or nil }
                live = live + 1
            end
        end
    end
    return plan
end

-- ============================================================
-- run — a record, a process, a record
-- ============================================================

--- Single-quote `s` for `sh -c`.
local function sq(s)
    return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

--- The last `n` bytes of `s`, so a record stays a record and not a log.
local function tail(s, n)
    s = tostring(s or "")
    if #s <= n then
        return s
    end
    return s:sub(-n)
end

--- The command a run is: the block's script in its own process, started in
--- the block's project root (where `.env` is), writing to its own session
--- log. The prompt and context go as environment rather than arguments,
--- since the CLI reads both from `AGENT_BLOCK_PROMPT` / `AGENT_BLOCK_CONTEXT`
--- and a shell argument would have to survive the quoting twice.
---
--- @param decl table  a decl
--- @param log string  the run's session log path
--- @param opts table|nil  { bin? }
--- @return string command
function M.command(decl, log, opts)
    opts = opts or {}
    local parts = { "AGENT_BLOCK_KNL_PATH=" .. sq(log) }
    if decl.prompt ~= nil then
        parts[#parts + 1] = "AGENT_BLOCK_PROMPT=" .. sq(decl.prompt)
    end
    if decl.context ~= nil then
        parts[#parts + 1] = "AGENT_BLOCK_CONTEXT=" .. sq(decl.context)
    end
    parts[#parts + 1] = sq(opts.bin or DEFAULT_BIN)
    parts[#parts + 1] = "-s"
    parts[#parts + 1] = sq(decl.path)
    parts[#parts + 1] = "-p"
    parts[#parts + 1] = sq(decl.cwd)
    return table.concat(parts, " ")
end

--- What a `sh.exec` result means for a run.
---
--- @param result table  `{ ok, code, stdout, stderr }` or `{ ok = false, error, timed_out? }`
--- @return string outcome
--- @return number|nil exit_code
--- @return string|nil error
local function outcome_of(result)
    if type(result) ~= "table" then
        return "failed", nil, "run answered " .. tostring(result)
    end
    if result.ok then
        -- Ended by `sh.kill` — a stop request, or the manager leaving and
        -- taking its runs with it. `killed` is the fact; the exit code that
        -- leaves depends on what the command was (a signalled process has
        -- none, a nested host that forwarded the signal exits 130), so it
        -- is recorded but not read.
        if result.killed == true then
            return "stopped", result.code, nil
        end
        if result.code == 0 then
            return "ok", 0, nil
        end
        -- No exit code is a process ended by a signal from elsewhere: still
        -- a run someone stopped, not a run that said no.
        if type(result.code) == "number" and result.code < 0 then
            return "stopped", result.code, nil
        end
        return "failed", result.code, nil
    end
    local err = tostring(result.error or "")
    if result.timed_out == true or err:match("^timeout after") then
        return "timeout", nil, err
    end
    return "failed", nil, err
end

--- One run of `decl`: the start is recorded, the process runs to its end or
--- its `timeout`, the end is recorded, and the outcome is answered.
---
--- The record is written BEFORE the process starts: a manager that dies
--- between the two leaves a `run_started` that `reconcile` closes as `lost`,
--- which is the truthful reading. Written after, a run the manager did not
--- survive would never have happened.
---
--- `opts.exec` is `sh.exec` unless a caller hands in another — a spec does,
--- and so would a caller whose "process" is something else. `opts.now` is
--- the clock (`std.time.now` unless told), for the same reason.
---
--- @param session userdata|table  the manager's session
--- @param decl table  a decl
--- @param opts table  { log, exec?, now?, bin?, requested?, run_id? }
--- @return table  { run_id, outcome, exit_code?, took_s, error? }
function M.run(session, decl, opts)
    opts = opts or {}
    if type(opts.log) ~= "string" or opts.log == "" then
        error("job.run: opts.log (the run's session log path) is required", 2)
    end
    local exec = opts.exec or (rawget(_G, "sh") and sh.exec)
    if type(exec) ~= "function" then
        error("job.run: no exec: pass opts.exec, or run where `sh.exec` is registered", 2)
    end
    local now = opts.now
    if now == nil then
        local std = rawget(_G, "std")
        now = std and std.time and std.time.now or os.time
    end

    local started = now()
    local run_id = opts.run_id or string.format("%s-%d", decl.name, math.floor(started * 1000))
    session:append({
        kind = "run_started",
        data = {
            job = decl.name,
            run_id = run_id,
            block = decl.block,
            cwd = decl.cwd,
            log = opts.log,
            timeout_s = decl.timeout,
            requested = opts.requested,
        },
    })

    -- `label` is what a stop reaches the process by: `sh.kill(run_id)` ends
    -- the run's whole group, and this call answers with no exit code, which
    -- is read as `stopped` below.
    local result =
        exec(M.command(decl, opts.log, { bin = opts.bin }), { cwd = decl.cwd, timeout = decl.timeout, label = run_id })
    local took = now() - started
    local outcome, exit_code, err = outcome_of(result)

    session:append({
        kind = "run_ended",
        data = {
            job = decl.name,
            run_id = run_id,
            outcome = outcome,
            exit_code = exit_code,
            took_s = took,
            error = err,
            stderr = type(result) == "table" and tail(result.stderr, STDERR_TAIL_BYTES) or nil,
        },
    })
    return { run_id = run_id, outcome = outcome, exit_code = exit_code, took_s = took, error = err }
end

--- Close every run the log says is live as `lost`, and answer how many. For
--- the start of a manager: a run this process did not start cannot be one
--- it is waiting on, and a `run_started` with no end would otherwise keep
--- its job skipped as `overlapping` forever.
---
--- @param session userdata|table  the manager's current session
--- @param now number|nil  seconds
--- @param opts table|nil  { sessions? } — the previous manager's runs are on its stream
--- @return number closed
function M.reconcile(session, now, opts)
    now = now or os.time()
    local facts = M.read(session, opts)
    local closed = 0
    for job, live in pairs(facts.live) do
        session:append({
            kind = "run_ended",
            data = {
                job = job,
                run_id = live.run_id,
                outcome = "lost",
                took_s = now - live.started,
            },
        })
        closed = closed + 1
    end
    return closed
end

--- Record a request for a run of `job` now. The next tick starts it.
---
--- @param session userdata|table
--- @param job string
--- @param by string|nil  who asked
function M.request(session, job, by)
    if type(job) ~= "string" or job == "" then
        error("job.request: job must be a non-empty string", 2)
    end
    session:append({ kind = "run_requested", data = { job = job, by = by } })
end

--- Record that a requested start was refused, which answers the request.
---
--- @param session userdata|table
--- @param skip table  a `plan.skip` item with `requested` set
function M.skipped(session, skip)
    session:append({
        kind = "run_skipped",
        data = { job = skip.job, reason = skip.reason, requested = skip.requested },
    })
end

--- Record a request for run `run_id` to stop. The loop that holds the
--- process acts on it at its next tick.
---
--- @param session userdata|table
--- @param run_id string
--- @param by string|nil
function M.stop(session, run_id, by)
    if type(run_id) ~= "string" or run_id == "" then
        error("job.stop: run_id must be a non-empty string", 2)
    end
    session:append({ kind = "run_stop_requested", data = { run_id = run_id, by = by } })
end

return M
