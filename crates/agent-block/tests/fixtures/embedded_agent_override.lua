-- embedded_agent_override.lua — `require("agent")` under a project that
-- shadows the embedded `agent` and delegates to it through `embedded.agent`.
--
-- The override module itself is written into the project root by the test
-- (a checked-in `blocks/` directory here would be a second override for every
-- other fixture). What this script proves is only the caller's side: the name
-- `agent` resolves to the project's module, and that module reached the one it
-- replaced.
local agent = require("agent")

local res = agent.run({ prompt = "not sent anywhere" })

print("RESULT_FROM=" .. tostring(res.from))
print("RESULT_PROMPT=" .. tostring(res.prompt))
