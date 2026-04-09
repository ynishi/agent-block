-- test_mcp.lua — MCP client test
-- outline-mcpに接続してshelf（Book一覧）を取得する

mcp.connect("outline", "outline-mcp", {})

local tools_result = mcp.list_tools("outline")
if tools_result.ok then
    log.info("Available tools:")
    for _, t in ipairs(tools_result.tools) do
        log.info("  - " .. t.name .. ": " .. (t.description or ""))
    end
else
    log.error("list_tools failed: " .. (tools_result.error or "unknown"))
end

-- Try calling shelf
local result = mcp.call("outline", "shelf", {})
if result.ok then
    log.info("Shelf result:")
    -- contentは配列で、各要素にtype="text"とtextフィールドがある
    if result.content then
        for _, c in ipairs(result.content) do
            if c.text then
                log.info(c.text)
            end
        end
    end
else
    log.error("shelf failed: " .. (result.error or "unknown"))
end

mcp.disconnect("outline")
