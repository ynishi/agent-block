-- Minimal smoke fixture for the :memory: SQLite path.
-- Requires AGENT_BLOCK_TS_PATH=:memory: to be set in the environment.
-- (The e2e_ts::ts_memory test sets this env variable automatically.)

std.ts.append("smoke_mem", 99)
local row = std.ts.last("smoke_mem")
print("mem_last_value=" .. tostring(row.value))
