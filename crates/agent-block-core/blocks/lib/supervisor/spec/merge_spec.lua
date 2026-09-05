-- merge_spec.lua — unit tests for `supervisor.merge`, the fold that reads
-- several sessions into one request.
--
-- Run via:
--   test_launch(code_file=".../supervisor/spec/merge_spec.lua",
--               search_paths=[".../blocks/lib"])  -- so require("supervisor") resolves
--
-- What this proves:
--   1 THE ORDER IS THE DOCUMENTED ONE: the listed sessions, in the order given,
--     then the folding session's own events. The order is a choice, so it is a
--     case rather than a paragraph;
--   2 A SESSION NOT IN THE LIST IS NOT READ. The set is what the fold was built
--     with, it reaches the kernel as `opts.sessions`, and nothing else appears
--     in the request;
--   3 each session is folded SEPARATELY — one fold over a merged event list
--     would let one session's tool_results answer another's tool_use ids;
--   4 nothing is appended anywhere: reading is all this does;
--   5 a read that hit the row cap raises rather than folding what it got.
--
-- The one stand-in: `std.json.decode` here is the support's memo, not a codec
-- (its header says why). What these cases pin is that the fold decodes the
-- `data` column it read and folds the result; what a real decode does to the
-- text is the host's.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("supervisor.spec.support")
local supervisor = require("supervisor")

local DEVICE = { system = "be terse" }

