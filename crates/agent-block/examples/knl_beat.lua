-- knl_beat.lua — the smallest real shell over the knl beat primitive.
--
-- What this is: a caller-written loop (there is no run in knl — the loop is
-- composed on the spot, shell-style) driving `knl.beat(session, device)`
-- against the REAL Anthropic API through knl_adapter's LLMPort, with one Lua
-- tool bound through the ToolPort path. The lifecycle is the canonical
-- bracket, `knl.session(opts, fn)`: the kernel opens, runs the body and
-- closes — an error escaping the body still records the boundary. The full
-- host provides the `knl` syscall bridge and auto-loads `.env`
-- (ANTHROPIC_API_KEY) from the project root.
--
-- Run:
--   agent-block -s crates/agent-block/examples/knl_beat.lua
--
-- Expected: the model calls the `add` tool once, the pair lands in the
-- history under that beat's id, and the final beat settles on a plain
-- answer. The run is then read back with `knl.views.beats` — one SELECT over
-- the log, one row per beat. The script prints `[E2E] all_ok` at the end.
--
-- Two sections, and the difference between them is the point:
--   [1] the plain kernel — a device with an llm, tools and a system line, and
--       a loop that stops on a beat with no tool call;
--   [2] the same run with `policy` plugged in — a windowed fold, a filter that
--       carries a failure forward, and two questions the loop asks between
--       beats. Nothing in the kernel changes to make the second one work:
--       every policy is a value in a seam the device already had, or a
--       predicate the loop calls itself.

local kernel = require("knl")
local adapter = require("knl_adapter")
local policy = require("policy")
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

-- The policy half: resolved once, frozen, reusable across sessions.
local device = kernel.device({
    llm = llm,
    tools = tools,
    system = "You are a terse assistant. Use the add tool for any arithmetic.",
})

-- The loop's own cap: knl has no beat cap of its own — the loop lives inside
-- the bracket, and the stopping guarantee is the budget the owner granted.
-- It is the tighter of the two bounds below, so this run ends on the model
-- rather than on the quota; a run that did hit the grant would come back
-- `stopped`, which the match below prints.
local MAX_BEATS = 4

-- ===========================================================================
-- [1] the plain kernel
-- ===========================================================================

local function has_tool_use(out)
    for _, block in ipairs(out.content or {}) do
        if block.type == "tool_use" then
            return true
        end
    end
    return false
end

kernel.session({
    owner = "beat-e2e",
    -- The grant is counted in whatever the owner tags it with, and the
    -- kernel reads the number and nothing else: with the default cost (one
    -- unit per beat) the unit here is a beat, not a token. Tagging it
    -- "tokens" would promise a bound the kernel does not enforce — token
    -- usage is the separate `knl.views.usage` reading printed below.
    budget = { amount = 8, tag = "beats", desc = "one unit per beat" },
}, function(s)
    -- The seed is an event like any other: the envelope is `{ kind, beat?,
    -- meta?, data? }` and what the kind is about goes under `data`. `meta`
    -- is the shallow-label half — string / number / boolean values only —
    -- and it is what a view can read without being tied to any kind's shape.
    s:append({
        kind = "msg_user",
        meta = { label = "seed" },
        data = { content = "What is 20250904 + 42? Use the add tool, then answer with just the number." },
    })

    local beats = 0
    local last
    while beats < MAX_BEATS do
        last = kernel.beat(s, device)
        beats = beats + 1
        print(string.format("[BEAT %d] status=%s", beats, tostring(last.status)))
        if not Outcome.is_ok(last) then
            break
        end
        if not has_tool_use(last.out) then
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
            -- A detail is a sentence, or a record with the sentence under
            -- `message` — the kernel's reading of a syscall failure (kind /
            -- retryable) for `state`, or a traced raise in dev mode. Read
            -- it the one way that works for both.
            local detail = o.detail
            if type(detail) == "table" then
                detail = tostring(detail.message)
                if o.detail.kind ~= nil then
                    detail = o.detail.kind .. ": " .. detail
                end
            end
            print("[E2E] error(" .. tostring(o.kind) .. "): " .. tostring(detail))
        end,
        stopped = function(o)
            print("[E2E] stopped(" .. tostring(o.reason) .. "): grant " .. tostring(o.tag))
        end,
    })

    -- One id per beat, declared by the shell — and the grouping is a read,
    -- not a loop written here: `knl.views.beats` runs one SELECT over the
    -- log and answers a row per beat. A consumer's own
    -- view is a function of exactly this form.
    local grouped = kernel.views.beats(s)

    local kinds = {}
    for _, ev in ipairs(s:events()) do
        kinds[#kinds + 1] = ev.kind
    end

    -- The token accounting is a view like the grouping above — one SELECT,
    -- one row per stream that answered — and not something the kernel serves
    -- itself. This run reads its own stream, so there is one row (or none,
    -- had no beat come off).
    local usage = kernel.views.usage(s)[1] or { calls = 0, input_tokens = 0, output_tokens = 0 }
    print(
        string.format(
            "[E2E] beats=%d declared=%d usage: calls=%s in=%s out=%s remaining=%s",
            beats,
            #grouped,
            tostring(usage.calls),
            tostring(usage.input_tokens),
            tostring(usage.output_tokens),
            tostring(s:remaining())
        )
    )
    print("[E2E] history: " .. table.concat(kinds, ","))
    for i, row in ipairs(grouped) do
        print(
            string.format(
                "[E2E] beat %d: %s seq %s..%s kinds=%s",
                i,
                tostring(row.beat),
                tostring(row.seq_from),
                tostring(row.seq_to),
                tostring(row.kinds)
            )
        )
    end
end)

