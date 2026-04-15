-- Same pattern as agent_with_kv_v2.lua, but wired via std.sql.register_tools.
-- Exercises the LLM-facing sql_query / sql_exec tools on the agent's local SQLite.

local agent = require("agent")

-- Setup: ensure table exists. Agent author decides the schema; LLM only
-- interacts via the sql_* tools.
std.sql.exec([[
    CREATE TABLE IF NOT EXISTS notes (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        topic      TEXT NOT NULL,
        body       TEXT NOT NULL,
        created_at INTEGER NOT NULL
    )
]])

local names = std.sql.register_tools()
print("registered=" .. table.concat(names, ","))

local turn_log = {}

local result = agent.run({
    prompt = [[
Manage a small notes table using the sql_* tools.

1. Insert three notes into the `notes` table:
   - topic="kv", body="JSON-backed local store"
   - topic="sql", body="Embedded SQLite local store"
   - topic="timeout", body="5s default per query"
   Use NOW-ish unix timestamps for created_at (you may pass the same constant 0 for all rows).
2. Query all rows, ordered by id ascending.
3. Query only rows where topic = 'sql' using a parameterized statement.
4. Return a short plain-text summary of what you found.

Use the tools; do not invent data.
]],
    system  = "You are a concise assistant that uses the provided sql_* tools to manage a local SQLite table. Prefer parameterized queries.",
    model   = "claude-haiku-4-5-20251001",
    max_tokens     = 600,
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

print("\n=== final SQL state ===")
local rows = std.sql.query("SELECT id, topic, body FROM notes ORDER BY id")
print("row_count=" .. tostring(#rows))
for _, r in ipairs(rows) do
    print(string.format("  [%d] %s: %s", r.id, r.topic, r.body))
end
