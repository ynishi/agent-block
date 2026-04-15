-- sql_roundtrip.lua — E2E fixture for std.sql bridge
-- Tests INSERT → SELECT → UPDATE → SELECT → DELETE → COUNT

-- 0. Bootstrap: manifest-driven migrate is TBD, so the fixture sets up its
-- own table via std.sql.exec (DDL is allowed through the direct bridge).
std.sql.exec("CREATE TABLE IF NOT EXISTS test_kv (k TEXT PRIMARY KEY, v TEXT)")

-- 1. INSERT
local ins = std.sql.exec("INSERT INTO test_kv (k, v) VALUES (?, ?)", { "hello", "world" })
print("affected=" .. tostring(ins.affected))

-- 2. SELECT
local rows = std.sql.query("SELECT k, v FROM test_kv WHERE k = ?", { "hello" })
print("row_count=" .. tostring(#rows))
print("k=" .. rows[1].k)
print("v=" .. rows[1].v)

-- 3. UPDATE
local upd = std.sql.exec("UPDATE test_kv SET v = ? WHERE k = ?", { "planet", "hello" })
print("updated=" .. tostring(upd.affected))

-- 4. SELECT after UPDATE
local rows2 = std.sql.query("SELECT v FROM test_kv WHERE k = 'hello'")
print("after_update=" .. rows2[1].v)

-- 5. DELETE
local del = std.sql.exec("DELETE FROM test_kv WHERE k = ?", { "hello" })
print("deleted=" .. tostring(del.affected))

-- 6. SELECT COUNT(*)
local rows3 = std.sql.query("SELECT COUNT(*) as cnt FROM test_kv")
print("count=" .. tostring(rows3[1].cnt))
