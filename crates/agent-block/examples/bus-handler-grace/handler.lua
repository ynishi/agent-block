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
--
-- IMPORTANT — upvalue-free handler:
-- bus.on serializes the handler closure via Function::dump(true) and
-- reloads it on the handler Isle VM. Lua bytecode transfer re-binds only
-- the _ENV upvalue (globals); every other upvalue becomes nil on the new
-- VM. The busy-wait is therefore inlined inside the closure body — if we
-- referenced a file-scope `local function block_sync` it would be nil on
-- the Isle, the handler would crash on the first call, and this whole
-- grace-window test would silently false-positive.

local my_id = mesh.agent_id()
log.info("receiver agent_id=" .. my_id)

bus.on("mesh", function(ev)
    log.info("handler START id=" .. tostring(ev.id))
    -- Inline CPU-bound busy-wait. `os` is a Lua global (accessed via
    -- _ENV), so it survives bytecode transfer. No user-defined upvalues.
    local deadline = os.time() + 10
    while os.time() < deadline do
        for _ = 1, 1000000 do
        end
    end
    log.info("handler END id=" .. tostring(ev.id))
    return { ok = true }
end)

log.info("READY — bus.serve()")
bus.serve()
log.info("after bus.serve")
