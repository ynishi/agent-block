-- fork_spec.lua — mlua-lspec tests for `coding.fork`: a new run that copies
-- an earlier one's record up to the end of a beat, puts the files back as they
-- were then under a fresh directory, and runs the loop on from there.
--
-- Run via:
--   just test-lua fork_spec
--
-- Unlike coding_spec, this file runs the loop itself — the parent through
-- `coding.run`, the child through `coding.fork` — on the shared fake kernel
-- (knl/spec/fake_bridge.lua, with the kernel's own boundary and ledger writes
-- on, so the kinds a fork must not copy are in the parent's log), an
-- in-memory file system behind `std.fs`, a verify behind `sh.exec` that is
-- green when `a.lua` reads `v3`, and a model that answers from a queue.
--
-- The parent runs four beats over one target: three that each land an edit
-- (v0 -> v1 -> v2 -> v3, so three states are recorded after a beat, beside
-- the baseline) and one that declares on the green verify. The child forks
-- at the second.
--
-- What this proves:
--   1 the child's record: `forked_from` first, then the parent's events up to
--     and including the last one of beat 2, in order and unchanged, without
--     the kernel's own kinds; then the child's verify, its note, its config
--     and the restored state recorded under beat 2 at the child's paths;
--   2 the files: the target under the child's repo holds beat 2's content,
--     and the parent's file is where the parent left it;
--   3 the parent's record is unchanged by the fork, whether it was handed in
--     as its events or as its session;
--   4 the budget: the child's grant is its own (iters x turns), and the
--     parent's spend up to the cut is on `forked_from` as a number only;
--   5 policy.lineage answers the child's `forked_from` and nil for the parent;
--     and `coding.restore` over the child's log re-roots from the child's
--     repo — the last `config`;
--   6 the copied edits count: a child that declares at once on a green
--     verify over restored edits converges;
--   7 the refusals, by name and before a session opens: a beat not in the
--     log, "baseline", the parent's repo (as written, with a trailing slash,
--     through `..`, and emptied so the comparison is what catches it), a
--     directory inside it, a directory that already holds a file, an empty
--     repo holding a symlink to the parent's file (nothing written), a `spec`,
--     an array with no `parent`, a `parent` that is not the session handed
--     in, and a parent run that recorded no state of the files;
--   8 what is taken: a sibling whose name starts with the parent's
--     (`/parent2`), and a nested directory not there yet.
--
-- The fake disk folds `.` and `..` and follows the links a case plants, which
-- is enough for a file symlink inside the child's repo; symlinks to
-- directories, and a directory around the parent's, are proved against the
-- real file system in crates/agent-block/tests/e2e_coding.rs.

local describe, it, expect = lust.describe, lust.it, lust.expect

local fake = require("knl.spec.fake_bridge").install({ writes = { lifecycle = true, ledger = true } })
local json = require("knl.spec.json_stub")

-- ─────────────────────────────────────────────────────────────────────────────
-- The host, in memory
-- ─────────────────────────────────────────────────────────────────────────────

local files = {}
-- Directories made by `std.fs.mkdir`; a directory also exists while a file
-- sits under it.
local dirs = {}

