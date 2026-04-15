-- Agentify-scanner-like flow using std.kv + std.sql.
--
-- Demonstrates a realistic block usage:
--   1. read cursor (kv)
--   2. scan fake input
--   3. insert candidates (sql)
--   4. advance cursor (kv)
--   5. query top-N (sql)
--
-- Manual JSON equivalent would need:
--   - std.fs.read + std.json.decode on each poll (missing-file branch)
--   - std.fs.write + atomic dance (tmp + rename; rename not in std.fs yet)
--   - hand-rolled array iteration for filtering / sorting
--   - per-call error handling boilerplate

local scanner = "agentify-scanner"

-- 1. Ensure schema (POC: DDL via exec; production: manifest-driven migrate)
std.sql.exec([[
  CREATE TABLE IF NOT EXISTS candidates (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    kind       TEXT NOT NULL,
    reason     TEXT,
    created_at INTEGER NOT NULL
  )
]])

-- 2. Read cursor
local cursor_before = std.kv.get("cursor", scanner) or "(none)"
print("cursor_before=" .. cursor_before)

-- 3. Fake scan: 3 new sessions
local new_sessions = {
  { id = "sess-101", kind = "agent-candidate", reason = "repeated web+read+grep sequence" },
  { id = "sess-102", kind = "skill-candidate", reason = "complex argument-parsing stanza" },
  { id = "sess-103", kind = "agent-candidate", reason = "three-stage planning pattern" },
}

-- 4. Insert + advance cursor
local base_ts = os.time()
for i, s in ipairs(new_sessions) do
  local r = std.sql.exec(
    "INSERT INTO candidates (session_id, kind, reason, created_at) VALUES (?, ?, ?, ?)",
    { s.id, s.kind, s.reason, base_ts + i }
  )
  print(string.format("inserted id=%d session=%s", r.last_id, s.id))
end
std.kv.set("cursor", scanner, new_sessions[#new_sessions].id)

-- 5. Top-N agent-candidate query
local top = std.sql.query(
  "SELECT session_id, kind, reason FROM candidates "
    .. "WHERE kind = ? ORDER BY created_at DESC LIMIT ?",
  { "agent-candidate", 5 }
)
print("top_count=" .. tostring(#top))
for i, r in ipairs(top) do
  print(string.format("top[%d]=%s :: %s", i, r.session_id, r.reason))
end

-- 6. Cursor after
print("cursor_after=" .. std.kv.get("cursor", scanner))

-- 7. Total count across all kinds (SQL shines here)
local total = std.sql.query("SELECT COUNT(*) AS n FROM candidates")
print("total=" .. tostring(total[1].n))
