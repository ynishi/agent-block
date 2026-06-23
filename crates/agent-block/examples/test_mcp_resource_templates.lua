-- test_mcp_resource_templates.lua
--
-- Smoke test for mcp.list_resource_templates(name).
-- Lists resource URI templates exposed by an MCP server.
--
-- Run against a real MCP server that exposes resource templates (e.g. algocline):
--   agent-block -s examples/test_mcp_resource_templates.lua

local server = "algocline"

-- Connect to the MCP server.
mcp.connect(server, "algocline-mcp", {})

-- List resource URI templates.
local result = mcp.list_resource_templates(server)

if result.ok then
    log.info("list_resource_templates succeeded")
    local templates = result.resource_templates
    log.info("template count: " .. #templates)
    for i, tpl in ipairs(templates) do
        local uri_template = tpl.uriTemplate or "(no uriTemplate)"
        local name = tpl.name or "(no name)"
        log.info("  [" .. i .. "] " .. name .. " => " .. uri_template)
    end
else
    log.warn("list_resource_templates failed: " .. (result.error or "unknown error"))
end

mcp.disconnect(server)