-- ===========================================================================
-- [2] the same run, with policies plugged in
-- ===========================================================================
--
-- Four policies, and each one goes exactly where the kernel already had a
-- seam:
--
--   window      the device's `fold`. The request carries the last 3 beats and
--               nothing earlier, sliced by beat so a tool pair is never split.
--   carry       one of the device's `filters`. If the beat before ended in a
--               tool error or a call that did not come off, one bounded note
--               goes in front of the request saying so — which is the only
--               way the model hears about a failed CALL at all, since the
--               fold skips `llm_call_failed`.
--   stagnation  the loop's own. Two counters over the log: the same tool call
--               three beats running, or two beats that wrote nothing.
--   escalate    the loop's own. After a refusal or a failure that asking again
--               would not fix, the next beat runs on the stronger model —
--               changing the tool, not handing the work to a supervisor.
--
-- `carry` is the one that has to be bound, and it is bound INSIDE the bracket:
-- its opts are policy and the session is an argument, so the policy is built
-- wherever and the binding happens where the session exists.

local strong = adapter.anthropic:open({
    model = "claude-sonnet-4-5-20250929",
    max_tokens = 1024,
})

-- Session-free: both are values, held out here and reused for any run.
local stalled = policy.stagnation({ same = 3, no_progress = 2 })
local escalate = policy.escalate({ strong = strong })

kernel.session({
    owner = "beat-e2e-policy",
    budget = { amount = 8, tag = "beats", desc = "one unit per beat" },
}, function(s)
    s:append({
        kind = "msg_user",
        meta = { label = "seed" },
        data = { content = "What is 1918 + 77, and then that plus 5? Use the add tool for each step." },
    })

    local policied = kernel.device({
        llm = llm,
        tools = tools,
        system = "You are a terse assistant. Use the add tool for any arithmetic.",
        fold = policy.window({ tail = 3 }),
        filters = { policy.carry({ max_bytes = 400 })(s) },
    })

    -- The device for the NEXT beat is a value the loop carries, which is what
    -- lets `escalate` answer it without anything being mutated: the original
    -- device stays exactly as it was built.
    local current = policied
    local beats, last, why = 0, nil, nil
    while beats < MAX_BEATS do
        last = kernel.beat(s, current)
        beats = beats + 1
        print(string.format("[POLICY BEAT %d] status=%s", beats, tostring(last.status)))

        current = escalate(last, current)
        if current ~= policied then
            print("[POLICY] escalated: the next beat runs on the stronger model")
        end

        if not Outcome.is_ok(last) then
            break
        end
        if not has_tool_use(last.out) then
            break
        end

        why = stalled(s)
        if why ~= nil then
            print("[POLICY] stagnation: " .. why)
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
            print("[POLICY] final answer: " .. table.concat(text, " "))
        end,
        refused = function(o)
            print("[POLICY] refused: " .. tostring(o.reason))
        end,
        error = function(o)
            local detail = o.detail
            if type(detail) == "table" then
                detail = tostring(detail.message)
            end
            print("[POLICY] error(" .. tostring(o.kind) .. "): " .. tostring(detail))
        end,
        stopped = function(o)
            print("[POLICY] stopped(" .. tostring(o.reason) .. "): grant " .. tostring(o.tag))
        end,
    })

    -- What the request the last beat sent actually carried, read out of the
    -- durable record: the window is visible as a message count that stops
    -- growing, and a carried note as the first message.
    local requests = {}
    for _, ev in ipairs(s:events()) do
        if ev.kind == "llm_request" then
            requests[#requests + 1] = ev.data.request
        end
    end
    local last_request = requests[#requests]
    print(
        string.format(
            "[POLICY] beats=%d declared=%d messages_sent_last=%d stagnation=%s escalated=%s",
            beats,
            #kernel.views.beats(s),
            last_request and #last_request.messages or 0,
            tostring(why),
            tostring(current ~= policied)
        )
    )
end)

print("[E2E] all_ok")
