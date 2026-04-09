-- hello_stream.lua — Anthropic Messages API with SSE streaming
--
-- Usage:
--   ANTHROPIC_API_KEY=sk-... agent-block -s scripts/hello_stream.lua

local api_key = std.env.get("ANTHROPIC_API_KEY")
if not api_key then
    log.error("ANTHROPIC_API_KEY not set")
    return
end

local model = std.env.get_or("ANTHROPIC_MODEL", "claude-haiku-4-5-20251001")

local body = std.json.encode({
    model = model,
    max_tokens = 256,
    stream = true,
    messages = {
        { role = "user", content = "Say hello in one sentence." },
    },
})

http.request("https://api.anthropic.com/v1/messages", {
    method = "POST",
    headers = {
        ["x-api-key"] = api_key,
        ["anthropic-version"] = "2023-06-01",
        ["content-type"] = "application/json",
    },
    body = body,
    timeout = 30,
    stream = true,
    on_data = function(data)
        local event = std.json.decode(data)
        if event.type == "content_block_delta" and event.delta and event.delta.text then
            io.write(event.delta.text)
            io.flush()
        end
    end,
})

print() -- trailing newline
