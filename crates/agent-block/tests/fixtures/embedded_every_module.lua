-- embedded_every_module.lua — every embedded module answers a table, under
-- its own name and under the `embedded.` alias.
--
-- The rule this pins is the one the delegation idiom in the README rests on:
-- `local base = require("embedded.<name>")` is only a base to wrap if what
-- comes back is the module's table. A module that ran for its side effects
-- and returned nothing answers `true`, and the idiom silently has nothing to
-- wrap — which is what `fs_tools` / `kv_tools` / `sql_tools` / `ts_tools`
-- did while they were scripts the bridge ran rather than libraries it
-- installs.
--
-- The list is `EMBEDDED_BLOCKS` + `EMBEDDED_LIBS` from `src/host.rs`, plus
-- `knl_types`, the one embedded module with no file behind it (generated at
-- start from the Rust syscall surface). A name added there and not here is
-- a module nothing holds to this rule; the count below is the reminder.

local NAMES = {
    -- EMBEDDED_BLOCKS
    "agent",
    "coding",
    -- EMBEDDED_LIBS
    "session",
    "llm_proto",
    "llm_proto.openai",
    "llm_proto.anthropic",
    "lshape",
    "lshape.t",
    "lshape.check",
    "lshape.reflect",
    "lshape.luacats",
    "mcp_tools",
    "knl",
    "knl_adapter",
    "policy",
    "supervisor",
    "job",
    "fs_tools",
    "ts_tools",
    "sql_tools",
    "kv_tools",
    -- generated at host start, not a file
    "knl_types",
}

local failures = {}

local function probe(name)
    local ok, mod = pcall(require, name)
    if not ok then
        failures[#failures + 1] = string.format("require(%q) raised: %s", name, tostring(mod))
    elseif type(mod) ~= "table" then
        failures[#failures + 1] = string.format("require(%q) answered %s, not a table", name, type(mod))
    end
end

for _, name in ipairs(NAMES) do
    probe(name)
    probe("embedded." .. name)
end

print("checked=" .. #NAMES)
for _, line in ipairs(failures) do
    print("FAIL " .. line)
end
if #failures == 0 then
    print("ok")
end
