-- blocks/session/init.lua — Conversation session persistence (StdPkg)
--
-- Thin wrapper around `std.kv` that round-trips an `agent.run` messages
-- array across process invocations. agent-block is single-run by design;
-- this block lets a caller keep one conversational thread alive without
-- introducing long-term memory / persona / vector-store concerns
-- (deliberately out of scope — handle those outside agent-block).
--
-- Usage:
--   local session = require("session")
--   local agent   = require("agent")
--
--   local id = os.getenv("AGENT_ID") or "default"
--   local prior = session.load(id)          -- empty table on first run
--
--   local r = agent.run({
--       prompt  = "hello again",
--       history = prior,                    -- prepended to messages
--       system  = "...",
--   })
--
--   session.save(id, r.messages)            -- store full thread back
--
-- Trim / compaction / summarisation are caller's responsibility:
--   session.save(id, trim_last_n(r.messages, 20))
--
-- API:
--   session.load(id)           → table (messages array; {} when absent)
--   session.save(id, msgs)     → void
--   session.clear(id)          → boolean (true if a row was deleted)
--   session.NS                 → namespace constant for advanced std.kv use

local M = {}

M.NS = "_agent_block_session"

local function assert_id(id)
    if type(id) ~= "string" or id == "" then
        error("session: id must be a non-empty string", 3)
    end
end

--- Load the saved messages array for `id`.
--- @param id string
--- @return table  Messages array. Empty table when no prior session.
function M.load(id)
    assert_id(id)
    local v = std.kv.get(M.NS, id)
    if v == nil then
        return {}
    end
    if type(v) ~= "table" then
        -- Defensive: a non-table value would mean the slot was clobbered by
        -- something other than session.save. Surface as empty so a corrupted
        -- row does not crash the agent loop; caller can session.clear(id).
        log.warn("session: load(" .. id .. "): non-table value, returning empty")
        return {}
    end
    return v
end

--- Save the messages array for `id`. Overwrites any prior value.
--- @param id string
--- @param messages table
function M.save(id, messages)
    assert_id(id)
    if type(messages) ~= "table" then
        error("session: save: messages must be a table", 2)
    end
    std.kv.set(M.NS, id, messages)
end

--- Delete the saved session for `id`.
--- @param id string
--- @return boolean  true when a row was removed, false when none existed
function M.clear(id)
    assert_id(id)
    return std.kv.delete(M.NS, id)
end

return M
