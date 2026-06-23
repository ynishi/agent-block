-- task_phase1.lua — E2E fixture for std.task Phase 1
-- Covers: spawn → join, sleep, yield, handle introspection, abort, SQL in task.

-- 1. spawn + join returns function's value
local h1 = std.task.spawn(function()
    return 42
end)
print("h1.id_type=" .. type(h1.id))
print("h1.name_type=" .. type(h1.name))
local v1 = h1:join()
print("v1=" .. tostring(v1))

-- 2. sleep advances and yields (wall clock via std.time.millis)
local before = std.time.millis()
std.task.sleep(50)
local elapsed_ms = std.time.millis() - before
print("slept_ok=" .. tostring(elapsed_ms >= 40))

-- 3. concurrent spawns complete independently (each sleeps then returns)
local h2 = std.task.spawn(function()
    std.task.sleep(30)
    return "a"
end)
local h3 = std.task.spawn(function()
    std.task.sleep(30)
    return "b"
end)
local t0 = std.time.millis()
local v2 = h2:join()
local v3 = h3:join()
local concurrent_ms = std.time.millis() - t0
print("v2=" .. tostring(v2))
print("v3=" .. tostring(v3))
-- If truly concurrent (both ~30ms running in parallel) total < 55ms;
-- sequential would be ~60ms.  Use 50ms threshold for CI jitter.
print("concurrent_ok=" .. tostring(concurrent_ms < 55))

-- 4. name option propagates
local h4 = std.task.spawn(function() return 1 end, { name = "worker" })
print("h4.name=" .. tostring(h4.name))
h4:join()

-- 5. elapsed() returns a number in ms
local h5 = std.task.spawn(function() std.task.sleep(10); return 1 end)
h5:join()
local el = h5:elapsed()
print("h5.elapsed_type=" .. type(el))
print("h5.elapsed_positive=" .. tostring(el > 0))

-- 6. yield is callable and returns cleanly
std.task.yield()
print("yield_ok=true")

-- 7. SQL from within a task (exercises spawn_blocking inside spawn_local)
std.sql.exec("CREATE TABLE IF NOT EXISTS t_tasks (id INTEGER, v TEXT)")
local h6 = std.task.spawn(function()
    std.sql.exec("INSERT INTO t_tasks (id, v) VALUES (?, ?)", { 1, "from_task" })
    local rows = std.sql.query("SELECT v FROM t_tasks WHERE id = ?", { 1 })
    return rows[1].v
end)
local v6 = h6:join()
print("sql_from_task=" .. tostring(v6))

-- 8. abort on unjoined handle does not panic
local h7 = std.task.spawn(function()
    std.task.sleep(10000)
    return "never"
end)
h7:abort()
print("abort_ok=true")

print("done")
