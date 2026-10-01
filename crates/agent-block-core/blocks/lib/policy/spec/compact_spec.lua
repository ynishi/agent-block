-- compact_spec.lua — mlua-lspec unit tests for the three values that fold a
-- record into a summary: `policy.compact` (when, and the summarising fold),
-- `policy.ledger` (the facts beside the summary), `policy.window{
-- from_summary }` (the request from the summary on) — and `policy.room_note`
-- beside them, the line that tells the model what the request took.
--
-- Run via:
--   just test-lua compact_spec
--
-- What this proves:
--   1 compact: the bounds — `at` in (0, 1], a whole `min_beats`, a non-empty
--     `prompt`, a `fold` that is required — are loud, and an unknown option
--     is refused;
--   2 due reads the window report `knl.beat` wrote on the last llm_request
--     since the latest summary: "dropped" when that fold left beats out,
--     "share" when the request took `at` of the limit or more, nil below it,
--     nil while fewer than `min_beats` beats have followed the last summary,
--     and nil when no fold reported numbers and nothing was dropped;
--   3 the summarising fold is the caller's fold with the prompt as one more
--     user message at the end and no tools, and the report is the caller's
--     fold's, unchanged;
--   4 ledger: beats and summaries counted, calls per tool, the distinct
--     values per tool under the tracked keys in sorted order, the checks per
--     kind with the last answer, and the opts it refuses;
--   5 window{ from_summary }: the request is the seed, the summary rendered as
--     one user message (prefix, summary, ledger), and the beats after it; an
--     older summary and the summarising beat are behind the cut; a log with
--     no summary folds as before; `keep_seed` is required;
--   6 room_note appends one user message naming what the request took and
--     what is left, off the Port's count and profile, without touching the
--     request it was handed.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local shared = require("policy.shared")
local policy = require("policy")

-- ─────────────────────────────────────────────────────────────────────────────
-- Hand-built histories, in the shape `knl.beat` writes them.
-- ─────────────────────────────────────────────────────────────────────────────

local function seed(text, seq)
    return { { kind = "msg_user", data = { content = text }, seq = seq } }
end

--- One beat that answered with text; `window` goes on its llm_request as
--- `knl.beat` records a fold's report.
local function answered(id, body, seq, window)
    return {
        {
            kind = "llm_request",
            meta = { beat = id },
            data = { request = { messages = {} }, window = window },
            seq = seq,
        },
        {
            kind = "llm_response",
            meta = { beat = id },
            data = { content = { { type = "text", text = body } }, usage = {} },
            seq = seq + 1,
        },
    }
end

--- One beat that called a tool with `args` and recorded the pair.
local function called(id, name, args, seq)
    local call_id = id .. "-c"
    return {
        { kind = "llm_request", meta = { beat = id }, data = { request = { messages = {} } }, seq = seq },
        {
            kind = "llm_response",
            meta = { beat = id },
            data = { content = { { type = "tool_use", id = call_id, name = name, input = args } }, usage = {} },
            seq = seq + 1,
        },
        {
            kind = "tool_call",
            meta = { beat = id },
            data = { call_id = call_id, name = name, args = args },
            seq = seq + 2,
        },
        {
            kind = "tool_result",
            meta = { beat = id },
            data = { call_id = call_id, ok = true, result = "R" },
            seq = seq + 3,
        },
    }
end

local function summary(content, seq, ledger)
    return { { kind = "summary", data = { content = content, ledger = ledger }, seq = seq } }
end

local function check(kind, ok, seq)
    return { { kind = kind, data = { ok = ok, ran = true }, seq = seq } }
end

