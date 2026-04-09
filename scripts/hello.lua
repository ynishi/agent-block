-- hello.lua — Anthropic Messages API via http.request
--
-- Usage:
--   ANTHROPIC_API_KEY=sk-... agent-block -s scripts/hello.lua

local api_key = std.env.get("ANTHROPIC_API_KEY")
if not api_key then
    log.error("ANTHROPIC_API_KEY not set")
    return
end

local model = std.env.get_or("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")

local body = std.json.encode({
    model = model,
    max_tokens = 256,
    messages = {
        { role = "user", content = "Say hello in one sentence." },
    },
})

local resp = http.request("https://api.anthropic.com/v1/messages", {
    method = "POST",
    headers = {
        ["x-api-key"] = api_key,
        ["anthropic-version"] = "2023-06-01",
        ["content-type"] = "application/json",
    },
    body = body,
    timeout = 30,
})

if resp.status ~= 200 then
    log.error("API error " .. resp.status .. ": " .. resp.body)
    return
end

local result = std.json.decode(resp.body)
for _, block in ipairs(result.content) do
    if block.type == "text" then
        print(block.text)
    end
end
