-- task_phase4.lua — regression for C1/C2/H2 fixes.
--
-- Covers:
--   1. with_timeout bounds wall time even when a child never checkpoints
--      (abort_all on timeout).
--   2. task.spawn inside a spawned child attaches to the enclosing scope —
--      not whichever sibling happens to have the most recently pushed
--      scope on the (old) VM-wide stack.
--   3. nested task.scope inside a task.spawn child works, with inner
--      children collected by the inner scope, not the outer one.
--   4. unknown opts keys rejected (M3).

-- 1. Non-cooperative child + with_timeout must return within a bounded
--    window via abort_all.  Without the abort_all fix, drain_scope would
--    await the 1s sleep.
local t0 = std.time.millis()
local ok1, err1 = pcall(function()
    std.task.with_timeout(20, function(scope)
        scope:spawn(function()
            std.task.sleep(1000) -- no checkpoint; cooperative cancel can't bail
        end)
        std.task.sleep(1000)
    end)
end)
local elapsed1 = std.time.millis() - t0
print("timeout_abort_raises=" .. tostring((not ok1) and tostring(err1):find("exceeded") ~= nil))
print("timeout_abort_bounded=" .. tostring(elapsed1 < 300))

-- 2. Concurrent scopes + task.spawn inside a child.  Task A is spawned into
--    scope_outer.  Task A awaits, then calls task.spawn which must attach
--    to scope_outer (A's enclosing scope via task_local), not scope_inner
--    which Task B concurrently entered.
--
--    Correctness signal: a_grandchild_ran must be true (the grandchild
--    actually executed and we joined on it).  On the old implementation
--    with a VM-wide stack this was racy because current_scope() would
--    return whichever scope happened to be on top when A resumed.
local a_grandchild_ran = false
local b_inner_child_ran = false

std.task.scope("outer", function(scope_outer)
    scope_outer:spawn(function()
        -- Task A: sleeps to interleave with Task B.
        std.task.sleep(10)
        std.task.yield()
        -- Task B may have entered its own scope by now.  task.spawn here
        -- must still see `scope_outer` as the current scope.
        local gh = std.task.spawn(function()
            a_grandchild_ran = true
            return "a_done"
        end)
        gh:join()
    end)

    scope_outer:spawn(function()
        -- Task B: runs its own nested scope concurrently.
        std.task.scope("inner_b", function(scope_inner)
            scope_inner:spawn(function()
                std.task.sleep(5)
                b_inner_child_ran = true
            end)
        end)
    end)
end)
print("a_grandchild_ran=" .. tostring(a_grandchild_ran))
print("b_inner_child_ran=" .. tostring(b_inner_child_ran))

-- 3. Unknown opts key is rejected (M3).
local ok3, err3 = pcall(function()
    std.task.spawn(function() return 1 end, { drivr = "coroutine" })
end)
print("unknown_opts_rejected=" .. tostring((not ok3) and tostring(err3):find("unknown opts key") ~= nil))

-- 4. sleep rejects +Infinity (H3).
local ok4, err4 = pcall(function() std.task.sleep(1/0) end)
print("sleep_rejects_inf=" .. tostring((not ok4) and tostring(err4):find("invalid duration") ~= nil))

-- 5. coroutine sleep is cancel-aware: scope:cancel() breaks a long
--    coroutine.yield(ms) promptly rather than waiting for the sleep to end.
local cr_t0 = std.time.millis()
std.task.scope(function(scope)
    scope:spawn(function()
        coroutine.yield(1000) -- 1s coroutine sleep
    end, { driver = "coroutine" })
    std.task.sleep(20)
    scope:cancel()
end)
local cr_elapsed = std.time.millis() - cr_t0
print("coro_cancel_bounded=" .. tostring(cr_elapsed < 300))

-- 6. parse_opts type / key validation.
-- 6a. opts.name = <non-string> must be rejected with an opts.name-tagged error.
local ok_nname, err_nname = pcall(function()
    std.task.spawn(function() return 1 end, { name = 123 })
end)
print("opts_name_non_string_rejected=" .. tostring((not ok_nname) and tostring(err_nname):find("opts.name") ~= nil))

-- 6b. opts.driver = <non-string> must be rejected with an opts.driver-tagged error.
local ok_ndrv, err_ndrv = pcall(function()
    std.task.spawn(function() return 1 end, { driver = 42 })
end)
print("opts_driver_non_string_rejected=" .. tostring((not ok_ndrv) and tostring(err_ndrv):find("opts.driver") ~= nil))

-- 6c. Non-string opts key (integer array key) must be rejected.
local ok_intk, err_intk = pcall(function()
    std.task.spawn(function() return 1 end, { [1] = "foo" })
end)
print("opts_non_string_key_rejected=" .. tostring((not ok_intk) and tostring(err_intk):find("opts keys must be strings") ~= nil))

-- 6d. driver = "async_fn" must be an accepted alias for the default driver.
local h_afn = std.task.spawn(function() return 7 end, { driver = "async_fn" })
print("driver_async_fn_alias_ok=" .. tostring(h_afn:join() == 7))

-- 6e. driver = "async" must also be an accepted alias.
local h_a = std.task.spawn(function() return 8 end, { driver = "async" })
print("driver_async_alias_ok=" .. tostring(h_a:join() == 8))

-- 7. with_timeout grace_ms — 3-stage (cancel → grace → abort) semantics.

-- 7a. grace_ms = 0 forces immediate abort path (no cooperative window).
--     Even with cancel-aware sleep, zero grace must still bound the
--     teardown well under default grace.
local gz_t0 = std.time.millis()
local ok_gz = pcall(function()
    std.task.with_timeout(20, function(scope)
        scope:spawn(function() std.task.sleep(1000) end)
        std.task.sleep(1000)
    end, { grace_ms = 0 })
end)
local gz_elapsed = std.time.millis() - gz_t0
print("grace_zero_raises=" .. tostring(not ok_gz))
print("grace_zero_bounded=" .. tostring(gz_elapsed < 100))

-- 7b. A cooperative child with a pcall-protected cleanup runs its cleanup
--     inside the grace window when the parent times out.  Without
--     cancel-aware sleep OR without a grace window this would not fire.
local cleanup_ran = false
pcall(function()
    std.task.with_timeout(20, function(scope)
        scope:spawn(function()
            pcall(function() std.task.sleep(1000) end) -- swallows cancel
            cleanup_ran = true -- must execute before grace expires
        end)
        std.task.sleep(1000)
    end, { grace_ms = 200 })
end)
print("cleanup_ran=" .. tostring(cleanup_ran))

-- 7c. Unknown opts key on with_timeout is rejected.
local ok_uo, err_uo = pcall(function()
    std.task.with_timeout(10, function() end, { grac = 100 })
end)
print("timeout_unknown_opts_rejected=" .. tostring((not ok_uo) and tostring(err_uo):find("unknown opts key") ~= nil))

-- 7d. grace_ms non-number is rejected.
local ok_gt, err_gt = pcall(function()
    std.task.with_timeout(10, function() end, { grace_ms = "abc" })
end)
print("grace_non_number_rejected=" .. tostring((not ok_gt) and tostring(err_gt):find("grace_ms") ~= nil))

-- 8. ms upper bound — Infinity and values beyond u64-ns range must raise.
local ok_ub, err_ub = pcall(function() std.task.sleep(1e20) end)
print("sleep_ms_out_of_range=" .. tostring((not ok_ub) and tostring(err_ub):find("out of range") ~= nil))

print("done")
