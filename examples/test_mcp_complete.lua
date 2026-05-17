-- test_mcp_complete.lua
--
-- Smoke test for mcp.complete(server, ref, arg_name, arg_value).
-- Exercises both prompt-ref and resource-ref completion paths.
--
-- Run against a real MCP server that supports completion (e.g. outline-mcp):
--   agent-block -s examples/test_mcp_complete.lua
--
-- The .env file is auto-loaded by agent-block; manual `source .env` is not needed.

local server = "outline"

-- Connect to the MCP server.
mcp.connect(server, "outline-mcp", {})

-- ── Prompt-ref completion ───────────────────────────────────────────────────
local prompt_ref = { type = "ref/prompt", name = "greet" }
local r1 = mcp.complete(server, prompt_ref, "name", "al")

if r1.ok then
    log.info("complete (prompt-ref) succeeded")
    local values = r1.values or {}
    log.info("completion value count: " .. #values)
    for i, v in ipairs(values) do
        log.info("  [" .. i .. "] " .. tostring(v))
    end
else
    log.warn("complete (prompt-ref) failed: " .. (r1.error or "unknown error"))
end

-- ── Resource-ref completion ─────────────────────────────────────────────────
local resource_ref = { type = "ref/resource", uri = "file:///" }
local r2 = mcp.complete(server, resource_ref, "uri", "file:///")

if r2.ok then
    log.info("complete (resource-ref) succeeded")
    local values = r2.values or {}
    log.info("completion value count: " .. #values)
    for i, v in ipairs(values) do
        log.info("  [" .. i .. "] " .. tostring(v))
    end
else
    log.warn("complete (resource-ref) failed: " .. (r2.error or "unknown error"))
end

mcp.disconnect(server)
