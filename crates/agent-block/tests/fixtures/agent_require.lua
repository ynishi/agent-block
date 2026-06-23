-- agent_require.lua — verify require("agent") succeeds and exposes expected API
local agent = require("agent")

-- agent.run must be a function
assert(type(agent.run) == "function", "agent.run should be a function")

print("agent module loaded successfully")
print("agent.run type: " .. type(agent.run))
