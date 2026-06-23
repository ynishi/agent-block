-- task_phase3.lua — E2E fixture for std.task Phase 3
-- Covers: std.task.current(), coroutine driver (opts.driver='coroutine'),
-- coroutine.yield(ms) sleep and coroutine.yield() cooperative yield.

-- 1. current() outside any spawned task is nil.
local outside = std.task.current()
print("outside_current_nil=" .. tostring(outside == nil))

-- 2. current() inside a spawned task returns {id, name, cancelled}.
local info_dump
local h = std.task.spawn(function()
    local c = std.task.current()
    return { id = c.id, name = c.name, cancelled = c.cancelled }
end, { name = "introspect" })
info_dump = h:join()
print("current_id_type=" .. type(info_dump.id))
print("current_name=" .. tostring(info_dump.name))
print("current_cancelled=" .. tostring(info_dump.cancelled))

-- 3. coroutine driver with yield(ms) sleeps; total must be >= the sleep window.
local t0 = std.time.millis()
local h_cr = std.task.spawn(function()
    coroutine.yield(30)  -- sleep 30ms
    return "coro_done"
end, { driver = "coroutine" })
local cr_val = h_cr:join()
local cr_elapsed = std.time.millis() - t0
print("coro_val=" .. tostring(cr_val))
print("coro_sleep_ok=" .. tostring(cr_elapsed >= 25))

-- 4. coroutine driver with yield() (no arg) cooperatively yields N times
--    then returns — ensures the driver handles nil yield values.
local h_y = std.task.spawn(function()
    for _ = 1, 5 do coroutine.yield() end
    return 99
end, { driver = "coroutine" })
print("coro_yield_val=" .. tostring(h_y:join()))

-- 5. Two coroutine-driven tasks run concurrently (each sleeps 30ms via yield).
local c1 = std.task.spawn(function() coroutine.yield(30); return "x" end, { driver = "coroutine" })
local c2 = std.task.spawn(function() coroutine.yield(30); return "y" end, { driver = "coroutine" })
local tc0 = std.time.millis()
local cx = c1:join()
local cy = c2:join()
local coro_concurrent_ms = std.time.millis() - tc0
print("coro_concurrent_ok=" .. tostring(cx == "x" and cy == "y" and coro_concurrent_ms < 55))

-- 6. Unknown driver string errors.
local ok_driver, err_driver = pcall(function()
    std.task.spawn(function() return 1 end, { driver = "bogus" })
end)
print("unknown_driver_rejected=" .. tostring((not ok_driver) and tostring(err_driver):find("unknown driver") ~= nil))

-- 7. current() exposes the user-supplied name in coroutine-driven tasks too.
local named = std.task.spawn(function()
    return std.task.current().name
end, { driver = "coroutine", name = "coro_named" })
print("coro_current_name=" .. tostring(named:join()))

print("done")
