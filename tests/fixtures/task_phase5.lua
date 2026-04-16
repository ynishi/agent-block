-- task_phase5.lua — Task-API-based KV/SQL cancellation integration.
--
-- Verifies that `task.with_timeout` (and `scope:cancel()`) reach in-flight
-- SQLite work via `sqlite3_interrupt`.  Before this change, `race_timeout`
-- only observed the wall-clock `tokio::time::timeout`; a `task.with_timeout`
-- wrapping a long SQL query had to wait for the per-call SQL timeout to
-- expire.  After the change, the enclosing task's `CancelToken` is raced
-- alongside the wall-clock timeout and the SQL connection is interrupted
-- as soon as either fires.

-- ─── 1. SQL cancellation via task.with_timeout ─────────────────────────
-- A recursive CTE counting up to 1e9 would take many seconds to complete;
-- we expect sqlite3_interrupt to fire from the task cancel arm and return
-- control well within the grace window.  `with_timeout` substitutes its
-- own outer error ("task.with_timeout: exceeded …") so we only check the
-- timing bounds here; error-format assertions live in scenarios 1b/2b.
std.sql.exec("CREATE TABLE IF NOT EXISTS t5 (x INTEGER)")

local t0 = std.time.millis()
local ok_sql = pcall(function()
    std.task.with_timeout(100, function(scope)
        local h = scope:spawn(function()
            return std.sql.query([[
                WITH RECURSIVE c(x) AS (
                    SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 1000000000
                )
                SELECT count(*) AS n FROM c
            ]])
        end)
        h:join()
    end)
end)
local elapsed_sql = std.time.millis() - t0
print("sql_cancel_raises=" .. tostring(not ok_sql))
print("sql_cancel_bounded=" .. tostring(elapsed_sql < 2000))

-- ─── 1b. SQL cancel via scope:cancel surfaces the child's error ───────
-- Unlike with_timeout, `scope` propagates the first child error instead
-- of substituting its own message, so `h:join()` re-raises the exact
-- string produced by `race_timeout` — the place we want to lock down.
local ok_sql_b, err_sql_b = pcall(function()
    std.task.scope(function(scope)
        local h = scope:spawn(function()
            return std.sql.query([[
                WITH RECURSIVE c(x) AS (
                    SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 1000000000
                )
                SELECT count(*) AS n FROM c
            ]])
        end)
        scope:spawn(function()
            std.task.sleep(100)
            scope:cancel()
        end)
        return h:join()
    end)
end)
local err_sql_str = tostring(err_sql_b)
print("sql_cancel_raises_b=" .. tostring(not ok_sql_b))
print("sql_cancel_err_match=" .. tostring(
    string.find(err_sql_str, "task cancelled during sql.query", 1, true) ~= nil
))
-- Regression guard: the pre-fix format "task cancelled during sql sql.*"
-- (double "sql") must never reappear.
print("sql_cancel_no_hybrid=" .. tostring(
    string.find(err_sql_str, "during sql sql", 1, true) == nil
))

-- ─── 2. KV cancellation via task.with_timeout ──────────────────────────
-- KV ops are normally sub-millisecond, so we exercise cancellation by
-- running a tight loop of `std.kv.set` calls and relying on the cancel
-- token arm in race_timeout to fire between iterations.
local t1 = std.time.millis()
local ok_kv = pcall(function()
    std.task.with_timeout(50, function(scope)
        local h = scope:spawn(function()
            for i = 1, 10000 do
                std.kv.set("phase5", "k" .. i, tostring(i))
            end
        end)
        h:join()
    end)
end)
local elapsed_kv = std.time.millis() - t1
print("kv_cancel_raises=" .. tostring(not ok_kv))
print("kv_cancel_bounded=" .. tostring(elapsed_kv < 2000))

-- ─── 2b. KV cancel via scope:cancel surfaces the child's error ────────
-- Same structural pattern as 1b — verifies that `op = "kv.set"` flows
-- through to the Lua-visible message without any "sql" prefix.
local ok_kv_b, err_kv_b = pcall(function()
    std.task.scope(function(scope)
        local h = scope:spawn(function()
            for i = 1, 100000 do
                std.kv.set("phase5b", "k" .. i, tostring(i))
            end
        end)
        scope:spawn(function()
            std.task.sleep(50)
            scope:cancel()
        end)
        return h:join()
    end)
end)
local err_kv_str = tostring(err_kv_b)
print("kv_cancel_raises_b=" .. tostring(not ok_kv_b))
print("kv_cancel_err_match=" .. tostring(
    string.find(err_kv_str, "task cancelled during kv.set", 1, true) ~= nil
))
-- Regression guard: "during sql kv.*" must never reappear.
print("kv_cancel_no_hybrid=" .. tostring(
    string.find(err_kv_str, "during sql kv", 1, true) == nil
))

-- ─── 3. Top-level calls (no task scope) still work ────────────────────
-- effective_token() returns None outside any scope → cancel arm never
-- fires → behaviour falls back to the wall-clock timeout only.
std.kv.set("phase5", "plain", "ok")
local v = std.kv.get("phase5", "plain")
print("kv_plain_ok=" .. tostring(v == "ok"))

std.sql.exec("INSERT INTO t5 (x) VALUES (?)", { 1 })
local rows = std.sql.query("SELECT x FROM t5 WHERE x = ?", { 1 })
print("sql_plain_ok=" .. tostring(rows[1].x == 1))

-- ─── 4. Concurrent fan-out: scope:spawn × {kv, sql} × N ────────────────
-- The original motivation for std.task was to let Lua issue multiple I/O
-- calls concurrently and await them together.  Verify the fan-out form
-- returns all results via handle:join() and completes faster than
-- serial issuance would imply (each op > 0, total bounded).
local t2 = std.time.millis()
local results = {}
std.task.scope(function(scope)
    local handles = {}
    for i = 1, 5 do
        handles[i] = scope:spawn(function()
            std.kv.set("phase5_fan", "k" .. i, "v" .. i)
            return std.kv.get("phase5_fan", "k" .. i)
        end)
    end
    for i = 1, 5 do
        results[i] = handles[i]:join()
    end
end)
local elapsed_fan = std.time.millis() - t2
print("fan_all_joined=" .. tostring(#results == 5))
print("fan_values_ok=" .. tostring(results[1] == "v1" and results[5] == "v5"))
print("fan_bounded=" .. tostring(elapsed_fan < 2000))

print("done")
