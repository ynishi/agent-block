-- test_algocline_pause.lua — alc.llm() pause/continue ループ検証
--
-- Step 3a: 手動で needs_response → LLM呼び出し → alc_continue を回す
--
-- Run with:
--   agent-block -s examples/test_algocline_pause.lua

log.info("=== Step 3a: alc.llm() pause/continue 手動ループ ===")

mcp.connect("algocline", "alc", {})

-- alc.llm() を含むコードを実行 → needs_response が返るはず
local code = [[
local resp = alc.llm("What is 2 + 3? Answer with just the number.")
return { llm_said = resp }
]]

log.info("Running alc_run with alc.llm()...")
local result = mcp.call("algocline", "alc_run", { code = code })

if not result.ok then
    log.error("alc_run failed: " .. (result.error or "unknown"))
    mcp.disconnect("algocline")
    return
end

local response_text = result.content[1].text
log.info("alc_run response: " .. response_text)

local data = std.json.decode(response_text)

if data.status ~= "needs_response" then
    log.info("Status: " .. (data.status or "nil") .. " (expected needs_response)")
    mcp.disconnect("algocline")
    return
end

log.info("Got needs_response!")
log.info("  session_id: " .. data.session_id)
log.info("  query_id:   " .. (data.query_id or "(none)"))
log.info("  prompt:     " .. data.prompt)

-- Anthropic API を直接叩いて LLM 応答を取得
local api_key = std.env.get("ANTHROPIC_API_KEY")
if not api_key then
    log.error("ANTHROPIC_API_KEY not set")
    mcp.disconnect("algocline")
    return
end

log.info("Calling Anthropic API (Haiku)...")
local llm_response = http.request("https://api.anthropic.com/v1/messages", {
    method = "POST",
    headers = {
        ["content-type"] = "application/json",
        ["x-api-key"] = api_key,
        ["anthropic-version"] = "2023-06-01",
    },
    body = std.json.encode({
        model = "claude-haiku-4-5-20251001",
        max_tokens = data.max_tokens or 128,
        messages = {
            { role = "user", content = data.prompt }
        },
    }),
})

if llm_response.status ~= 200 then
    log.error("LLM API failed: status=" .. llm_response.status)
    log.error("Body: " .. (llm_response.body or ""))
    mcp.disconnect("algocline")
    return
end

local llm_data = std.json.decode(llm_response.body)
local llm_text = llm_data.content[1].text
local usage = llm_data.usage or {}
log.info("LLM responded: " .. llm_text)
log.info(string.format("  tokens: %d in / %d out",
    usage.input_tokens or 0, usage.output_tokens or 0))

-- alc_continue で再開（session_id 必須）
log.info("Calling alc_continue...")
local continue_result = mcp.call("algocline", "alc_continue", {
    session_id = data.session_id,
    query_id = data.query_id,
    response = llm_text,
    usage = {
        prompt_tokens = usage.input_tokens,
        completion_tokens = usage.output_tokens,
    },
})

if continue_result.ok then
    log.info("alc_continue result:")
    for _, c in ipairs(continue_result.content or {}) do
        if c.text then log.info(c.text) end
    end
else
    log.error("alc_continue failed: " .. (continue_result.error or "unknown"))
end

mcp.disconnect("algocline")
log.info("=== Done ===")
