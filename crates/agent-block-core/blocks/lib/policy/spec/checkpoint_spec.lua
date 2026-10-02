-- checkpoint_spec.lua — mlua-lspec unit tests for the two readings of the
-- state of the files: `policy.checkpoints` (every recorded state, by version)
-- and `policy.checkpoint_at` (one state, by content).
--
-- Run via:
--   just test-lua checkpoint_spec
--
-- The events are built by hand, in the shapes the loop appends
-- (`policy.shapes.checkpoint` / `checkpoint_blob`), on the fake kernel; the
-- loop that appends them is exercised end to end (tests/e2e_coding.rs).
--
-- What this proves:
--   1 checkpoints: every `checkpoint` in log order, the beat each follows
--     (`meta.beat`, or "baseline" for the one with none), its seq, path ->
--     version, and `missing`; a log with none answers an empty list;
--   2 checkpoint_at: the state after a named beat, "baseline", and the latest
--     when no beat is named, each with contents resolved from the blobs — a
--     version written once and named by two checkpoints resolves both times;
--     a beat with no state answers nil;
--   3 missing: a target recorded absent is listed and has no content, and the
--     others in that state still resolve;
--   4 a version no blob carries is raised, naming the path and the version,
--     rather than answered as a state with a file left out;
--   5 a truncated read is refused like every reader's, and the events of a
--     log (an export) are taken in place of a session;
--   6 the shapes the loop appends are published and closed, and neither
--     reader appends;
--   7 lineage: the first `forked_from` in a log (its own, ahead of the ones
--     a fork of a fork copies in), nil for a log with none, from a session or
--     its events, and the shape closed.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local policy = require("policy")
local check = require("lshape.check")

-- ─────────────────────────────────────────────────────────────────────────────
-- A recorded run: the state before the first beat, a beat that edited a.lua,
-- a beat that landed no edit (no state), and a beat that put a.lua back and
-- deleted b.lua.
-- ─────────────────────────────────────────────────────────────────────────────

local A0, A1, B0 = "local a = 0\n", "local a = 1\n", "local b = 0\n"

local function blob(s, version, content)
    s:append({
        kind = "checkpoint_blob",
        meta = { label = "checkpoint" },
        data = { version = version, content = content },
    })
end

local function point(s, beat, files, missing)
    local data = { files = files }
    if missing then
        data.missing = missing
    end
    s:append({ kind = "checkpoint", meta = { label = "checkpoint", beat = beat }, data = data })
end

local function recorded()
    local s = support.seed(support.session(), "the task")
    blob(s, "va0", A0)
    blob(s, "vb0", B0)
    point(s, nil, { ["/repo/a.lua"] = "va0", ["/repo/b.lua"] = "vb0" }, { "/repo/c.lua" })
    s:append({ kind = "tool_call", meta = { beat = "b1" }, data = { call_id = "c1", name = "fs_write", args = {} } })
    blob(s, "va1", A1)
    point(s, "b1", { ["/repo/a.lua"] = "va1", ["/repo/b.lua"] = "vb0" }, { "/repo/c.lua" })
    s:append({ kind = "tool_call", meta = { beat = "b2" }, data = { call_id = "c2", name = "fs_read", args = {} } })
    -- b3 put a.lua back to what it was, so no blob is written for it, and
    -- b.lua is gone.
    s:append({ kind = "tool_call", meta = { beat = "b3" }, data = { call_id = "c3", name = "fs_write", args = {} } })
    point(s, "b3", { ["/repo/a.lua"] = "va0" }, { "/repo/b.lua", "/repo/c.lua" })
    return s
end

