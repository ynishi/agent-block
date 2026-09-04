-- beat_e2e.lua — the smallest real shell over the knl beat primitive.
--
-- What this is: a caller-written loop (there is no run in knl — the loop is
-- composed on the spot, shell-style) driving `knl.beat(ctx)` against the
-- REAL Anthropic API through knl_adapter's LLMPort, with one Lua tool bound
-- through the ToolPort path. The full host provides the `knl` syscall
-- bridge and auto-loads `.env` (ANTHROPIC_API_KEY) from the project root.
--
-- Run:
--   agent-block -s crates/agent-block/examples/knl_beat.lua
--
-- Expected: the model calls the `add` tool once, the pair lands in the
-- history with the beat number, and the final beat settles on a plain
-- answer. The script prints `[E2E] all_ok` at the end.

local kernel = require("knl")
local adapter = require("knl_adapter")
local Outcome = kernel.Outcome

-- The provider backend: Port + shim, conf is llm_proto vocabulary.
local llm = adapter.anthropic:open({
    model = "claude-haiku-4-5-20251001",
    max_tokens = 1024,
})

-- One purpose-shaped Lua tool, bound through the adapter (flat spec form).
local tools = adapter.tools({
    {
        name = "add",
        description = "Add two numbers and return their sum.",
        input_schema = {
            type = "object",
            properties = {
                a = { type = "number" },
                b = { type = "number" },
            },
            required = { "a", "b" },
        },
        handler = function(args)
            return tostring(args.a + args.b)
        end,
    },
})

-- The loop's own cap: knl has no max_turns config — the loop lives here, and
-- the stopping guarantee is the budget the owner granted.
local MAX_BEATS = 4

local ctx = kernel.open({
    owner = "beat-e2e",
    budget = { amount = 50000, tag = "tokens" },
    llm = llm,
    tools = tools,
    system = "You are a terse assistant. Use the add tool for any arithmetic.",
})

ctx:append({
    kind = "msg_user",
    content = "What is 20250904 + 42? Use the add tool, then answer with just the number.",
})

local function has_tool_use(out)
    for _, block in ipairs(out.content or {}) do
        if block.type == "tool_use" then
            return true
        end
    end
    return false
end

-- The loop, written where it is needed, bounded by its own local cap.
local beats = 0
local last
while beats < MAX_BEATS do
    last = kernel.beat(ctx)
    beats = beats + 1
    print(string.format("[BEAT %d] status=%s", beats, tostring(last.status)))
    if not Outcome.is_ok(last) then
        break
    end
    if last.out.budget_stopped or not has_tool_use(last.out) then
        break
    end
end

Outcome.match(last, {
    ok = function(o)
        local text = {}
        for _, block in ipairs(o.out.content or {}) do
            if block.type == "text" then
                text[#text + 1] = block.text
            end
        end
        print("[E2E] final answer: " .. table.concat(text, " "))
    end,
    refused = function(o)
        print("[E2E] refused: " .. tostring(o.reason))
    end,
    error = function(o)
        print("[E2E] error(" .. tostring(o.kind) .. "): " .. tostring(o.detail))
    end,
})

local usage = ctx:view("usage")
print(string.format(
    "[E2E] beats=%d recorded=%d usage: calls=%s in=%s out=%s remaining=%s",
    beats,
    ctx:beats(),
    tostring(usage.model_calls),
    tostring(usage.input_tokens),
    tostring(usage.output_tokens),
    tostring(ctx:remaining())
))

local kinds = {}
for _, ev in ipairs(ctx:events()) do
    kinds[#kinds + 1] = ev.kind
end
print("[E2E] history: " .. table.concat(kinds, ","))

ctx:close("done")
print("[E2E] all_ok")
