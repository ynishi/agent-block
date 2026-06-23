-- test_bus.lua — EventBus demo
--
-- Registers a kind-specific handler (`custom`) and an on_any fallback,
-- then calls `bus.serve()` to park the runtime until SIGTERM / Ctrl+C.
--
-- Usage:
--   agent-block -s examples/test_bus.lua
--   # ...then in another shell: kill -TERM <pid>
--
-- There is no external event source wired in this demo (mesh relay / webhook
-- come with separate setup). The point is to exercise the `bus.on` +
-- `bus.serve` happy path end-to-end, and to verify graceful shutdown on
-- SIGTERM within `AGENT_BLOCK_TASK_GRACE_MS`.

bus.on("custom", function(ev)
    log.info("custom event", ev.id)
    return { ok = true }
end)

bus.on_any(function(ev)
    log.debug("any event", ev.kind, ev.id)
end)

log.info("serving...")
bus.serve()
log.info("shutdown")
