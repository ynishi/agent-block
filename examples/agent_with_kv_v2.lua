-- Same scenario as agent_with_kv.lua, but wired via std.kv.register_tools.
-- Demonstrates the boilerplate collapse: schemas + handlers come from stdlib.

local agent = require("agent")

-- 1 line: register get / set / list tools, scoped to the "todos" namespace.
local names = std.kv.register_tools({
    prefix  = "todo_",
    allowed = { "get", "set", "list" },
    ns_lock = "todos",
})
print("registered=" .. table.concat(names, ","))

local turn_log = {}

local result = agent.run({
    prompt = [[
Please manage a small TODO list using the todo_* tools provided.
The namespace is implicit (already bound); you only need to pass key / value / prefix.

1. Add three TODOs with keys t1, t2, t3:
   - t1: "write POC report"
   - t2: "review KV design"
   - t3: "ship release notes"
2. List all keys to confirm.
3. Retrieve t2 and report its content.
4. Return a short summary of what you did.

Use the tools; do not guess values.
]],
    system  = "You are a concise assistant that uses the provided todo_* tools to manage state.",
    model   = "claude-haiku-4-5-20251001",
    max_tokens     = 512,
    max_iterations = 8,
    on_turn = function(info)
        local ns = {}
        for _, tc in ipairs(info.tool_calls) do table.insert(ns, tc.name) end
        table.insert(turn_log, string.format(
            "turn %d: [%s]",
            info.turn_number, table.concat(ns, ",")
        ))
    end,
})

print("=== turn log ===")
for _, line in ipairs(turn_log) do print(line) end

print("\n=== result ===")
print("ok=" .. tostring(result.ok) .. " turns=" .. tostring(result.num_turns))
if result.usage then
    print(string.format("tokens: in=%d out=%d total=%d",
        result.usage.input_tokens, result.usage.output_tokens, result.usage.total_tokens))
end
if result.ok then
    print("--- final content ---")
    print(result.content)
else
    print("error=" .. tostring(result.error))
end

print("\n=== final KV ===")
local all = std.kv.list("todos")
table.sort(all)
for _, k in ipairs(all) do
    print(string.format("  %s = %s", k, tostring(std.kv.get("todos", k))))
end
