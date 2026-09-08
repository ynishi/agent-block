-- serve.lua — the job manager's loop. Embedded in `agent-block serve`; the
-- module doc of `crates/agent-block/src/serve/mod.rs` says what the command
-- is, and `job`'s says what a job and a run are. This file only composes:
--
--   every tick     read the log → decide → start what is due, in a task each
--   every request  resume the log → answer, or record a request for the loop
--   on SIGTERM     stop the live runs, record them, leave
--
-- Two globals arrive from the CLI: `_JOBS` (the declarations found beside
-- the registered blocks) and `_SERVE` (`store`, `session_file`, `runs_dir`,
-- `tick_s`, `max_runs`, `bin`). Both are set on the handler Isle too, which
-- is what lets the HTTP handler below find the log without an upvalue —
-- `bus.on` handlers run on the handler Isle and keep none.

local knl = require("knl")
local job = require("job")

local S = _SERVE

local jobs = {}
for _, raw in ipairs(_JOBS or {}) do
    jobs[#jobs + 1] = job.decl(raw)
end

-- The manager's record is a set of sessions, one per start: the host closes
-- a session when its process ends and a closed session cannot be resumed,
-- so each start opens its own and reads across all of them (`job`'s header,
-- "Reading across restarts"). The file beside the store lists the ids, one
-- per line, newest last; the last line is the current session while a
-- manager is up, which is how the HTTP handler below finds it.
local function session_ids()
    local ok, text = pcall(std.fs.read, S.session_file)
    local ids = {}
    if ok and type(text) == "string" then
        for line in text:gmatch("[^\n]+") do
            local id = line:match("^%s*(.-)%s*$")
            if id ~= "" then
                ids[#ids + 1] = id
            end
        end
    end
    return ids
end

local s = knl.open({ owner = "serve", store = { sqlite = S.store } })
local ids = session_ids()
ids[#ids + 1] = s:id()
std.fs.write(S.session_file, table.concat(ids, "\n") .. "\n")
local span = { sessions = ids }

local closed = job.reconcile(s, std.time.now(), span)
log.info(
    string.format(
        "serve: %d job(s), %d lost run(s) closed, tick %ds, max_runs %d, log %s",
        #jobs,
        closed,
        S.tick_s,
        S.max_runs,
        S.store
    )
)

-- The runs this process holds: run_id → task handle. This is the one thing
-- not in the log, because it cannot be — a handle is this process's — and
-- it decides nothing: a run in the log with no handle here is one a
-- previous manager started, and `lost` is its truthful end.
local handles = {}

local function live_of(facts, run_id)
    for name, live in pairs(facts.live) do
        if live.run_id == run_id then
            return name, live
        end
    end
    return nil
end

local function tick()
    local now = std.time.now()
    local facts = job.read(s, span)

    -- Stops first, so a stopped job can be started again on this same tick.
    -- The process is ended by its group, under the label `job.run` gave it:
    -- the run's task is awaiting `sh.exec`, and aborting a task does not
    -- reach a command that is already running. Once the group is gone the
    -- exec answers `killed = true` and `job.run` records `stopped` itself.
    for _, st in ipairs(facts.stops) do
        local name, live = live_of(facts, st.run_id)
        local killed = sh.kill(st.run_id)
        if not killed then
            -- Nothing of that name is running here: a run a previous manager
            -- started (its end is `lost`), or one whose task never reached
            -- the exec. Either way the record is closed by hand.
            local h = handles[st.run_id]
            if h ~= nil then
                h:abort()
                handles[st.run_id] = nil
            end
            if name ~= nil then
                s:append({
                    kind = "run_ended",
                    data = {
                        job = name,
                        run_id = st.run_id,
                        outcome = h ~= nil and "stopped" or "lost",
                        took_s = now - live.started,
                    },
                })
            end
        end
    end
    if #facts.stops > 0 then
        facts = job.read(s, span)
    end

    local plan = job.tick(jobs, facts, now, { max_runs = S.max_runs })
    for _, sk in ipairs(plan.skip) do
        if sk.requested ~= nil then
            job.skipped(s, sk)
        end
    end
    for _, item in ipairs(plan.start) do
        local decl, requested = item.decl, item.requested
        local run_id = string.format("%s-%d", decl.name, math.floor(now * 1000))
        local log_path = string.format("%s/%s/%s.sqlite", S.runs_dir, decl.name, run_id)
        local result_path = string.format("%s/%s/%s.result", S.runs_dir, decl.name, run_id)
        handles[run_id] = std.task.spawn(function()
            local ok, err = pcall(job.run, s, decl, {
                log = log_path,
                result = result_path,
                run_id = run_id,
                requested = requested,
                bin = S.bin,
            })
            handles[run_id] = nil
            if not ok then
                log.error("serve: run " .. run_id .. " raised: " .. tostring(err))
            end
        end)
    end
end

-- The loop is a task so `bus.serve` can hold the main coroutine; its handle
-- is kept so leaving can end it, since a task still sleeping is a VM still
-- running and a host that does not return.
local loop = std.task.spawn(function()
    while true do
        local ok, err = pcall(tick)
        if not ok then
            log.error("serve: tick raised: " .. tostring(err))
        end
        std.task.sleep(S.tick_s * 1000)
    end
end)

-- The HTTP surface. Runs on the handler Isle: globals only, and the log is
-- resumed per request from the id the loop wrote.
bus.on("http", function(ev)
    local knl_h = require("knl")
    local job_h = require("job")
    local conf = _SERVE

    local function reply(status, body)
        return { status = status, body = body }
    end

    -- A block's answer is a JSON string; on the wire it is the value it
    -- encodes when it decodes, and the string as it was when it does not.
    local function decoded(text)
        if type(text) ~= "string" then
            return text
        end
        local ok, value = pcall(std.json.decode, text)
        if ok then
            return value
        end
        return text
    end

    -- All of the manager's sessions, for reading; the last is the current
    -- one, for writing. Read here rather than captured: a handler has no
    -- upvalues.
    local ids = {}
    for line in std.fs.read(conf.session_file):gmatch("[^\n]+") do
        local id = line:match("^%s*(.-)%s*$")
        if id ~= "" then
            ids[#ids + 1] = id
        end
    end
    -- One handle for the life of this Isle, kept in a global: a handle that
    -- is dropped records `session_closed` on its stream (the kernel's rule
    -- for a handle going away), and a stream closed that way refuses to be
    -- resumed. Resuming per request and letting the handle go was six
    -- requests away from a manager no request could reach.
    local cached = rawget(_G, "__serve_session")
    if cached == nil or cached.id ~= ids[#ids] then
        cached = { id = ids[#ids], session = knl_h.resume({ session = ids[#ids], store = { sqlite = conf.store } }) }
        rawset(_G, "__serve_session", cached)
    end
    local session = cached.session
    local span = { sessions = ids }

    local decls = {}
    for _, raw in ipairs(_JOBS or {}) do
        decls[#decls + 1] = job_h.decl(raw)
    end
    local function declared(name)
        for _, d in ipairs(decls) do
            if d.name == name then
                return d
            end
        end
        return nil
    end

    local p = ev.payload or {}
    local method, path = tostring(p.method or ""), tostring(p.path or "/")
    local query = {}
    for k, v in tostring(p.query or ""):gmatch("([^&=]+)=([^&]*)") do
        query[k] = v
    end
    local by = "http " .. tostring((ev.meta or {}).remote or "?")

    if method == "GET" and path == "/jobs" then
        local facts = job_h.read(session, span)
        local out = {}
        for _, d in ipairs(decls) do
            local live = facts.live[d.name]
            out[#out + 1] = {
                name = d.name,
                block = d.block,
                every = d.every,
                timeout = d.timeout,
                last_ended = facts.ended[d.name],
                live = live and live.run_id or nil,
            }
        end
        return reply(200, { jobs = out })
    end

    if method == "GET" and path == "/runs" then
        local rows = job_h.runs(session, { job = query.job, limit = tonumber(query.limit), sessions = ids })
        for _, row in ipairs(rows) do
            row.result = decoded(row.result)
        end
        return reply(200, { runs = rows })
    end

    local run_id = path:match("^/runs/([^/]+)$")
    if method == "GET" and run_id ~= nil then
        local row = job_h.run_of(session, run_id, span)
        if row == nil then
            return reply(404, { error = "no such run" })
        end
        row.result = decoded(row.result)
        return reply(200, { run = row })
    end

    if method == "DELETE" and run_id ~= nil then
        -- Spelled out rather than calling `live_of` above: that is an
        -- upvalue, and this handler has none.
        local facts = job_h.read(session, span)
        local live = false
        for _, l in pairs(facts.live) do
            if l.run_id == run_id then
                live = true
            end
        end
        if not live then
            return reply(404, { error = "no such live run" })
        end
        job_h.stop(session, run_id, by)
        return reply(202, { stop = run_id })
    end

    local name = path:match("^/jobs/([^/]+)/runs$")
    if method == "POST" and name ~= nil then
        if declared(name) == nil then
            return reply(404, { error = "no such job" })
        end
        job_h.request(session, name, by)
        return reply(202, { requested = name })
    end

    return reply(404, { error = "no such route" })
end)

bus.serve()

-- Leaving: what this process holds ends with it, and the log says so. The
-- groups are killed by label first, so nothing outlives the manager; a run
-- whose exec answers in time records its own `stopped`, the rest are
-- recorded here.
loop:abort()
local now = std.time.now()
for run_id, h in pairs(handles) do
    sh.kill(run_id)
    h:abort()
end
local facts = job.read(s, span)
for name, live in pairs(facts.live) do
    if handles[live.run_id] ~= nil then
        s:append({
            kind = "run_ended",
            data = { job = name, run_id = live.run_id, outcome = "stopped", took_s = now - live.started },
        })
    end
end
log.info("serve: stopped")
