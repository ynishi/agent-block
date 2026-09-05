-- Asserts that the host installed `mlua_batteries::async_overrides`.
--
-- `std.time.sleep` is the witness. Registered by `register_all` it is a
-- blocking sleep on the VM thread, so a sibling coroutine gets no turn for its
-- whole duration; overridden it is `tokio::time::sleep`, and the sibling keeps
-- running. The two are indistinguishable from Lua in every other way — same
-- name, same argument, same return — so counting the sibling's ticks is what
-- tells them apart, and this fixture fails if `register_by_name` did not run.

local ticks = 0

-- The ticker yields on every iteration through `std.task.sleep`, which is
-- async whether or not the overrides are installed. 400 * 5ms bounds it well
-- past the sleep below; it is aborted rather than joined.
local ticker = std.task.spawn(function()
    for _ = 1, 400 do
        std.task.sleep(5)
        ticks = ticks + 1
    end
end)

local before = ticks
std.time.sleep(0.15)
local during = ticks - before

ticker:abort()

-- ~30 ticks fit in 150ms. The floor is far below that because the property is
-- "the VM kept running", not a measurement of how fast it ran.
print("[OVR] ticks_during=" .. tostring(during))
print("[OVR] overrides_active=" .. tostring(during >= 5))
print("[OVR] done")
