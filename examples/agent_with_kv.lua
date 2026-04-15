-- LLM agent uses std.kv as a tool.
--
-- Flow:
--   1. Register kv_set / kv_get / kv_list as Lua tools wrapping std.kv
--   2. Ask the agent to manage a small TODO list via those tools
--   3. Print final response + tool-call log

local agent = require("agent")

-- ── Tool registration ────────────────────────────────────────────────
-- Each handler is sync and thin: forward to std.kv.
-- std.kv itself is sync so no coroutine wrangling.

tool.register("kv_set", {
    description = "Store a value under a namespace/key. Value is any JSON-serializable data.",
    input_schema = {
        type = "object",
        properties = {
            ns    = { type = "string", description = "Namespace (logical group)" },
            key   = { type = "string", description = "Key within namespace" },
            value = { description = "Value to store (string/number/bool/table)" },
        },
        required = { "ns", "key", "value" },
    },
}, function(input)
    std.kv.set(input.ns, input.key, input.value)
    return { ok = true }
end)

tool.register("kv_get", {
    description = "Fetch a value by namespace/key. Returns nil if missing.",
    input_schema = {
        type = "object",
        properties = {
            ns  = { type = "string" },
            key = { type = "string" },
        },
        required = { "ns", "key" },
    },
}, function(input)
    local v = std.kv.get(input.ns, input.key)
    return { value = v }
end)

tool.register("kv_list", {
    description = "List keys under a namespace. Optional prefix filter.",
    input_schema = {
        type = "object",
        properties = {
            ns     = { type = "string" },
            prefix = { type = "string" },
        },
        required = { "ns" },
    },
}, function(input)
    local keys = std.kv.list(input.ns, input.prefix)
    return { keys = keys }
end)

-- ── Run agent ────────────────────────────────────────────────────────

local turn_log = {}

local result = agent.run({
    prompt = [[
Please manage a small TODO list using the kv_* tools provided.

1. Add three TODOs under namespace "todos" with keys t1, t2, t3:
   - t1: "write POC report"
   - t2: "review KV design"
   - t3: "ship release notes"
2. List all keys in that namespace to confirm.
3. Retrieve t2 and report its content.
4. Return a short summary of what you did.

Use the tools; do not guess values.
]],
    system  = "You are a concise assistant that uses the provided kv_* tools to manage state. Call tools rather than narrating intent.",
    model   = "claude-haiku-4-5-20251001",
    max_tokens     = 512,
    max_iterations = 8,
    on_turn = function(info)
        local names = {}
        for _, tc in ipairs(info.tool_calls) do
            table.insert(names, tc.name)
        end
        table.insert(turn_log, string.format(
            "turn %d: tool_calls=[%s] in=%d out=%d",
            info.turn_number,
            table.concat(names, ","),
            info.usage and info.usage.input_tokens or 0,
            info.usage and info.usage.output_tokens or 0
        ))
    end,
})

-- ── Report ───────────────────────────────────────────────────────────

print("=== turn log ===")
for _, line in ipairs(turn_log) do print(line) end

print("\n=== result ===")
print("ok=" .. tostring(result.ok))
print("turns=" .. tostring(result.num_turns))
if result.usage then
    print(string.format("tokens: in=%d out=%d total=%d",
        result.usage.input_tokens, result.usage.output_tokens, result.usage.total_tokens))
end
if not result.ok then
    print("error=" .. tostring(result.error))
else
    print("\n--- final content ---")
    print(result.content)
end

print("\n=== final KV state ===")
local all = std.kv.list("todos")
table.sort(all)
for _, k in ipairs(all) do
    print(string.format("  %s = %s", k, tostring(std.kv.get("todos", k))))
end
