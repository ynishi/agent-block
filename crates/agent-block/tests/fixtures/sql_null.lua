-- sql_null.lua — E2E fixture for SQL NULL ↔ std.sql.null round-trip.
-- Verifies that NULL columns survive SELECT as the sentinel, and that
-- `col == std.sql.null` distinguishes "NULL" from "absent".

std.sql.exec([[
    CREATE TABLE IF NOT EXISTS null_probe (
        id     INTEGER PRIMARY KEY,
        label  TEXT,
        note   TEXT
    )
]])

-- Two rows: one with NULL note, one with a real note.
std.sql.exec("INSERT INTO null_probe (id, label, note) VALUES (?, ?, NULL)", { 1, "empty" })
std.sql.exec("INSERT INTO null_probe (id, label, note) VALUES (?, ?, ?)", { 2, "present", "hi" })

local rows = std.sql.query("SELECT id, label, note FROM null_probe ORDER BY id")
print("row_count=" .. tostring(#rows))

-- Row 1 should have note == std.sql.null (sentinel), NOT nil/absent.
local r1 = rows[1]
print("r1.note_is_null=" .. tostring(r1.note == std.sql.null))
print("r1.note_is_nil=" .. tostring(r1.note == nil))
-- Key must still be present in the row table (we didn't skip NULL anymore).
local has_note_key = false
for k, _ in pairs(r1) do
    if k == "note" then has_note_key = true end
end
print("r1.has_note_key=" .. tostring(has_note_key))

-- Row 2 should have a real string.
local r2 = rows[2]
print("r2.note_is_null=" .. tostring(r2.note == std.sql.null))
print("r2.note=" .. tostring(r2.note))
