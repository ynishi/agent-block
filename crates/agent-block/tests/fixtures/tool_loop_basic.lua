-- tool_loop against a mock Anthropic endpoint.
--
-- Covers what the abstraction is for: it dispatches only the tools it was
-- handed, the set can change per turn, and a name outside the set comes back
-- as a tool_result the model can recover from rather than an error that ends
-- the run.

local loop = require("tool_loop")

local base = std.env.get("ANTHROPIC_BASE_URL_TEST")
local llm = { provider = "anthropic", base_url = base, api_key = "dummy", model = "mock", max_tokens = 512 }

-- Unpadded on purpose: the Rust assertions match these lines literally, and
-- column alignment is not something a test should be able to break.
local function line(k, v)
    print(string.format("[TL] %s=%s", k, tostring(v)))
end

-- Records what it was called with, so the test can assert dispatch happened.
local seen = {}
local function spec(name)
    return {
        name = name,
        description = name,
        input_schema = { type = "object", properties = {} },
        handler = function(input)
            table.insert(seen, name .. ":" .. tostring(input.v))
            return { echoed = input.v }
        end,
    }
end

-- 1 ─ dispatch + termination ------------------------------------------------
local r1 = loop.run({
    prompt = "call alpha then finish",
    system = "test",
    tools = { spec("alpha") },
    llm = llm,
})
line("1.ok", r1.ok)
line("1.turns", r1.turns)
line("1.tool_calls", #r1.tool_calls)
line("1.dispatched", table.concat(seen, ","))
line("1.content", r1.content)
line("1.usage_in>0", (r1.usage.input_tokens or 0) > 0)

-- 2 ─ a name outside the set is answered, not raised ------------------------
seen = {}
local r2 = loop.run({
    prompt = "call something that is not there",
    tools = { spec("alpha") },
    llm = llm,
})
line("2.ok", r2.ok)
local unknown_reported = false
for _, c in ipairs(r2.tool_calls) do
    if c.name == "ghost" and tostring(c.result):find("unknown tool", 1, true) then
        unknown_reported = true
    end
end
line("2.unknown_as_result", unknown_reported)
line("2.not_dispatched", #seen == 0)

-- 3 ─ adaptive: the set changes per turn ------------------------------------
seen = {}
local turns_seen = {}
local r3 = loop.run({
    prompt = "two turns",
    state = { n = 0 },
    tools = function(ctx)
        table.insert(turns_seen, ctx.turn)
        -- The write tool is withdrawn after the first turn.
        if ctx.turn == 1 then
            return { spec("alpha"), spec("beta") }
        end
        return { spec("alpha") }
    end,
    llm = llm,
})
line("3.ok", r3.ok)
line("3.tools_fn_turns", table.concat(turns_seen, ","))
line("3.dispatched", table.concat(seen, ","))

-- 4 ─ max_turns bound -------------------------------------------------------
local r4 = loop.run({
    prompt = "loop forever",
    tools = { spec("alpha") },
    max_turns = 2,
    llm = llm,
})
line("4.ok", r4.ok)
line("4.turns", r4.turns)
line("4.error", (r4.error or ""):find("max_turns", 1, true) ~= nil)

-- 5 ─ on_turn observability -------------------------------------------------
local observed = {}
loop.run({
    prompt = "call alpha then finish",
    tools = { spec("alpha") },
    llm = llm,
    on_turn = function(info)
        table.insert(observed, info.turn .. ":" .. #info.tool_calls)
    end,
})
line("5.on_turn", table.concat(observed, ","))

-- 6 ─ input validation ------------------------------------------------------
line("6.no_prompt", loop.run({ tools = {} }).ok == false)
line("6.bad_provider", loop.run({ prompt = "x", llm = { provider = "nope" } }).ok == false)

print("[TL] done")
