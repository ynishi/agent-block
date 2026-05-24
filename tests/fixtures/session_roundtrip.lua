-- session block round-trip: load (empty) → save → load → clear.
local session = require("session")

local id = "_e2e_session_" .. tostring(os.time())

-- 1. Empty load returns {} (no error).
local first = session.load(id)
print("first_type=" .. type(first))
print("first_count=" .. tostring(#first))

-- 2. Save a 3-turn synthetic messages array and load it back.
local synthetic = {
    { role = "user",      content = "hello" },
    { role = "assistant", content = "hi there" },
    { role = "user",      content = "how are you?" },
}
session.save(id, synthetic)

local loaded = session.load(id)
print("loaded_count=" .. tostring(#loaded))
print("loaded_role1=" .. tostring(loaded[1].role))
print("loaded_content2=" .. tostring(loaded[2].content))
print("loaded_role3=" .. tostring(loaded[3].role))

-- 3. clear removes the row (true), second clear returns false.
print("clear_existing=" .. tostring(session.clear(id)))
print("clear_missing=" .. tostring(session.clear(id)))

-- 4. load after clear returns {} again.
local after = session.load(id)
print("after_clear_count=" .. tostring(#after))

-- 5. id validation rejects empty / non-string.
local ok_empty = pcall(session.load, "")
print("reject_empty=" .. tostring(not ok_empty))
local ok_nil = pcall(session.load, nil)
print("reject_nil=" .. tostring(not ok_nil))
