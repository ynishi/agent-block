-- bus-handler-grace: CPU-bound handler grace-window verification.
--
-- Registers a bus.on("mesh", ...) handler that blocks synchronously (NOT
-- an async yield) for 10s. When the agent-block process receives SIGTERM
-- while this handler is in-flight, the grace window (AGENT_BLOCK_TASK_GRACE_MS,
-- default 1000ms) must bound the shutdown time.
--
-- Before the handler Isle split (subtasks 1+2), the main Isle LocalSet was
-- occupied by the CPU-bound Lua loop, so the shutdown signal future could
-- not be polled and the process waited for the handler to finish (~10s).
-- After the split, the handler runs on a dedicated OS thread so the main
-- runtime stays responsive and exits within ~grace + overhead.

local my_id = mesh.agent_id()
log.info("receiver agent_id=" .. my_id)

-- Synchronous busy-wait using os.clock() to avoid coroutine yield.
-- os.clock() returns CPU time, so this spins the CPU, which is the
-- pathological case the grace window is designed to bound.
local function block_sync(seconds)
    local deadline = os.time() + seconds
    while os.time() < deadline do
        -- tight spin; intentional to simulate a misbehaving handler
        for _ = 1, 1000000 do end
    end
end

bus.on("mesh", function(ev)
    log.info("handler START id=" .. tostring(ev.id))
    block_sync(10)
    log.info("handler END id=" .. tostring(ev.id))
    return { ok = true }
end)

log.info("READY — bus.serve()")
bus.serve()
log.info("after bus.serve")
