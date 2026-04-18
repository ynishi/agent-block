-- bus-handler-fast: fast-path handler roundtrip verification.
--
-- Complementary to bus-handler-grace/handler.lua (which exercises the
-- pathological CPU-bound case). This handler returns immediately, so the
-- verification focuses on:
--
--   1. The mesh → bus → handler Isle → bus.dispatch → ack chain actually
--      carries the handler's return value back to the caller.
--   2. After the ack is delivered, SIGTERM brings the process down
--      promptly — not bounded by grace because no handler is in flight.
--
-- Upvalue discipline (same as bus-handler-grace):
-- bus.on serializes the handler via Function::dump(true), so any
-- file-scope `local` referenced from inside the closure becomes nil on
-- the handler Isle. Keep all handler state inside the closure body.

local my_id = mesh.agent_id()
log.info("receiver agent_id=" .. my_id)

bus.on("mesh", function(ev)
    log.info("handler START id=" .. tostring(ev.id))
    log.info("handler END id=" .. tostring(ev.id))
    -- Echo a well-known marker so verify.sh can grep the ack
    -- (agent-meshctl prints the JSON reply to stdout).
    return {
        ok = true,
        kind = ev.kind,
        id = ev.id,
        echo = ev.payload,
        marker = "bus-handler-fast-ack",
    }
end)

log.info("READY — bus.serve()")
bus.serve()
log.info("after bus.serve")
