-- Fixture: OpenAI provider e2e test via in-process mock server.
--
-- Reads OPENAI_BASE_URL_TEST from environment (set by the test harness),
-- calls agent.run with provider="openai" and a single extra tool "echo".
-- Prints OPENAI_MOCK_TOOL_DISPATCHED_OK on success so the Rust test can
-- verify via predicate::str::contains.

local base_url = std.env.get("OPENAI_BASE_URL_TEST")
assert(base_url, "OPENAI_BASE_URL_TEST must be set")

local agent = require("agent")
local result = agent.run({
    provider = "openai",
    base_url = base_url,
    model = "qwen-test",
    api_key = "dummy",
    prompt = "Use the echo tool to say hello",
    extra_tools = {
        {
            name = "echo",
            description = "echo input",
            input_schema = {
                type = "object",
                properties = { message = { type = "string" } },
                required = { "message" },
            },
        },
    },
    mcp_servers = {},
})

assert(result.ok == true, "agent.run failed: " .. tostring(result.error))
assert(
    result.content ~= nil and result.content ~= "",
    "content should not be empty"
)
print("OPENAI_MOCK_TOOL_DISPATCHED_OK")
print(result.content)