local function concat(...)
    local out = {}
    for _, list in ipairs({ ... }) do
        for _, ev in ipairs(list) do
            out[#out + 1] = ev
        end
    end
    return out
end

--- A real session holding `events`, so the predicates that ask
--- `knl.is_session` are satisfied.
local function log_of(events)
    local s = support.session()
    for _, ev in ipairs(events) do
        s:append(ev)
    end
    return s
end

local function none()
    return setmetatable({}, { __jsontype = "array" })
end

--- A report as the fitted fold writes it.
local function report(dropped, after, limit)
    return { dropped = dropped or none(), kept = 1, seed_kept = true, after = after, limit = limit }
end

--- A fold that answers a fixed request and report, and records what it saw.
local function recording_fold(request, rep)
    local seen = {}
    return function(events, device)
        seen[#seen + 1] = { events = events, device = device }
        return request, rep
    end,
        seen
end

--- A Port whose window is `window` and whose count is the length of every
--- message's content, strings only.
local function port_of(window, max_output)
    return {
        profile = function()
            return { context_window = window, max_output = max_output }
        end,
        count = function(_, request)
            local n = 0
            for _, message in ipairs(request.messages or {}) do
                if type(message.content) == "string" then
                    n = n + #message.content
                end
            end
            return n
        end,
    }
end

local plain_fold = function(events, device)
    return kernel.fold(events, device), report(none(), 1, 10)
end

-- ─────────────────────────────────────────────────────────────────────────────
-- 1 construction
-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.compact — construction", function()
    it("answers a predicate and a fold", function()
        local due, fold = policy.compact({ fold = plain_fold })
        expect(type(due)).to.be("function")
        expect(type(fold)).to.be("function")
    end)

    it("requires the caller's fold, in prod as well as dev", function()
        -- In dev the registry gate refuses the opts first (the shape names
        -- `fold`); in prod the factory's own check does. Either way it fails
        -- at the line that built it.
        expect(function()
            policy.compact({})
        end).to.fail()
        expect(function()
            policy.compact({ fold = "fold" })
        end).to.fail()
    end)

    it("holds `at` to (0, 1], `min_beats` to a whole number, and `prompt` to a non-empty string", function()
        -- Values the shape accepts (a number, a string) and the bound does
        -- not: these are the factory's own refusals, loud in both modes.
        for _, bad in ipairs({ 0, 1.5, -1 }) do
            local ok, err = pcall(policy.compact, { fold = plain_fold, at = bad })
            expect(ok).to.be(false)
            expect(tostring(err):find("at must be a number in (0, 1]", 1, true)).to.exist()
        end
        local ok, err = pcall(policy.compact, { fold = plain_fold, min_beats = 0 })
        expect(ok).to.be(false)
        expect(tostring(err):find("min_beats must be a whole number >= 1", 1, true)).to.exist()
        local ok2, err2 = pcall(policy.compact, { fold = plain_fold, prompt = "" })
        expect(ok2).to.be(false)
        expect(tostring(err2):find("prompt must be a non-empty string", 1, true)).to.exist()
    end)

    it("refuses an option it does not know, a session most of all", function()
        local ok, err = pcall(policy.compact, { fold = plain_fold, atx = 0.5 })
        expect(ok).to.be(false)
        expect(tostring(err):find("unknown option 'atx'", 1, true)).to.exist()
        local ok2, err2 = pcall(policy.compact, { fold = plain_fold, session = support.session() })
        expect(ok2).to.be(false)
        expect(tostring(err2):find("a session is an argument, never an option", 1, true)).to.exist()
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- 2 due
-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.compact — due", function()
    local due = policy.compact({ fold = plain_fold, at = 0.8, min_beats = 2 })

    it("answers nil while the last request is under the share and nothing was dropped", function()
        local s = log_of(
            concat(
                seed("task", 1),
                answered("b1", "one", 2, report(nil, 10, 100)),
                answered("b2", "two", 4, report(nil, 50, 100))
            )
        )
        expect(due(s)).to.be(nil)
    end)

    it('answers "share" when the last request took `at` of the limit or more', function()
        local s = log_of(
            concat(
                seed("task", 1),
                answered("b1", "one", 2, report(nil, 10, 100)),
                answered("b2", "two", 4, report(nil, 80, 100))
            )
        )
        expect(due(s)).to.be("share")
    end)

    it('answers "dropped" when the last fold left beats out, whatever the share', function()
        local s = log_of(
            concat(
                seed("task", 1),
                answered("b1", "one", 2, report(nil, 10, 100)),
                answered("b2", "two", 4, report({ "b1" }, 20, 100))
            )
        )
        expect(due(s)).to.be("dropped")
    end)

    it("answers nil until `min_beats` beats have followed the last summary", function()
        local one = log_of(concat(seed("task", 1), answered("b1", "one", 2, report(nil, 90, 100))))
        expect(due(one)).to.be(nil)
        local after = log_of(
            concat(
                seed("task", 1),
                answered("b1", "one", 2, report(nil, 90, 100)),
                answered("b2", "two", 4, report(nil, 95, 100)),
                summary("so far", 6),
                answered("b3", "three", 7, report(nil, 90, 100))
            )
        )
        expect(due(after)).to.be(nil)
        local later = log_of(
            concat(
                seed("task", 1),
                answered("b1", "one", 2, report(nil, 90, 100)),
                answered("b2", "two", 4, report(nil, 95, 100)),
                summary("so far", 6),
                answered("b3", "three", 7, report(nil, 90, 100)),
                answered("b4", "four", 9, report(nil, 90, 100))
            )
        )
        expect(due(later)).to.be("share")
    end)

    it("reads only the requests since the latest summary", function()
        local s = log_of(
            concat(
                seed("task", 1),
                answered("b1", "one", 2, report({ "b0" }, 90, 100)),
                summary("so far", 4),
                answered("b2", "two", 5, report(nil, 10, 100)),
                answered("b3", "three", 7, report(nil, 20, 100))
            )
        )
        expect(due(s)).to.be(nil)
    end)

    it("answers nil when the fold reported no numbers and dropped nothing", function()
        local s = log_of(
            concat(
                seed("task", 1),
                answered("b1", "one", 2, { dropped = none(), kept = 1, seed_kept = true }),
                answered("b2", "two", 4, { dropped = none(), kept = 2, seed_kept = true })
            )
        )
        expect(due(s)).to.be(nil)
        local bare = log_of(concat(seed("task", 1), answered("b1", "one", 2), answered("b2", "two", 4)))
        expect(due(bare)).to.be(nil)
    end)

    it("insists on a session, and refuses a log longer than one read", function()
        local ok, err = pcall(due, { events = function() end })
        expect(ok).to.be(false)
        expect(tostring(err):find("session must be a knl session", 1, true)).to.exist()
        local s = support.truncate(log_of(concat(seed("task", 1), answered("b1", "one", 2))))
        local ok2, err2 = pcall(due, s)
        expect(ok2).to.be(false)
        expect(tostring(err2):find("longer than one read", 1, true)).to.exist()
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- 3 the summarising fold
-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.compact — the summarising fold", function()
    it("is the caller's fold with the prompt last, no tools, and the report untouched", function()
        local request = {
            messages = {
                { role = "user", content = "task" },
                { role = "assistant", content = { { type = "text", text = "ok" } } },
            },
            system = "SYS",
            tools = { { name = "read" } },
        }
        local rep = report(nil, 42, 100)
        local inner, seen = recording_fold(request, rep)
        local _, fold = policy.compact({ fold = inner, prompt = "Summarise." })
        local events, device = { { kind = "msg_user", data = { content = "task" } } }, { system = "SYS" }
        local out, got = fold(events, device)
        expect(#seen).to.be(1)
        expect(seen[1].events).to.be(events)
        expect(seen[1].device).to.be(device)
        expect(got).to.be(rep)
        expect(#out.messages).to.be(3)
        expect(out.messages[3].role).to.be("user")
        expect(out.messages[3].content).to.be("Summarise.")
        expect(out.tools).to.be(nil)
        expect(out.system).to.be("SYS")
        -- The caller's request is not edited.
        expect(#request.messages).to.be(2)
        expect(request.tools).to.exist()
    end)

    it("ends with the default instruction when the caller names none", function()
        local inner = recording_fold({ messages = {} }, report(nil, 1, 10))
        local _, fold = policy.compact({ fold = inner })
        local out = fold({}, {})
        expect(out.messages[1].content).to.be(shared.DEFAULT_COMPACT_PROMPT)
        expect(shared.DEFAULT_COMPACT_PROMPT:find("no tool call", 1, true)).to.exist()
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- 4 ledger
-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.ledger", function()
    local events = concat(
        seed("task", 1),
        called("b1", "fs_read", { path = "b.rs" }, 2),
        called("b2", "fs_read", { path = "a.rs" }, 6),
        called("b3", "fs_edit", { path = "a.rs", content = "x" }, 10),
        check("verify", false, 14),
        summary("so far", 15),
        called("b4", "fs_read", { path = "b.rs", limit = 10 }, 16),
        check("verify", true, 20)
    )

    it("counts the beats, the summaries, the calls per tool and the checks per kind", function()
        local ledger = policy.ledger()(log_of(events))
        expect(ledger.beats).to.be(4)
        expect(ledger.summaries).to.be(1)
        expect(ledger.calls.fs_read).to.be(3)
        expect(ledger.calls.fs_edit).to.be(1)
        expect(ledger.checks.verify.count).to.be(2)
        expect(ledger.checks.verify.passed).to.be(1)
        expect(ledger.checks.verify.last_ok).to.be(true)
    end)

    it("keeps the distinct values per tool under the tracked keys, sorted", function()
        local ledger = policy.ledger()(log_of(events))
        expect(ledger.touched.fs_read.path).to.equal({ "a.rs", "b.rs" })
        expect(ledger.touched.fs_edit.path).to.equal({ "a.rs" })
        expect(ledger.touched.fs_read.limit).to.be(nil)
        local wide = policy.ledger({ track = { "path", "limit" } })(log_of(events))
        expect(wide.touched.fs_read.limit).to.be(nil) -- a number is not a name
    end)

    it("reads the kinds it is told as checks, and only those", function()
        local ledger = policy.ledger({ kinds = { "lint" } })(log_of(events))
        expect(ledger.checks.verify).to.be(nil)
        expect(ledger.checks.lint).to.be(nil)
    end)

    it("renders as one line per fact, in a fixed order", function()
        local text = shared.render_ledger(policy.ledger()(log_of(events)))
        local lines = {}
        for line in (text .. "\n"):gmatch("(.-)\n") do
            lines[#lines + 1] = line
        end
        expect(lines[1]).to.be("beats so far: 4; summaries before this one: 1")
        expect(lines[2]).to.be("fs_edit: called 1 time; path: a.rs")
        expect(lines[3]).to.be("fs_read: called 3 times; path: a.rs, b.rs")
        expect(lines[4]).to.be("verify: 2 run, 1 passed, the last one passed")
    end)

    it("holds its answer to the published shape", function()
        local check_shape = require("lshape.check")
        local ok, why = check_shape.check(policy.ledger()(log_of(events)), policy.shapes.ledger)
        expect(ok).to.be(true, why)
    end)

    it("refuses a track or kinds that is not an array of names, and an unknown option", function()
        -- A string where an array goes is the shape's refusal in dev and the
        -- factory's in prod; an empty name passes the shape and is the
        -- factory's in both.
        expect(function()
            policy.ledger({ track = "path" })
        end).to.fail()
        local ok2, err2 = pcall(policy.ledger, { kinds = { "" } })
        expect(ok2).to.be(false)
        expect(tostring(err2):find("kinds[1] must be a non-empty string", 1, true)).to.exist()
        local ok3, err3 = pcall(policy.ledger, { session = support.session() })
        expect(ok3).to.be(false)
        expect(tostring(err3):find("a session is an argument, never an option", 1, true)).to.exist()
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- 5 window{ from_summary }
-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.window — from_summary", function()
    local ledger = {
        beats = 2,
        summaries = 0,
        calls = { fs_read = 1 },
        touched = { fs_read = { path = { "a.rs" } } },
        checks = {},
    }
    local events = concat(
        seed("task", 1),
        { { kind = "config", data = { strict = false }, seq = 2 } },
        answered("b1", "one", 3),
        answered("b2", "two", 5),
        answered("s1", "the first summary", 7), -- the summarising beat
        summary("first", 9, ledger),
        answered("b3", "three", 10),
        answered("s2", "the second summary", 12), -- the second summarising beat
        summary("second", 14),
        answered("b4", "four", 15)
    )

    it("folds the seed, the latest summary as one user message, and the beats after it", function()
        local fold = policy.window({ tail = 10, keep_seed = true, from_summary = true })
        local request = fold(events, {})
        expect(#request.messages).to.be(3)
        expect(request.messages[1].content).to.be("task")
        expect(request.messages[2].role).to.be("user")
        local text = request.messages[2].content
        expect(text:sub(1, #shared.SUMMARY_PREFIX)).to.be(shared.SUMMARY_PREFIX)
        expect(text:find("Summary:\nsecond", 1, true)).to.exist()
        expect(text:find("first", 1, true)).to.be(nil)
        expect(text:find("the second summary", 1, true)).to.be(nil)
        expect(request.messages[3].content[1].text).to.be("four")
    end)

    it("renders the ledger under the summary when the event carried one", function()
        local fold = policy.window({ tail = 10, keep_seed = true, from_summary = true })
        local request = fold(
            concat(seed("task", 1), answered("b1", "one", 2), summary("so far", 4, ledger), answered("b2", "two", 5)),
            {}
        )
        local text = request.messages[2].content
        expect(
            text:find(
                "Ledger:\nbeats so far: 2; summaries before this one: 0\nfs_read: called 1 time; path: a.rs",
                1,
                true
            )
        ).to.exist()
    end)

    it("still windows the beats after the summary, and reports what it dropped", function()
        local fold = policy.window({ tail = 1, keep_seed = true, from_summary = true })
        local request, rep =
            fold(concat(seed("task", 1), summary("so far", 2), answered("b1", "one", 3), answered("b2", "two", 5)), {})
        expect(#request.messages).to.be(3)
        expect(request.messages[3].content[1].text).to.be("two")
        expect(rep.dropped).to.equal({ "b1" })
        expect(rep.seed_kept).to.be(true)
    end)

    it("folds a log with no summary exactly as before", function()
        local plain = concat(seed("task", 1), answered("b1", "one", 2), answered("b2", "two", 4))
        local with = policy.window({ tail = 10, keep_seed = true, from_summary = true })(plain, {})
        local without = policy.window({ tail = 10, keep_seed = true })(plain, {})
        expect(with).to.equal(without)
    end)

    it("fits against the compacted request, not the whole log", function()
        -- The port counts the string contents: the seed, the rendered summary
        -- (its prefix included) and the beat after it fit a window the
        -- thousand-character beat before the summary never would.
        -- The port counts string contents, so the bulk is a user message
        -- between two beats: behind the summary's cut, and behind the first
        -- beat a plain window has to drop.
        local port = port_of(400, 0)
        local long = concat(
            seed("task", 1),
            answered("b0", "zero", 2),
            { { kind = "msg_user", data = { content = string.rep("x", 1000) }, seq = 4 } },
            answered("b1", "one", 5),
            summary("short", 7),
            answered("b2", "two", 8)
        )
        local fold, fits = policy.window({ fit = { port = port }, keep_seed = true, from_summary = true })
        local request, rep = fold(long, {})
        expect(#request.messages).to.be(3)
        expect(#rep.dropped).to.be(0)
        expect(fits(log_of(long), {})).to.be(nil)
        -- The same window without the summary has to drop a beat to fit.
        local _, plain = policy.window({ fit = { port = port }, keep_seed = true })(long, {})
        expect(plain.dropped).to.equal({ "b0" })
    end)

    it("needs keep_seed, and a boolean", function()
        local ok, err = pcall(policy.window, { tail = 2, from_summary = true })
        expect(ok).to.be(false)
        expect(tostring(err):find("from_summary needs keep_seed = true", 1, true)).to.exist()
        expect(function()
            policy.window({ tail = 2, keep_seed = true, from_summary = "yes" })
        end).to.fail()
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- 6 room_note
-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.room_note", function()
    it("appends one user message naming what the request took and what is left", function()
        local filter = policy.room_note({ port = port_of(100, 0) })
        local request = { messages = { { role = "user", content = string.rep("x", 40) } }, system = "SYS" }
        local out = filter(request)
        expect(#out.messages).to.be(2)
        expect(out.messages[2].role).to.be("user")
        expect(out.messages[2].content).to.be("[window] 40 of 100 tokens used; 60 left for the reply")
        expect(out.system).to.be("SYS")
        expect(#request.messages).to.be(1)
    end)

    it("bounds what is left by the wire's cap, and never says less than nothing", function()
        local capped = policy.room_note({ port = port_of(100, 20) })({
            messages = { { role = "user", content = string.rep("x", 40) } },
        })
        expect(capped.messages[2].content).to.be("[window] 40 of 100 tokens used; 20 left for the reply")
        local over = policy.room_note({ port = port_of(100, 0) })({
            messages = { { role = "user", content = string.rep("x", 120) } },
        })
        expect(over.messages[2].content).to.be("[window] 120 of 100 tokens used; 0 left for the reply")
    end)

    it("refuses a port that cannot count or answer a profile, and an unknown option", function()
        local ok, err = pcall(policy.room_note, { port = {} })
        expect(ok).to.be(false)
        expect(tostring(err):find("port must answer count", 1, true)).to.exist()
        local ok2, err2 = pcall(policy.room_note, { port = port_of(100, 0), share = 1 })
        expect(ok2).to.be(false)
        expect(tostring(err2):find("unknown option 'share'", 1, true)).to.exist()
    end)
end)
