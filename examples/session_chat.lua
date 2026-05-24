-- examples/session_chat.lua
--
-- Single-shot CLI invocation that participates in a persistent conversation.
-- Run multiple times with the same AGENT_ID and the agent will remember the
-- prior turns:
--
--   AGENT_ID=alice AGENT_PROMPT="my name is alice" agent-block -s examples/session_chat.lua
--   AGENT_ID=alice AGENT_PROMPT="what is my name?" agent-block -s examples/session_chat.lua
--   AGENT_ID=alice agent-block -s examples/session_chat.lua -- clear   # wipe thread
--
-- The session block round-trips r.messages via std.kv (SQLite-backed,
-- persists across runs). Trimming / compaction is a caller concern — see
-- the optional trim_last_n example below.

local agent   = require("agent")
local session = require("session")

local id     = std.env.get_or("AGENT_ID",     "default")
local prompt = std.env.get_or("AGENT_PROMPT", "Say hello in one short sentence.")

-- Optional: trim history to last N messages to bound token cost.
local function trim_last_n(msgs, n)
    if #msgs <= n then return msgs end
    local out = {}
    for i = #msgs - n + 1, #msgs do
        table.insert(out, msgs[i])
    end
    return out
end

local prior = session.load(id)
print(string.format("session[%s] prior_turns=%d", id, #prior))

local r = agent.run({
    prompt  = prompt,
    history = prior,
    system  = "You are a concise assistant. Remember context from prior turns.",
    max_iterations = 5,
})

if not r.ok then
    print("agent error: " .. tostring(r.error))
    os.exit(1)
end

print("--- response ---")
print(r.content)
print("----------------")
print(string.format("usage: in=%d out=%d total=%d",
    r.usage.input_tokens, r.usage.output_tokens, r.usage.total_tokens))

-- Save the full thread back. Swap to `trim_last_n(r.messages, 40)` if the
-- conversation grows and per-call cost becomes a concern.
session.save(id, r.messages)
print(string.format("session[%s] saved_turns=%d", id, #r.messages))
