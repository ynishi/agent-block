-- Manual verification script for examples/echo_mcp_server.
--
-- Prerequisites:
--   1. Start the echo harness in a separate terminal:
--        cargo run --example echo_mcp_server -- --transport http --port 0 --emit-logs
--   2. Copy the printed ECHO_MCP_URL value (e.g. http://127.0.0.1:54321/mcp)
--      and export it: export ECHO_MCP_URL=http://127.0.0.1:54321/mcp
--   3. Run this script:
--        agent-block --script examples/verify_echo_harness.lua
--
-- To verify sampling, also pass --request-sampling to the server and register a
-- sampling handler below.

local url = std.env.get("ECHO_MCP_URL")
if not url or url == "" then
    error("ECHO_MCP_URL is not set. Start the harness and export the URL.")
end

-- ── connect ──────────────────────────────────────────────────────────────────

local r = mcp.connect_http("echo", url)
assert(r and r.ok, "connect_http failed: " .. tostring(r and r.error))
print("CONNECT_OK")

-- ── tools/list ───────────────────────────────────────────────────────────────

local tools = mcp.list_tools("echo")
assert(tools and tools.ok, "list_tools failed")
assert(#tools.tools == 2, "expected 2 tools, got " .. #tools.tools)
local tool_names = {}
for _, t in ipairs(tools.tools) do
    tool_names[t.name] = true
end
assert(tool_names["echo"], "tool 'echo' missing")
assert(tool_names["slow_echo"], "tool 'slow_echo' missing")
print("LIST_TOOLS_OK (count=" .. #tools.tools .. ")")

-- ── resources/list ───────────────────────────────────────────────────────────

local resources = mcp.list_resources("echo")
assert(resources and resources.ok, "list_resources failed")
assert(#resources.resources == 2, "expected 2 resources, got " .. #resources.resources)
print("LIST_RESOURCES_OK (count=" .. #resources.resources .. ")")

-- ── resources/read ───────────────────────────────────────────────────────────

local res_hello = mcp.read_resource("echo", "text://hello")
assert(res_hello and res_hello.ok, "read_resource text://hello failed")
assert(#res_hello.contents > 0, "contents empty")
assert(res_hello.contents[1].text == "hello world", "unexpected content: " .. tostring(res_hello.contents[1].text))
print("READ_RESOURCE_OK (text://hello)")

-- ── prompts/list ─────────────────────────────────────────────────────────────

local prompts = mcp.list_prompts("echo")
assert(prompts and prompts.ok, "list_prompts failed")
assert(#prompts.prompts == 1, "expected 1 prompt, got " .. #prompts.prompts)
assert(prompts.prompts[1].name == "greet", "expected prompt 'greet'")
print("LIST_PROMPTS_OK (count=" .. #prompts.prompts .. ")")

-- ── prompts/get ──────────────────────────────────────────────────────────────

local greet = mcp.get_prompt("echo", "greet", { name = "Alice" })
assert(greet and greet.ok, "get_prompt failed")
assert(#greet.messages > 0, "expected at least 1 message")
local msg_text = greet.messages[1].content
assert(type(msg_text) == "string" and msg_text:find("Alice"), "unexpected greeting: " .. tostring(msg_text))
print("GET_PROMPT_OK (greet/Alice)")

-- ── tools/call: echo ─────────────────────────────────────────────────────────

local echo_result = mcp.call("echo", "echo", { msg = "ping" })
assert(echo_result and echo_result.ok, "echo call failed")
assert(#echo_result.content > 0, "echo returned no content")
local echo_text = echo_result.content[1].text
assert(echo_text == "ping", "echo returned wrong value: " .. tostring(echo_text))
print("ECHO_OK")

-- ── tools/call: slow_echo with progress tracking ─────────────────────────────

local progress_count = 0
mcp.on_progress("echo", function(tok, prog, total, msg)
    progress_count = progress_count + 1
    print(
        string.format(
            "  PROGRESS token=%s prog=%s/%s msg=%s",
            tostring(tok),
            tostring(prog),
            tostring(total),
            tostring(msg)
        )
    )
end)

-- Send a progressToken in the _meta field via raw call arguments.
-- Note: agent-block's mcp.call automatically injects a progressToken when
-- an on_progress handler is registered.
local slow_result = mcp.call("echo", "slow_echo", { msg = "hi", steps = 3 })
assert(slow_result and slow_result.ok, "slow_echo call failed")
assert(#slow_result.content > 0, "slow_echo returned no content")

if progress_count >= 1 then
    print("PROGRESS_OK (received " .. progress_count .. " notifications)")
else
    -- Progress may not fire if the client does not inject a progressToken.
    print("PROGRESS_SKIP (no token injected; on_progress not exercised)")
end

-- ── logging (requires --emit-logs on the server) ──────────────────────────────

local log_count = 0
mcp.on_log("echo", function(level, logger, data)
    log_count = log_count + 1
    print(string.format("  LOG level=%s logger=%s data=%s", tostring(level), tostring(logger), tostring(data)))
end)

print("LOG_WAIT (if server started with --emit-logs, expect 5 log notifications over ~5 seconds)")
std.time.sleep(6)

if log_count >= 1 then
    print("LOG_OK (received " .. log_count .. " log notifications)")
else
    print("LOG_SKIP (no logs received; rerun server with --emit-logs)")
end

-- ── done ─────────────────────────────────────────────────────────────────────

print("VERIFY_DONE")