--- The path with `.` and `..` folded — all that resolving does on a disk
--- with no links.
local function folded(p)
    local out = {}
    for part in p:gmatch("[^/]+") do
        if part == ".." then
            out[#out] = nil
        elseif part ~= "." then
            out[#out + 1] = part
        end
    end
    return "/" .. table.concat(out, "/")
end

local function under(dir, path)
    return path:sub(1, #dir + 1) == dir .. "/"
end

-- Symlinks: `links[from] = to`. Only `exists` and `std.path.absolute`
-- follow them, which is all the fork's check reads; `walk` lists files, not
-- links, as the host's does.
local links = {}

local function real(p)
    p = folded(p)
    for _ = 1, 8 do
        local hit = false
        for from, to in pairs(links) do
            if p == from or under(from, p) then
                p = folded(to .. p:sub(#from + 1))
                hit = true
                break
            end
        end
        if not hit then
            break
        end
    end
    return p
end

local function exists(p)
    p = real(p)
    if dirs[p] or files[p] ~= nil then
        return true
    end
    for path in pairs(files) do
        if under(p, path) then
            return true
        end
    end
    return false
end

local function fs_tool(op, lock)
    local allowed = {}
    for _, path in ipairs(lock) do
        allowed[path] = true
    end
    local handler
    if op == "read" then
        handler = function(input)
            if not allowed[input.path] then
                return { ok = false, reason = "path_locked" }
            end
            return { ok = files[input.path] ~= nil, content = files[input.path] }
        end
    else
        handler = function(input)
            local path = input.path
            if not allowed[path] then
                return { ok = false, reason = "path_locked" }
            end
            local content = files[path]
            if content == nil then
                return { ok = false, reason = "not_found" }
            end
            for _, e in ipairs(input.edits or {}) do
                local at = content:find(e.search, 1, true)
                if at == nil then
                    return { ok = false, reason = "no_match" }
                end
                content = content:sub(1, at - 1) .. e.replace .. content:sub(at + #e.search)
            end
            files[path] = content
            return { ok = true }
        end
    end
    return { name = "fs_" .. op, description = op, input_schema = { type = "object" }, handler = handler }
end

_G.std = {
    json = { encode = json.encode },
    fs = {
        tool_specs = function(o)
            return { fs_tool(o.allowed[1], o.path_lock) }
        end,
        read_versioned = function(path)
            local content = files[path]
            if content == nil then
                error("not found: " .. path)
            end
            return { content = content, version = "ver-" .. content }
        end,
        write = function(path, content)
            files[path] = content
        end,
        exists = exists,
        is_dir = function(p)
            return files[folded(p)] == nil and exists(p)
        end,
        mkdir = function(p)
            dirs[folded(p)] = true
            return true
        end,
        walk = function(p)
            local out = {}
            for path in pairs(files) do
                if under(folded(p), path) then
                    out[#out + 1] = path
                end
            end
            table.sort(out)
            return out
        end,
    },
    path = {
        absolute = function(p)
            if not exists(p) then
                error("not found: " .. p)
            end
            return real(p)
        end,
    },
}

_G.sh = {
    exec = function(_cmd, o)
        local content = files[o.cwd .. "/a.lua"]
        local green = content == "v3"
        return { ok = true, code = green and 0 or 1, stdout = "", stderr = "a.lua reads " .. tostring(content) }
    end,
}

local coding = require("coding")
local policy = require("policy")

-- ─────────────────────────────────────────────────────────────────────────────
-- The model
-- ─────────────────────────────────────────────────────────────────────────────

local function usage()
    return { input_tokens = 1, output_tokens = 1, thinking_tokens = 0 }
end

local function text(body)
    return { status = "ok", content = { { type = "text", text = body } }, usage = usage(), stop_reason = "end_turn" }
end

local function edit(id, path, from, to)
    return {
        status = "ok",
        content = {
            {
                type = "tool_use",
                id = id,
                name = "fs_search_replace",
                input = { path = path, edits = { { search = from, replace = to } } },
            },
        },
        usage = usage(),
        stop_reason = "tool_use",
    }
end

--- A port whose calls answer from `answers`, in order.
local function port_of(answers)
    local at = 0
    return {
        profile = function()
            return { context_window = 32768, max_output = 1024 }
        end,
        count = function()
            return 10
        end,
        open = function()
            return function(_request)
                at = at + 1
                local answer = answers[at]
                assert(answer ~= nil, "the model was called more often than the case queued")
                return answer
            end
        end,
    }
end

local function opts_of(repo, answers, extra)
    local o = {
        targets = { "a.lua" },
        verify = "check a.lua",
        repo = repo,
        llm = { port = port_of(answers), conf = { model = "m", timeout = 10, max_tokens = 1024 } },
        iters = 5,
        turns = 2,
        timeout = 10,
        compact = false,
    }
    for k, v in pairs(extra or {}) do
        o[k] = v
    end
    return o
end

-- ─────────────────────────────────────────────────────────────────────────────
-- Reading the record
-- ─────────────────────────────────────────────────────────────────────────────

local KERNEL = require("knl.spec.fake_bridge").KERNEL_KINDS

local function events_of(id)
    return fake.sessions[id]:events()
end

--- The parent: three edits and a declaration, from v0 in /parent.
local function run_parent(extra)
    files["/parent/a.lua"] = "v0"
    local o = opts_of("/parent", {
        edit("c1", "/parent/a.lua", "v0", "v1"),
        edit("c2", "/parent/a.lua", "v1", "v2"),
        edit("c3", "/parent/a.lua", "v2", "v3"),
        text("done"),
    }, extra)
    o.spec = "Make a.lua read v3."
    local result = coding.run(o)
    return result, events_of(result.session)
end

--- The beats that recorded a state, in order, "baseline" first.
local function state_beats(events)
    local out = {}
    for _, point in ipairs(policy.checkpoints(events)) do
        out[#out + 1] = point.beat
    end
    return out
end

--- The events up to and including the last one of `beat`, less the kernel's.
local function copied_prefix(events, beat)
    local cut
    for i, ev in ipairs(events) do
        if ev.meta ~= nil and ev.meta.beat == beat then
            cut = i
        end
    end
    local out = {}
    for i = 1, cut do
        if not KERNEL[events[i].kind] then
            out[#out + 1] = events[i]
        end
    end
    return out, events[cut]
end

local function index_of(events, kind)
    for i, ev in ipairs(events) do
        if ev.kind == kind then
            return i
        end
    end
end

describe("coding.fork — the same point of a run, continued", function()
    local parent, parent_events = run_parent()
    local beats = state_beats(parent_events)
    local b2 = beats[3]
    local before = json.encode(parent_events)

    files["/child/a.lua"] = nil
    local child = coding.fork(
        parent_events,
        b2,
        opts_of("/child", { text("thinking it over") }, { iters = 1, parent = parent.session, reason = "try" })
    )
    local child_events = events_of(child.session)

    it("the parent ran as the case says: four beats, three states after an edit, green", function()
        expect(parent.ok).to.be(true)
        expect(#beats).to.be(4)
        expect(beats[1]).to.be("baseline")
    end)

    it(
        "opens with forked_from, then copies the parent's record through beat 2, unchanged, less the kernel's kinds",
        function()
            local at = index_of(child_events, "forked_from")
            for i = 1, at - 1 do
                expect(KERNEL[child_events[i].kind]).to.be(true)
            end
            local prefix = copied_prefix(parent_events, b2)
            for i, ev in ipairs(prefix) do
                local got = child_events[at + i]
                expect(got.kind).to.be(ev.kind)
                expect(json.encode(got.meta)).to.be(json.encode(ev.meta))
                expect(json.encode(got.data)).to.be(json.encode(ev.data))
            end
            -- Nothing of beat 3 came along.
            for _, ev in ipairs(child_events) do
                expect(ev.meta ~= nil and ev.meta.beat == beats[4]).to.be(false)
            end
            -- Then the child's own: the verify, the note, its config, and the
            -- restored state under beat 2, at the child's paths.
            local after = at + #prefix
            expect(child_events[after + 1].kind).to.be("verify")
            expect(child_events[after + 2].kind).to.be("msg_user")
            expect(child_events[after + 2].meta.label).to.be("fork")
            expect(child_events[after + 2].data.content:find("/parent is the same file under /child", 1, true) ~= nil).to.be(
                true
            )
            expect(child_events[after + 2].data.content:find("The verify fails on these files", 1, true) ~= nil).to.be(
                true
            )
            expect(child_events[after + 3].kind).to.be("config")
            expect(child_events[after + 3].data.values.repo.value).to.be("/child")
            local state = child_events[after + 4]
            expect(state.kind).to.be("checkpoint")
            expect(state.meta.beat).to.be(b2)
            expect(state.data.files["/child/a.lua"]).to.be("ver-v2")
        end
    )

    it("puts beat 2's files under the child's repo, and leaves the parent's where they were", function()
        expect(files["/child/a.lua"]).to.be("v2")
        expect(files["/parent/a.lua"]).to.be("v3")
    end)

    it("leaves the parent's record as it was", function()
        expect(json.encode(events_of(parent.session))).to.be(before)
    end)

    it("grants the child its own budget, and records the parent's spend as a number only", function()
        local granted
        for _, ev in ipairs(child_events) do
            if ev.kind == "budget_granted" then
                granted = ev.data.amount
            end
        end
        expect(granted).to.be(1 * 2)
        local _, cut = copied_prefix(parent_events, b2)
        local spent = 0
        for _, ev in ipairs(parent_events) do
            if ev.seq > cut.seq then
                break
            end
            if ev.kind == "budget_reserved" or ev.kind == "budget_spent" then
                spent = spent + ev.data.amount
            end
        end
        local from = policy.lineage(child_events)
        expect(from.spent.amount).to.be(spent)
        expect(spent > 0).to.be(true)
        expect(from.spent.tag).to.be("beats")
        -- The child ran on its own grant: one iteration, and out.
        expect(child.ok).to.be(false)
        expect(child.failure_reason).to.be("max_iters")
    end)

    it("policy.lineage answers where the record came from, and nil for a run that was not forked", function()
        local from = policy.lineage(child_events)
        local _, cut = copied_prefix(parent_events, b2)
        expect(from.session).to.be(parent.session)
        expect(from.beat).to.be(b2)
        expect(from.upto_seq).to.be(cut.seq)
        expect(from.reason).to.be("try")
        expect(policy.lineage(parent_events)).to.be(nil)
        expect(policy.lineage(fake.sessions[child.session])).to.equal(from)
    end)

    it("restore over the child's log re-roots from the child's repo, the last config", function()
        local r = coding.restore(child_events, b2, "/elsewhere")
        expect(r.restored).to.equal({ "/elsewhere/a.lua" })
        expect(files["/elsewhere/a.lua"]).to.be("v2")
    end)

    it("counts the copied edits: a child that declares at once on a green verify converges", function()
        local b3 = beats[4]
        files["/green/a.lua"] = nil
        local r =
            coding.fork(fake.sessions[parent.session], b3, opts_of("/green", { text("already done") }, { iters = 1 }))
        expect(files["/green/a.lua"]).to.be("v3")
        expect(r.ok).to.be(true)
        expect(r.iters).to.be(1)
        expect(json.encode(events_of(parent.session))).to.be(before)
    end)
end)

describe("coding.fork — the directories it takes", function()
    local parent, parent_events = run_parent()
    local b2 = state_beats(parent_events)[3]

    local function forked(repo)
        return coding.fork(parent_events, b2, opts_of(repo, { text("x") }, { iters = 1, parent = parent.session }))
    end

    it("a sibling whose name starts with the parent's: /parent2 is not inside /parent", function()
        local r = forked("/parent2")
        expect(files["/parent2/a.lua"]).to.be("v2")
        expect(r.config.values.repo.value).to.be("/parent2")
    end)

    it("a nested directory not there yet, made for the run", function()
        local r = forked("/new/nested/repo/")
        expect(dirs["/new/nested/repo"]).to.be(true)
        expect(files["/new/nested/repo/a.lua"]).to.be("v2")
        expect(r.config.values.repo.value).to.be("/new/nested/repo")
    end)

    it("the path it resolved to is the child's repo: `..` folded in the run's config", function()
        local r = forked("/new/../folded")
        expect(files["/folded/a.lua"]).to.be("v2")
        expect(r.config.values.repo.value).to.be("/folded")
    end)
end)

describe("coding.fork — what it refuses, before a session opens", function()
    local parent, parent_events = run_parent()
    local b2 = state_beats(parent_events)[3]

    local function refused(source, beat, extra)
        local opened = 0
        for _ in pairs(fake.sessions) do
            opened = opened + 1
        end
        local o = opts_of("/fresh", { text("x") }, extra)
        if o.parent == nil and type(source) == "table" and source.append == nil then
            o.parent = parent.session
        end
        if extra and extra.parent == false then
            o.parent = nil
        end
        local ok, err = pcall(coding.fork, source, beat, o)
        local after = 0
        for _ in pairs(fake.sessions) do
            after = after + 1
        end
        expect(ok).to.be(false)
        expect(after).to.be(opened)
        return tostring(err)
    end

    local function says(err, text)
        expect(err:find(text, 1, true) ~= nil).to.be(true)
    end

    it("a beat no event carries, and the baseline", function()
        says(refused(parent_events, "beat-none"), "beat beat-none is not in the log")
        says(refused(parent_events, "baseline"), '"baseline" is the state before the first beat')
    end)

    it("the parent's repo, however it is written: it holds files, so it is refused as it stands", function()
        says(refused(parent_events, b2, { repo = "/parent" }), "repo /parent already holds files (/parent/a.lua)")
        says(refused(parent_events, b2, { repo = "/parent/" }), "repo /parent already holds files")
        says(refused(parent_events, b2, { repo = "/parent/../parent" }), "repo /parent/../parent already holds files")
    end)

    it("the parent's repo emptied: the comparison catches it, as written or through `..`", function()
        local kept = files["/parent/a.lua"]
        files["/parent/a.lua"] = nil
        dirs["/parent"] = true
        local as_written = refused(parent_events, b2, { repo = "/parent/" })
        local through = refused(parent_events, b2, { repo = "/x/../parent" })
        files["/parent/a.lua"] = kept
        says(as_written, "repo /parent is the repo a run in this history edited (/parent)")
        says(through, "repo /x/../parent is the repo a run in this history edited (/parent)")
    end)

    it("a directory inside the parent's repo, not there before", function()
        says(
            refused(parent_events, b2, { repo = "/parent/sub/" }),
            "repo /parent/sub lies inside /parent, the repo a run in this history edited"
        )
    end)

    it("an empty repo holding a symlink to the parent's file: refused, nothing written", function()
        dirs["/linked"] = true
        links["/linked/a.lua"] = "/parent/a.lua"
        local kept = files["/parent/a.lua"]
        local err = refused(parent_events, b2, { repo = "/linked" })
        links["/linked/a.lua"] = nil
        says(err, "coding.fork: /linked/a.lua leads to /parent/a.lua, outside the repo /linked")
        expect(files["/parent/a.lua"]).to.be(kept)
        expect(files["/linked/a.lua"]).to.be(nil)
    end)

    it("a directory that already holds a file, whoever's it is", function()
        files["/other/notes.txt"] = "x"
        local err = refused(parent_events, b2, { repo = "/other" })
        files["/other/notes.txt"] = nil
        says(err, "repo /other already holds files (/other/notes.txt)")
    end)

    it("a spec, and no repo", function()
        says(refused(parent_events, b2, { spec = "another task" }), "`spec` is not taken")
        local o = opts_of(nil, { text("x") }, { parent = parent.session })
        local ok, err = pcall(coding.fork, parent_events, b2, o)
        expect(ok).to.be(false)
        says(tostring(err), "`repo` is required")
    end)

    it("an array that does not say whose it is, and a parent that is not the session handed in", function()
        says(refused(parent_events, b2, { parent = false }), "an array of events does not say which session it is")
        says(
            refused(fake.sessions[parent.session], b2, { parent = "sess-other" }),
            "`parent` names sess-other but the session handed in is " .. parent.session
        )
    end)

    it("a parent run that recorded no state of the files", function()
        local off, off_events = run_parent({ checkpoint = false })
        local beat
        for _, ev in ipairs(off_events) do
            if ev.meta ~= nil and ev.meta.beat ~= nil then
                beat = ev.meta.beat
                break
            end
        end
        says(
            refused(off_events, beat, { parent = off.session }),
            "records no state of the files at or before beat " .. beat
        )
    end)
end)
