-- test_algocline.lua — algocline MCP 疎通テスト
--
-- Step 1: 接続 → tool一覧 → alc_status
-- Step 2: alc_run で簡単なLua実行
--
-- Run with:
--   agent-block -s examples/test_algocline.lua

log.info("=== Step 1: algocline MCP 疎通テスト ===")

mcp.connect("algocline", "alc", {})

-- tool 一覧取得
local tools_result = mcp.list_tools("algocline")
if tools_result.ok then
    log.info("Available tools: " .. #tools_result.tools)
    for _, t in ipairs(tools_result.tools) do
        log.info("  - " .. t.name)
    end
else
    log.error("list_tools failed: " .. (tools_result.error or "unknown"))
    mcp.disconnect("algocline")
    return
end

-- alc_status 呼び出し
local status = mcp.call("algocline", "alc_status", {})
if status.ok then
    log.info("alc_status OK:")
    for _, c in ipairs(status.content or {}) do
        if c.text then
            log.info(c.text)
        end
    end
else
    log.error("alc_status failed: " .. (status.error or "unknown"))
end

log.info("")
log.info("=== Step 2: alc_run 実行テスト ===")

-- 簡単な Lua コードを alc_run で実行
local run_result = mcp.call("algocline", "alc_run", {
    code = 'return { answer = 42, message = "hello from algocline" }',
})
if run_result.ok then
    log.info("alc_run OK:")
    for _, c in ipairs(run_result.content or {}) do
        if c.text then
            log.info(c.text)
        end
    end
else
    log.error("alc_run failed: " .. (run_result.error or "unknown"))
end

mcp.disconnect("algocline")
log.info("=== Done ===")
