-- replay_spec.lua — mlua-lspec specs that replay decisions policies made in
-- real runs, from the recorded log.
--
-- Run via:
--   just test-lua replay_spec
--
-- WHAT REPLAY MEANS HERE. A policy reads only the log (or its arguments) and
-- holds no state of its own (policy/init.tl, the rules every policy keeps).
-- So a recorded prefix of a run's log IS the whole input to a decision the
-- policy made there: put the prefix in a session, ask the policy the same
-- question, and it has to answer what it answered in the run. No model is
-- called and no loop runs — what is replayed is the decision, not the run.
-- A case pins a failure a real run found, so the fix for it cannot quietly
-- come undone; the fixtures under `replay/` say which run, and what in them
-- was exported and what reconstructed.
--
-- What this proves:
--   a parallel_read — the first beat of a run that asked for six reads at
--     once: over the run's room (window 8000, max_tokens 2048, result_share
--     0.3, beat_share 0.5 -> beat_budget 2976), `policy.beat_cap` refuses
--     the same three reads the run refused, each with the `tokens` / `used`
--     / `limit` the run recorded, and lets the other three through. The room
--     counts with the Port's estimate (`knl_adapter`'s, mirrored below),
--     which is what that run counted with;
--   b empty_summary — `policy.compact` asks for a summary where the run did
--     (`share`), and its summarising fold sends the recorded history with no
--     `tool_use` / `tool_result` block (every message's content is a string,
--     no tools) and the prompt last; and after a `compact_skipped` mark where
--     the summary came back empty, `due` answers nil until `min_beats` beats
--     follow — without the mark it would ask again on the very next beat;
--   c stale_seed — a seed saying the build is FAILING, the run's beats, and
--     a summary whose ledger (read by `policy.ledger` off the recorded log)
--     says the last verify passed: `policy.window{ room, keep_seed = true,
--     from_summary = true }` renders the summary with `shared.SUMMARY_PREFIX`,
--     which says the ledger is newer than the task message, and the ledger
--     line `verify: ... the last one passed`, after the seed.
--
-- HOW TO ADD A CASE from a new real run:
--   1 export the run's log: `agent-block knl export --store <db> --session
--     <id> --as events > run.jsonl` (the run's JSON result line names the
--     session);
--   2 convert the seq range the decision was made on into
--     `replay/<case>.lua`, a module returning `{ source = "<one line: the
--     model, the date, the room>", events = { { seq, kind, meta, data } ...
--     } }`: drop the kinds only the kernel writes, rewrite every absolute
--     path to /repo, keep `config` as its room and model name and an
--     `llm_request` as its window report, rename beat ids, cut long text to
--     a stand-in — but where the policy MEASURES a length (the caps, the
--     window), replace the text with filler of the length it rendered to, so
--     the count comes out as it did. Say in the file's header what was
--     exported and what, if anything, was reconstructed;
--   3 add a `describe` here that loads the prefix with `load_prefix`, asks
--     the policy built with the run's numbers (its `config` event has the
--     room), and asserts what the run decided — the recorded values where
--     the log has them, not numbers worked out again.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")
local shared = require("policy.shared")

-- ─────────────────────────────────────────────────────────────────────────────
-- What every case is built from
-- ─────────────────────────────────────────────────────────────────────────────

--- The Port's estimate for a request no server counts: every string's bytes,
--- every string key's, four per number or boolean, over 3.2 bytes a token,
--- rounded up. A mirror of `knl_adapter`'s `estimate` (ESTIMATE_BYTES_PER_TOKEN)
--- rather than that module, which needs a transport stub to load; the
--- parallel-read case checks it against the counts the run recorded, so a
--- mirror that drifted from what the run counted fails there.
local function estimate(request)
    local bytes = 0
    local function walk(v)
        local t = type(v)
        if t == "string" then
            bytes = bytes + #v
        elseif t == "number" or t == "boolean" then
            bytes = bytes + 4
        elseif t == "table" then
            for k, child in pairs(v) do
                if type(k) == "string" then
                    bytes = bytes + #k
                end
                walk(child)
            end
        end
    end
    walk(request)
    return math.ceil(bytes / 3.2)
end

--- The room a recorded run was sized by, from its `config` event: the Port
--- declares the window and the cap the run declared, and counts by the
--- estimate.
local function room_of(config, shares)
    local recorded = config.data.room
    local port = {
        profile = function()
            return { context_window = recorded.window, max_output = recorded.max_output }
        end,
        count = function(_, request)
            return estimate(request)
        end,
    }
    return policy.room({ port = port, result_share = shares.result, beat_share = shares.beat })
end

--- The fixture's event at `seq`.
local function at(fixture, seq)
    for _, ev in ipairs(fixture.events) do
        if ev.seq == seq then
            return ev
        end
    end
    error("no event at seq " .. seq .. " in " .. fixture.source)
end

--- The first fixture event of `kind`.
local function first_of(fixture, kind)
    for _, ev in ipairs(fixture.events) do
        if ev.kind == kind then
            return ev
        end
    end
    error("no " .. kind .. " event in " .. fixture.source)
end

--- Append the fixture's events with `from <= seq <= to` to `session`, as
--- they were written: kind, meta and data (the session stamps its own seq).
local function load_prefix(session, fixture, from, to)
    for _, ev in ipairs(fixture.events) do
        if ev.seq >= from and ev.seq <= to then
            session:append({ kind = ev.kind, meta = ev.meta, data = ev.data })
        end
    end
    return session
end

--- The seq of the first event of each beat after `after`, in order.
local function beat_starts(fixture, after)
    local seen, out = {}, {}
    for _, ev in ipairs(fixture.events) do
        local id = ev.meta ~= nil and ev.meta.beat or nil
        if ev.seq > after and id ~= nil and not seen[id] then
            seen[id] = true
            out[#out + 1] = ev.seq
        end
    end
    return out
end

--- The seq of the first event a beat wrote.
local function first_beat_seq(fixture)
    for _, ev in ipairs(fixture.events) do
        if ev.meta ~= nil and ev.meta.beat ~= nil then
            return ev.seq
        end
    end
    error("no beat in " .. fixture.source)
end

-- ─────────────────────────────────────────────────────────────────────────────
-- a — six reads in one beat, and the beat's budget
-- ─────────────────────────────────────────────────────────────────────────────

describe("replay: parallel reads over one beat's budget (policy.beat_cap)", function()
    local fixture = require("policy.spec.replay.parallel_read")
    local room = room_of(first_of(fixture, "config"), { result = 0.3, beat = 0.5 })
    local opened = first_beat_seq(fixture)
    local response = first_of(fixture, "llm_response")
    local beat = response.meta.beat

    --- The recorded tool_results of the beat, in order, and the path each
    --- call asked for.
    local function recorded()
        local path_of = {}
        for _, block in ipairs(response.data.content) do
            path_of[block.id] = block.input.path
        end
        local out = {}
        for _, ev in ipairs(fixture.events) do
            if ev.kind == "tool_result" and ev.meta.beat == beat then
                out[#out + 1] = { call_id = ev.data.call_id, path = path_of[ev.data.call_id], result = ev.data.result }
            end
        end
        return out
    end

    --- The beat again: the prefix before it in a session, the recorded
    --- answer as the llm's, `fs_read` answering what the file answered in
    --- the run, under `beat_cap` over the run's room.
    local function replay()
        local s = load_prefix(support.session(), fixture, 1, opened - 1)
        local id_of = {}
        for _, block in ipairs(response.data.content) do
            id_of[block.input.path] = block.id
        end
        local tools = policy.beat_cap({ room = room })(s)({
            fs_read = {
                description = "fs_read",
                input_schema = { type = "object" },
                handler = function(args)
                    return fixture.answers[id_of[args.path]].result
                end,
            },
        })
        local answer = support.answer(response.data.content, response.data.stop_reason)
        kernel.beat(s, kernel.device({ llm = support.always(answer), tools = tools }))
        local out = {}
        for _, ev in ipairs(s:events()) do
            if ev.kind == "tool_result" then
                out[#out + 1] = { call_id = ev.data.call_id, result = ev.data.result }
            end
        end
        return out
    end

    it("is sized by the run's room: a beat may take 2976 tokens", function()
        expect(room.beat_budget).to.be(2976)
        expect(room.beat_budget).to.be(first_of(fixture, "config").data.room.beat_budget)
        expect(room.result_limit).to.be(first_of(fixture, "config").data.room.result_limit)
    end)

    it("counts each answer as the run did", function()
        -- The refusals name what the refused read would have cost; the
        -- answer behind each (from the same file read again, or sized from
        -- the refusal) has to count to that number for the replay to be one.
        for _, rec in ipairs(recorded()) do
            if rec.result.reason == "beat_budget" then
                local answer = fixture.answers[rec.call_id].result
                expect(room:count_text(std.json.encode(answer))).to.be(rec.result.tokens)
            end
        end
    end)

    it("refuses the reads the run refused, with the numbers it recorded, and lets the rest through", function()
        local want = recorded()
        local got = replay()
        expect(#got).to.be(#want)
        expect(#want).to.be(6)
        local refused = {}
        for i, rec in ipairs(want) do
            expect(got[i].call_id).to.be(rec.call_id)
            if rec.result.reason == "beat_budget" then
                refused[#refused + 1] = rec.path
                expect(got[i].result.ok).to.be(false)
                expect(got[i].result.reason).to.be("beat_budget")
                expect(got[i].result.tokens).to.be(rec.result.tokens)
                expect(got[i].result.used).to.be(rec.result.used)
                expect(got[i].result.limit).to.be(rec.result.limit)
                expect(got[i].result.error).to.be(rec.result.error)
            else
                -- Let through: the handler's own answer, recorded as it was.
                expect(got[i].result.reason).to.be(nil)
                expect(got[i].result.content).to.be(rec.result.content)
            end
        end
        -- Which ones: the second and third ask cross the line on their own
        -- after the first; the two smaller files after them still fit; the
        -- last one does not.
        expect(table.concat(refused, " ")).to.be("/repo/src/beta.lua /repo/src/gamma.lua /repo/src/zeta.lua")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- b — the summarising request, and the mark an empty summary leaves
-- ─────────────────────────────────────────────────────────────────────────────

describe("replay: the summarising request and an empty summary (policy.compact)", function()
    local fixture = require("policy.spec.replay.empty_summary")
    local room = room_of(first_of(fixture, "config"), { result = 0.3, beat = 0.5 })
    local fold = policy.window({ room = room, keep_seed = true, from_summary = true })
    local due, summarise = policy.compact({ at = 0.6, min_beats = 2, fold = fold })
    -- The run's first summary, and the summarising beat before it.
    local summary = first_of(fixture, "summary")
    local asked = summary.seq - 2

    it("is due where the run asked for its first summary, for the reason it recorded", function()
        expect(at(fixture, asked).kind).to.be("llm_request")
        local s = load_prefix(support.session(), fixture, 1, asked - 1)
        expect(due(s)).to.be(summary.meta.reason)
        expect(due(s)).to.be("share")
    end)

    it("sends the history as prose: no tool block, no tools, the prompt last", function()
        local s = load_prefix(support.session(), fixture, 1, asked - 1)
        local request = summarise(s:events(), kernel.device({ llm = support.always(support.text("")) }))
        expect(request.tools).to.be(nil)
        expect(#request.messages >= 3).to.be(true)
        local calls = 0
        for _, message in ipairs(request.messages) do
            -- A string, so no `tool_use` / `tool_result` block can be in it.
            expect(type(message.content)).to.be("string")
            if message.content:find("[called fs_read with ", 1, true) then
                calls = calls + 1
            end
        end
        -- The recorded calls are there, written as lines.
        expect(calls > 0).to.be(true)
        local last = request.messages[#request.messages]
        expect(last.role).to.be("user")
        expect(last.content).to.be(shared.DEFAULT_COMPACT_PROMPT)
    end)

    it("after a compact_skipped mark, is not due until min_beats beats follow it", function()
        -- The summarising beat as recorded, then the mark a loop appends in
        -- the summary's place when the answer is empty (reconstructed: this
        -- run's summary was written; see the fixture's header).
        local s = load_prefix(support.session(), fixture, 1, summary.seq - 1)
        -- Without the mark, the summarising beat's own request (which dropped
        -- a beat) makes it due again before the very next beat.
        expect(due(s)).to.be("dropped")
        s:append({
            kind = shared.COMPACT_SKIPPED_KIND,
            meta = { label = "compact", reason = summary.meta.reason },
            data = { why = "empty" },
        })
        expect(due(s)).to.be(nil)
        -- The first recorded beat after the summary: one of two.
        local second_beat = beat_starts(fixture, summary.seq)[2]
        load_prefix(s, fixture, summary.seq + 1, second_beat - 1)
        expect(due(s)).to.be(nil)
        -- The second: the recorded request took 0.6 of the room and more.
        load_prefix(s, fixture, second_beat, fixture.events[#fixture.events].seq)
        expect(due(s)).to.be("share")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- c — a seed older than the ledger
-- ─────────────────────────────────────────────────────────────────────────────

describe("replay: a seed that says FAILING under a ledger that says passed (policy.window)", function()
    local fixture = require("policy.spec.replay.stale_seed")
    local room = room_of(first_of(fixture, "config"), { result = 0.3, beat = 0.5 })
    local fold = policy.window({ room = room, keep_seed = true, from_summary = true })
    local last = fixture.events[#fixture.events].seq

    --- The recorded log, and the next summary appended as the loop appends
    --- one: the model's text, and `policy.ledger`'s reading of the log.
    local function compacted()
        local s = load_prefix(support.session(), fixture, 1, last)
        local ledger = policy.ledger({ track = { "path" }, kinds = { "verify" } })(s)
        s:append({
            kind = "summary",
            meta = { label = "compact", reason = "share" },
            data = { content = "The heading now uses titlecase; the verify passes.", ledger = ledger },
        })
        return s, ledger
    end

    it("reads the ledger off the recorded log: three verifies, the last one green", function()
        local _, ledger = compacted()
        expect(ledger.checks.verify.count).to.be(3)
        expect(ledger.checks.verify.passed).to.be(1)
        expect(ledger.checks.verify.last_ok).to.be(true)
        expect(ledger.summaries).to.be(3)
    end)

    it("renders the summary after the seed, saying the ledger is newer than the task message", function()
        local s = compacted()
        local request = fold(s:events(), kernel.device({ llm = support.always(support.text("")) }))
        expect(#request.messages).to.be(2)
        local seed, rendered = request.messages[1].content, request.messages[2].content
        expect(seed:find("## Current build status: FAILING", 1, true)).to.exist()
        expect(request.messages[2].role).to.be("user")
        expect(rendered:sub(1, #shared.SUMMARY_PREFIX)).to.be(shared.SUMMARY_PREFIX)
        expect(shared.SUMMARY_PREFIX:find("the ledger below is newer", 1, true)).to.exist()
        expect(rendered:find("\nverify: 3 run, 1 passed, the last one passed", 1, true)).to.exist()
    end)

    it("renders a recorded summary's ledger as it was recorded: the last verify failed then", function()
        -- The run's own last summary, before the verify turned green.
        local recorded
        for _, ev in ipairs(fixture.events) do
            if ev.kind == "summary" then
                recorded = ev
            end
        end
        local s = load_prefix(support.session(), fixture, 1, recorded.seq)
        local request = fold(s:events(), kernel.device({ llm = support.always(support.text("")) }))
        local rendered = request.messages[2].content
        expect(rendered:sub(1, #shared.SUMMARY_PREFIX)).to.be(shared.SUMMARY_PREFIX)
        expect(rendered:find("\nverify: 2 run, 0 passed, the last one failed", 1, true)).to.exist()
    end)
end)
