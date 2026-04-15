-- task_phase2.lua — E2E fixture for std.task Phase 2
-- Covers: task.scope, task.with_timeout, task.checkpoint, task.cancel_token,
-- cooperative cancellation, structured join, error propagation.

-- 1. task.scope(fn) waits for children to complete (structured join).
--    Two 30ms tasks inside the scope should both finish, and scope should
--    not return before they do.
local t0 = std.time.millis()
local order = {}
std.task.scope(function(scope)
    scope:spawn(function()
        std.task.sleep(30)
        table.insert(order, "a")
    end)
    scope:spawn(function()
        std.task.sleep(30)
        table.insert(order, "b")
    end)
end)
local elapsed = std.time.millis() - t0
print("scope_elapsed_ok=" .. tostring(elapsed >= 25 and elapsed < 70))
print("scope_children_done=" .. tostring(#order == 2))

-- 2. task.scope(name, fn) — name propagates to the ScopeHandle.
local got_name
std.task.scope("worker_group", function(scope)
    got_name = scope.name
end)
print("scope_name=" .. tostring(got_name))

-- 3. scope:cancel() + task.checkpoint() — cooperative exit.
--    A long-running task checks in, observes cancellation, and bails.
local cancelled_ok = false
std.task.scope(function(scope)
    scope:spawn(function()
        local ok, err = pcall(function()
            for _ = 1, 100 do
                std.task.sleep(5)
                std.task.checkpoint()
            end
        end)
        cancelled_ok = (not ok) and tostring(err):find("cancelled") ~= nil
    end)
    std.task.sleep(10)
    scope:cancel()
end)
print("cooperative_cancel_ok=" .. tostring(cancelled_ok))

-- 4. task.with_timeout triggers cancel + error on deadline.
local to_ok, to_err = pcall(function()
    std.task.with_timeout(20, function(_scope)
        std.task.sleep(200)
    end)
end)
print("timeout_raises=" .. tostring((not to_ok) and tostring(to_err):find("exceeded") ~= nil))

-- 5. task.with_timeout returns value on success.
local to_val = std.task.with_timeout(100, function(_scope)
    std.task.sleep(5)
    return "ok"
end)
print("timeout_success_val=" .. tostring(to_val))

-- 6. task.cancel_token standalone — :is_cancelled / :cancel / :check.
local tok = std.task.cancel_token()
print("token_initial=" .. tostring(tok:is_cancelled()))
tok:cancel()
print("token_after_cancel=" .. tostring(tok:is_cancelled()))
local check_ok, check_err = pcall(function() tok:check() end)
print("token_check_raises=" .. tostring((not check_ok) and tostring(check_err):find("cancelled") ~= nil))

-- 7. scope error propagation: when the scope body errors, siblings are
--    cancelled cooperatively.  A sibling that checkpoints must observe it.
local sibling_cancelled = false
local ok7 = pcall(function()
    std.task.scope(function(scope)
        scope:spawn(function()
            local ok, err = pcall(function()
                for _ = 1, 100 do
                    std.task.sleep(5)
                    std.task.checkpoint()
                end
            end)
            sibling_cancelled = (not ok) and tostring(err):find("cancelled") ~= nil
        end)
        std.task.sleep(10)
        error("boom")
    end)
end)
print("scope_error_propagated=" .. tostring(not ok7))
print("sibling_cancelled_ok=" .. tostring(sibling_cancelled))

-- 8. scope:spawn returns a Handle whose :join yields the function value.
local joined
std.task.scope(function(scope)
    local h = scope:spawn(function() return 7 end)
    joined = h:join()
end)
print("scope_spawn_join=" .. tostring(joined))

print("done")