--- The request's messages as `role:text` lines, so a case can state the whole
--- order in one string.
local function rendered(request)
    local lines = {}
    for _, message in ipairs(request.messages) do
        local content = message.content
        if type(content) == "table" then
            local parts = {}
            for _, block in ipairs(content) do
                parts[#parts + 1] = block.text or block.type
            end
            content = table.concat(parts, "+")
        end
        lines[#lines + 1] = message.role .. ":" .. tostring(content)
    end
    return table.concat(lines, " | ")
end

--- A parent with two children, each carrying one exchange of its own.
local function tree()
    local parent = support.session({ budget = { amount = 20, tag = "beats" } })
    local first, second
    supervisor.child(parent, { budget = { amount = 4 } }, function(child)
        first = child
        support.seed(child, "find the files")
        support.answered(child, "two files")
    end)
    supervisor.child(parent, { budget = { amount = 4 } }, function(child)
        second = child
        support.seed(child, "read the notes")
        support.answered(child, "three notes")
    end)
    support.seed(parent, "put it together")
    return parent, first, second
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("supervisor.merge — the order", function()
    it("puts the listed sessions first, in the order given, then the parent's own", function()
        local parent, first, second = tree()
        local fold = supervisor.merge(parent, { first, second })
        expect(rendered(fold(parent:events(), DEVICE))).to.be(
            "user:find the files | assistant:two files | "
                .. "user:read the notes | assistant:three notes | "
                .. "user:put it together"
        )
    end)

    it("follows the list, not the order the sessions opened in", function()
        local parent, first, second = tree()
        local fold = supervisor.merge(parent, { second, first })
        expect(rendered(fold(parent:events(), DEVICE))).to.be(
            "user:read the notes | assistant:three notes | "
                .. "user:find the files | assistant:two files | "
                .. "user:put it together"
        )
    end)

    it("takes ids as readily as handles — a child is usually closed by then", function()
        local parent, first, second = tree()
        local fold = supervisor.merge(parent, { first:id(), second:id() })
        expect(rendered(fold(parent:events(), DEVICE))).to.be(
            "user:find the files | assistant:two files | "
                .. "user:read the notes | assistant:three notes | "
                .. "user:put it together"
        )
    end)
end)

describe("supervisor.merge — what is read", function()
    it("does not read a session that is not in the list", function()
        local parent, first = tree()
        local third
        supervisor.child(parent, { budget = { amount = 4 } }, function(child)
            third = child
            support.seed(child, "the unread one")
        end)

        local request = supervisor.merge(parent, { first })(parent:events(), DEVICE)
        expect(rendered(request):find("the unread one", 1, true)).to.be(nil)

        -- And the set reached the kernel as the read's own option, which is
        -- what makes one statement read a set of streams.
        local last = support.queries(parent)[#support.queries(parent)]
        expect(#last.opts.sessions).to.be(1)
        expect(last.opts.sessions[1]).to.be(first:id())
        expect(third:id() ~= first:id()).to.be(true)
    end)

    it("passes the read's own knobs through", function()
        local parent, first = tree()
        supervisor.merge(parent, { first }, { limit = 50, timeout_ms = 250 })(parent:events(), DEVICE)
        local last = support.queries(parent)[#support.queries(parent)]
        expect(last.opts.limit).to.be(50)
        expect(last.opts.timeout_ms).to.be(250)
    end)

    it("appends nothing to any stream", function()
        local parent, first, second = tree()
        local before = support.kinds(parent)
        supervisor.merge(parent, { first, second })(parent:events(), DEVICE)
        expect(support.kinds(parent)).to.be(before)
        expect(support.kinds(first)).to.be("session_opened,budget_granted,msg_user,llm_response,session_closed")
    end)

    it("raises when the read hit the row cap", function()
        local parent, first = tree()
        local fold = supervisor.merge(parent, { first })
        support.truncate(parent)
        expect(function()
            fold(parent:events(), DEVICE)
        end).to.fail()
    end)
end)

describe("supervisor.merge — the device is still the device's", function()
    it("carries system and tools from the device, not from any log", function()
        local parent, first = tree()
        local request = supervisor.merge(parent, { first })(parent:events(), {
            system = "be terse",
            tools = { add = { description = "add", input_schema = { type = "object" } } },
        })
        expect(request.system).to.be("be terse")
        expect(#request.tools).to.be(1)
        expect(request.tools[1].name).to.be("add")
    end)

    it("folds each session on its own, so a tool_use is answered by its own results", function()
        -- The repair `knl.fold` makes read-side: a `tool_use` with no
        -- answering `tool_result` is closed with a synthetic one. Folding two
        -- logs together would let the SECOND session's result close the
        -- FIRST's call — a request that says something neither session did.
        local parent = support.session({ budget = { amount = 20, tag = "beats" } })
        local caller, answerer
        supervisor.child(parent, { budget = { amount = 4 } }, function(child)
            caller = child
            child:append({
                kind = "llm_response",
                data = {
                    content = { { type = "tool_use", id = "call-1", name = "add" } },
                    usage = { input_tokens = 1, output_tokens = 1, thinking_tokens = 0 },
                },
            })
        end)
        supervisor.child(parent, { budget = { amount = 4 } }, function(child)
            answerer = child
            child:append({ kind = "tool_result", data = { call_id = "call-1", ok = true, result = "7" } })
        end)

        local request = supervisor.merge(parent, { caller, answerer })(parent:events(), DEVICE)
        local text = rendered(request)
        expect(text:find("tool_use", 1, true) ~= nil).to.be(true)
        -- Two tool_result blocks: the synthetic one that closed the dangling
        -- call in the first session, and the real one in the second.
        local first_result = text:find("tool_result", 1, true)
        expect(first_result ~= nil).to.be(true)
        expect(text:find("tool_result", first_result + 1, true) ~= nil).to.be(true)
    end)
end)

describe("supervisor.merge — construction", function()
    it("insists on a session and a non-empty list", function()
        local parent = support.session()
        expect(function()
            supervisor.merge("not a session", { "sess-1" })
        end).to.fail()
        expect(function()
            supervisor.merge(parent, {})
        end).to.fail()
        expect(function()
            supervisor.merge(parent, { 42 })
        end).to.fail()
    end)

    it("refuses an option it does not know, and a knob that is not whole", function()
        local parent = support.session()
        expect(function()
            supervisor.merge(parent, { parent:id() }, { sessions = { "sess-1" } })
        end).to.fail()
        expect(function()
            supervisor.merge(parent, { parent:id() }, { limit = 0 })
        end).to.fail()
        expect(function()
            supervisor.merge(parent, { parent:id() }, { timeout_ms = 1.5 })
        end).to.fail()
    end)

    it("answers a fold", function()
        local parent = support.session()
        expect(type(supervisor.merge(parent, { parent:id() }))).to.be("function")
    end)
end)
