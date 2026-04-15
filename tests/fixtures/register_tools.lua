-- register_tools.lua — verify std.kv.register_tools and std.sql.register_tools
-- register handlers into _TOOL_REGISTRY correctly.

local kv_names = std.kv.register_tools({ prefix = "k_", allowed = { "get", "set", "list" } })
print("kv_registered=" .. table.concat(kv_names, ","))

-- Exercise handlers via tool.call (round-trip without LLM)
tool.call("k_set", { ns = "demo", key = "x", value = 123 })
local got = tool.call("k_get", { ns = "demo", key = "x" })
print("k_get.value=" .. tostring(got.value))

local listed = tool.call("k_list", { ns = "demo" })
print("k_list.keys=" .. table.concat(listed.keys, ","))

-- ns_lock behaviour: agent cannot override ns.
local locked_names = std.kv.register_tools({ prefix = "ldemo_", ns_lock = "locked-ns", allowed = { "set", "get" } })
print("locked_registered=" .. table.concat(locked_names, ","))

tool.call("ldemo_set", { key = "a", value = "from-locked" })
-- Even if LLM tries to supply a different ns, ns_lock wins.
tool.call("ldemo_set", { ns = "ignored", key = "b", value = "still-locked" })
local lg = tool.call("ldemo_get", { key = "a" })
print("locked.value=" .. tostring(lg.value))
local other = tool.call("ldemo_get", { key = "b" })
print("locked.other=" .. tostring(other.value))

-- Verify that the supposedly-ignored ns did NOT receive the write.
print("ignored_list=" .. table.concat(std.kv.list("ignored"), ","))

-- SQL tool registration
local sql_names = std.sql.register_tools()
print("sql_registered=" .. table.concat(sql_names, ","))

-- Setup: ensure a table exists (POC allows DDL via std.sql.exec directly)
std.sql.exec([[CREATE TABLE IF NOT EXISTS poc_notes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    body TEXT
)]])

-- Allowed write
local w = tool.call("sql_exec", {
    sql = "INSERT INTO poc_notes (body) VALUES (?)",
    params = { "hello" },
})
print("sql_exec.affected=" .. tostring(w.affected))

-- Read back
local r = tool.call("sql_query", {
    sql = "SELECT body FROM poc_notes WHERE id = ?",
    params = { w.last_id },
})
print("sql_query.body=" .. tostring(r.rows[1].body))

local count = tool.call("sql_query", { sql = "SELECT COUNT(*) AS n FROM poc_notes" })
print("poc_notes_count=" .. tostring(count.rows[1].n))
