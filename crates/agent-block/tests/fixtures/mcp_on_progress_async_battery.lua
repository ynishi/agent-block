-- An MCP notification callback that awaits an async battery.
--
-- The `on_progress` callback below calls `std.task.sleep`, which is a
-- `create_async_function` — it yields. That only works if every frame between
-- the yield and the coroutine the Isle created is Lua. While notifications
-- were delivered through a Rust closure (`AsyncIsle::exec`), there was a
-- C-call boundary in that path and this fixture would die with
-- "attempt to yield across a C-call boundary". The dispatch now goes through
-- `coroutine_call("__mcp_dispatch_notify", ...)`, so the yield lands.
--
-- The second half of the claim — the VM keeps running while the callback is
-- parked — is measured, not asserted by construction: the main script ticks a
-- counter in its own coroutine, and the callback reads that counter on either
-- side of its sleep. A VM that stopped would report zero ticks.

local url = os.getenv("MCP_HTTP_URL")
assert(url and url ~= "", "MCP_HTTP_URL must be set")

mcp.connect_http("prog", url)
print("CONNECT_HTTP_OK")

-- Upvalues shared with the callback closure below (same chunk, and
-- `mcp.on_progress` stores the closure itself rather than its bytecode).
local ticks = 0
local callback_done = false
local during = -1

mcp.on_progress("prog", function(ev)
    assert(ev ~= nil, "ev must not be nil")
    assert(ev.type == "progress", "unexpected ev.type: " .. tostring(ev.type))
    assert(ev.server ~= nil, "envelope server must not be nil")
    assert(ev.progress ~= nil, "envelope progress must not be nil")

    local before = ticks
    -- The async battery. Reaching the line after this one is the whole point.
    std.task.sleep(120)
    during = ticks - before
    callback_done = true
    print("PROGRESS_ASYNC_OK")
end)

-- The server emits the progress notification during this call.
local r = mcp.call("prog", "emit_progress", {})
assert(r.ok == true, "emit_progress call failed: " .. tostring(r.error))
print("CALL_OK")

-- Tick for ~400ms. Each sleep is a yield point, so the callback's coroutine
-- gets to run — and vice versa.
for _ = 1, 80 do
    std.task.sleep(5)
    ticks = ticks + 1
end

assert(callback_done, "the on_progress callback never finished")
print(string.format("TICKS_DURING_CALLBACK=%d", during))
assert(
    during >= 3,
    "the VM stopped while the callback awaited: " .. tostring(during) .. " tick(s)"
)

print("FIXTURE_DONE")