describe("policy.checkpoints — every recorded state, by version", function()
    it("lists them in log order, with the beat each follows and baseline for the one before the first", function()
        local list = policy.checkpoints(recorded())
        expect(#list).to.be(3)
        expect(list[1].beat).to.be("baseline")
        expect(list[2].beat).to.be("b1")
        expect(list[3].beat).to.be("b3")
        expect(list[1].seq < list[2].seq and list[2].seq < list[3].seq).to.be(true)
        expect(list[2].files["/repo/a.lua"]).to.be("va1")
        expect(list[2].files["/repo/b.lua"]).to.be("vb0")
        expect(list[3].missing).to.equal({ "/repo/b.lua", "/repo/c.lua" })
    end)

    it("answers an empty list for a log that recorded none", function()
        local list = policy.checkpoints(support.seed(support.session(), "x"))
        expect(#list).to.be(0)
    end)
end)

describe("policy.checkpoint_at — one state, with its contents", function()
    it("resolves the state after a named beat from the blobs", function()
        local state = policy.checkpoint_at(recorded(), "b1")
        expect(state.beat).to.be("b1")
        expect(state.files["/repo/a.lua"]).to.be(A1)
        -- b.lua's version was written once, before the first beat, and is
        -- named again here: it resolves all the same.
        expect(state.files["/repo/b.lua"]).to.be(B0)
    end)

    it("resolves baseline, and the latest when no beat is named", function()
        local s = recorded()
        local first = policy.checkpoint_at(s, "baseline")
        expect(first.files["/repo/a.lua"]).to.be(A0)
        local latest = policy.checkpoint_at(s)
        expect(latest.beat).to.be("b3")
        -- A content the run came back to is resolved from its first blob.
        expect(latest.files["/repo/a.lua"]).to.be(A0)
    end)

    it("answers nil for a beat with no recorded state, and for a log with none", function()
        expect(policy.checkpoint_at(recorded(), "b2")).to.be(nil)
        expect(policy.checkpoint_at(recorded(), "nope")).to.be(nil)
        expect(policy.checkpoint_at(support.seed(support.session(), "x"))).to.be(nil)
    end)

    it("lists a target recorded absent and gives it no content, and resolves the rest", function()
        local state = policy.checkpoint_at(recorded(), "b3")
        expect(state.missing).to.equal({ "/repo/b.lua", "/repo/c.lua" })
        expect(state.files["/repo/b.lua"]).to.be(nil)
        expect(state.files["/repo/a.lua"]).to.be(A0)
    end)

    it("raises on a version no blob carries, naming it, rather than leaving the file out", function()
        local s = support.seed(support.session(), "x")
        point(s, "b1", { ["/repo/a.lua"] = "gone" })
        local ok, err = pcall(policy.checkpoint_at, s, "b1")
        expect(ok).to.be(false)
        expect(tostring(err):find("/repo/a.lua at version gone", 1, true) ~= nil).to.be(true)
        expect(tostring(err):find("the log is not whole", 1, true) ~= nil).to.be(true)
    end)

    it("refuses a beat that is not a string", function()
        local ok, err = pcall(policy.checkpoint_at, recorded(), 3)
        expect(ok).to.be(false)
        expect(tostring(err):find("beat must be", 1, true) ~= nil).to.be(true)
    end)
end)

describe("checkpoint readers — what they read", function()
    it("refuses a truncated read, as every reader of the whole log does", function()
        local s = support.truncate(recorded())
        for _, read in ipairs({ policy.checkpoints, policy.checkpoint_at }) do
            local ok, err = pcall(read, s)
            expect(ok).to.be(false)
            expect(tostring(err):find("longer than one read", 1, true) ~= nil).to.be(true)
        end
    end)

    it("takes the events of a log in place of a session — a finished run's export", function()
        local events = recorded():events()
        expect(#policy.checkpoints(events)).to.be(3)
        expect(policy.checkpoint_at(events, "b1").files["/repo/a.lua"]).to.be(A1)
    end)

    it("refuses something that is neither", function()
        local ok, err = pcall(policy.checkpoints, "s-1")
        expect(ok).to.be(false)
        expect(tostring(err):find("takes a knl session, or the events of one", 1, true) ~= nil).to.be(true)
    end)

    it("appends nothing", function()
        local s = recorded()
        local before = #s:events()
        policy.checkpoints(s)
        policy.checkpoint_at(s, "b1")
        expect(#s:events()).to.be(before)
    end)
end)

describe("policy.shapes — what the loop appends", function()
    it("publishes the two closed shapes", function()
        local cp, b = policy.shapes.checkpoint, policy.shapes.checkpoint_blob
        expect(check.check({ files = { ["/r/a"] = "v" } }, cp)).to.be(true)
        expect(check.check({ files = {}, missing = { "/r/a" } }, cp)).to.be(true)
        expect(check.check({ files = {}, beat = "b1" }, cp)).to.be(false)
        expect(check.check({ version = "v", content = "" }, b)).to.be(true)
        expect(check.check({ version = "v" }, b)).to.be(false)
    end)
end)

describe("policy.lineage — where a forked record came from", function()
    local own = { session = "s-child", upto_seq = 12, beat = "b2", reason = "another model" }
    local copied = { session = "s-root", upto_seq = 7, beat = "b1" }

    it("answers the first forked_from: the log's own, not the ones copied in after it", function()
        local s = support.session()
        s:append({ kind = "forked_from", data = own })
        s:append({ kind = "forked_from", data = copied })
        s:append({ kind = "msg_user", data = { content = "the copied seed" } })
        expect(policy.lineage(s)).to.equal(own)
        expect(policy.lineage(s:events())).to.equal(own)
        expect(#s:events()).to.be(3)
    end)

    it("answers nil for a log that was not forked", function()
        expect(policy.lineage(support.seed(support.session(), "a task"))).to.be(nil)
        expect(policy.lineage({})).to.be(nil)
    end)

    it("publishes the shape closed, with the parent's spend optional", function()
        local f = policy.shapes.forked_from
        expect(check.check(own, f)).to.be(true)
        expect(check.check({ session = "s", upto_seq = 3, beat = "b", spent = { amount = 2, tag = "beats" } }, f)).to.be(
            true
        )
        expect(check.check({ session = "s", beat = "b" }, f)).to.be(false)
        expect(check.check({ session = "s", upto_seq = 3, beat = "b", parent_budget = 9 }, f)).to.be(false)
    end)
end)
