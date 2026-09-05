-- compile_loop must declare and dispatch conf.extra_tools.
--
-- This has no coverage other than here: the device's tools map is assembled
-- inside the loop, so a caller tool that silently stops being declared looks
-- exactly like a model that chose not to call it.

local cl = require("compile_loop")

local target = std.env.get("COMPILE_LOOP_TARGET")
assert(target, "COMPILE_LOOP_TARGET must be set")

local f = assert(io.open(target, "w"))
f:write('print("hello")\n')
f:close()

-- Passes once the file says "world".
local function runner(path)
    local fh = assert(io.open(path, "r"))
    local content = fh:read("*a") or ""
    fh:close()
    if content:find("world", 1, true) then
        return { ok = true, stdout = "ok", stderr = "", exit_code = 0 }
    end
    return { ok = false, stdout = "", stderr = "not yet", exit_code = 1 }
end

local hint_calls = 0
local td = cl.make({
    runner = runner,
    lang = "lua",
    max_iters = 2,
    edit_mode = "diff",
    extra_tools = {
        {
            name = "get_hint",
            schema = {
                description = "Return the replacement the spec is asking for.",
                input_schema = { type = "object", properties = {} },
            },
            handler = function()
                hint_calls = hint_calls + 1
                return "use world"
            end,
        },
    },
    llm = {
        provider = "anthropic",
        base_url = std.env.get("ANTHROPIC_BASE_URL_TEST"),
        api_key = "dummy",
        model = "claude-haiku-mock",
    },
})

local raw = td.handler({ spec = 'change print("hello") to print("world")', target_file = target })
local result = std.json.decode(raw)

print("[XT] ok=" .. tostring(result.ok))
print("[XT] hint_calls=" .. tostring(hint_calls))
print("[XT] iters=" .. tostring(result.iters))
print("[XT] done")
