--- session — Conversation session persistence over `std.kv`.
---
--- Thin wrapper around `std.kv` that round-trips an `agent.run` messages
--- array across process invocations. agent-block is single-run by design;
--- this module lets a caller keep one conversational thread alive without
--- introducing long-term memory / persona / vector-store concerns
--- (deliberately out of scope — handle those outside agent-block).
---
--- Usage:
---   local session = require("session")
---   local agent   = require("agent")
---
---   local id = os.getenv("AGENT_ID") or "default"
---   local prior = session.load(id)          -- empty table on first run
---
---   local r = agent.run({
---       prompt  = "hello again",
---       history = prior,                    -- prepended to messages
---       system  = "...",
---       timeout = 120,
---       max_iterations = 20,
---   })
---
---   session.save(id, r.messages)            -- store full thread back
---
--- Trim / compaction / summarisation are caller's responsibility:
---   session.save(id, trim_last_n(r.messages, 20))
---
--- Why this is not `knl.resume`
---   The kernel's log IS the thread, so resuming a kernel session by id would be
---   the smaller mechanism — but the two ids are not the same id. This module's
---   is the CALLER's name for a conversation ("default", `$AGENT_ID`), chosen
---   before anything is opened; `knl.resume{ session = <id> }` takes the id the
---   kernel MINTED when the stream was opened. Bridging the two needs
---   `agent.run` to hand its session id back and to accept one to resume into,
---   and `agent.run`'s result shape is closed with no field for it. Until that
---   exists this stays what it is: `agent.run`'s `messages` out, into `std.kv`,
---   and back in as `history` — which the run then lays down as events, so the
---   thread the kernel folds is the thread the caller saved.
---
--- Written in Teal and embedded through `include_tl!` (see `host.rs`,
--- EMBEDDED_LIBS); the Lua the host holds is what Teal generates from this
--- file, without these comments. The record is named `M` so that the
--- generated module reads as every hand-written one does — `M.NS = ...`,
--- `return M` — and a vendored copy is edited the same way.
---
--- API:
---   session.load(id)           → Messages ({} when absent)
---   session.save(id, msgs)     → void
---   session.clear(id)          → boolean (true if a row was deleted)
---   session.NS                 → namespace constant for advanced std.kv use
local host = require("host_types")
global std: host.Std
global log: host.Log

--- One message of an `agent.run` thread: `role` and `content` are what a
--- caller reads back, and whatever else the run wrote rides along.
local type Message = {string:any}
local type Messages = {Message}

local record M
    --- The `std.kv` namespace every session lives under.
    NS: string
end

M.NS = "_agent_block_session"

--- The id a caller names a conversation by: a non-empty string. Checked at
--- run time as well as at build, because the caller is a Lua script.
local function assert_id(id: any)
    if type(id) ~= "string" or id == "" then
        error("session: id must be a non-empty string", 3)
    end
end

--- Load the saved messages array for `id`: the array, or an empty one when
--- there is no prior session.
function M.load(id: string): Messages
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
    return v as Messages
end

--- Save the messages array for `id`. Overwrites any prior value.
function M.save(id: string, messages: Messages)
    assert_id(id)
    if type(messages) ~= "table" then
        error("session: save: messages must be a table", 2)
    end
    std.kv.set(M.NS, id, messages)
end

--- Delete the saved session for `id`: true when a row was removed, false
--- when none existed.
function M.clear(id: string): boolean
    assert_id(id)
    return std.kv.delete(M.NS, id)
end

return M
