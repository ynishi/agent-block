-- compile_loop_obs_test.lua — mlua-lspec unit tests for edit observability.
--
-- Run via:
--   just test-lua compile_loop_obs_test   # this file
--   just test-lua                         # every spec fixture
--
-- Covers summarize_edits, which turns the `edits` array of an fs_edit call into
-- the position and magnitude fields of the tool_use / tool_use_fail obs lines,
-- and the two call sites that emit them. Before it the log recorded which files
-- an iteration touched but not where, so "did the fix land on the lines it was
-- pointed at" had no answer in the data and could only be argued. These tests
-- pin both the encoding and the fact that it reaches the log.
--
-- The runtime injects std / log / tool as globals; the harness has none of
-- them. `tool_loop` is replaced through package.loaded so a test can drive the
-- edit handler directly: run_loop hands its tool specs to tool_loop.run, which
-- is the only place the handler is reachable from outside.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Capture and stubs. Installed before compile_loop is required.
-- ─────────────────────────────────────────────────────────────────────────────

local log_lines = {}

local function reset_log()
    for i = #log_lines, 1, -1 do
        log_lines[i] = nil
    end
end

-- Return the first captured line containing every given substring.
local function find_line(...)
    local needles = { ... }
    for _, line in ipairs(log_lines) do
        local hit = true
        for _, needle in ipairs(needles) do
            if not line:find(needle, 1, true) then
                hit = false
                break
            end
        end
        if hit then
            return line
        end
    end
    return nil
end

if not log then
    log = {
        info = function(msg)
            table.insert(log_lines, tostring(msg))
        end,
        warn = function() end,
        debug = function() end,
        error = function() end,
    }
end

if not tool then
    tool = { register = function() end }
end

-- What the stubbed fs_edit handler returns; a test sets this before running.
local fs_edit_result = { ok = true, applied = 1, version = "v2" }

if not std then
    std = {
        env = {
            get = function(name)
                -- obs_event is a no-op at mode "off", which is the default.
                if name == "AGENT_BLOCK_LLM_DUMP" then
                    return "meta"
                end
                return nil
            end,
            get_or = function(_name, default)
                return default
            end,
        },
        json = {
            encode = function(v)
                return '"' .. tostring(v) .. '"'
            end,
        },
        fs = {
            metadata = function(_path)
                return nil
            end,
            -- Shape only: the real bridge validates and writes, which these
            -- tests neither need nor should depend on. What is under test is
            -- what compile_loop records about the call, not the edit itself.
            tool_specs = function(_opts)
                return {
                    {
                        name = "fs_edit",
                        description = "stub (compile_loop_obs_test)",
                        input_schema = { type = "object" },
                        handler = function(_input)
                            return fs_edit_result
                        end,
                    },
                }
            end,
        },
    }
end

-- Drives one tool_loop turn. A test sets this to the fs_edit input it wants
-- dispatched; nil means the iteration makes no tool call at all.
local pending_edit_input = nil

package.loaded["tool_loop"] = {
    run = function(args)
        if pending_edit_input then
            for _, spec in ipairs(args.tools or {}) do
                if spec.name == "fs_edit" then
                    spec.handler(pending_edit_input)
                end
            end
        end
        return { ok = true, content = "" }
    end,
}

local compile_loop = require("compile_loop")
local summarize_edits = compile_loop._test_helpers().summarize_edits

-- ─────────────────────────────────────────────────────────────────────────────
-- summarize_edits
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.summarize_edits", function()
    it("reports one range and the lines it spans", function()
        local ranges, removed, added = summarize_edits({
            { start_line = 12, end_line = 15, expect = "old", replace = "new" },
        })
        expect(ranges).to.equal("12-15")
        expect(removed).to.equal(4)
        expect(added).to.equal(1)
    end)

    it("joins several ranges in call order and sums what they replace", function()
        local ranges, removed = summarize_edits({
            { start_line = 12, end_line = 15, expect = "", replace = "x" },
            { start_line = 40, end_line = 40, expect = "", replace = "y" },
        })
        expect(ranges).to.equal("12-15,40-40")
        expect(removed).to.equal(5)
    end)

    it("counts a replacement by its lines, not its bytes", function()
        local _, _, added = summarize_edits({
            { start_line = 1, end_line = 1, expect = "", replace = "one\ntwo\nthree" },
        })
        expect(added).to.equal(3)
    end)

    it("treats an empty replacement as a deletion that adds nothing", function()
        local ranges, removed, added = summarize_edits({
            { start_line = 7, end_line = 9, expect = "", replace = "" },
        })
        expect(ranges).to.equal("7-9")
        expect(removed).to.equal(3)
        expect(added).to.equal(0)
    end)

    it("returns an empty summary for no edits", function()
        local ranges, removed, added = summarize_edits({})
        expect(ranges).to.equal("")
        expect(removed).to.equal(0)
        expect(added).to.equal(0)
    end)

    it("returns an empty summary for nil, which a rejected call can produce", function()
        local ranges, removed, added = summarize_edits(nil)
        expect(ranges).to.equal("")
        expect(removed).to.equal(0)
        expect(added).to.equal(0)
    end)

    it("skips an unaddressed entry instead of aborting the summary", function()
        local ranges, removed, added = summarize_edits({
            { expect = "", replace = "phantom" },
            { start_line = 3, end_line = 4, expect = "", replace = "real" },
        })
        expect(ranges).to.equal("3-4")
        expect(removed).to.equal(2)
        expect(added).to.equal(1)
    end)

    it("emits no space or '=' so the kv log leaves the field unquoted", function()
        local ranges = summarize_edits({
            { start_line = 12, end_line = 15, expect = "", replace = "x" },
            { start_line = 40, end_line = 40, expect = "", replace = "y" },
        })
        expect(ranges:find("[%s=]")).to.equal(nil)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- Emission through run_loop. Reaching on_edit is the point: the fields are
-- useless if the wiring that fills them is not exercised.
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop edit obs line", function()
    local run_loop = compile_loop._test_helpers().run_loop

    -- A non-empty target is required: single-file diff mode falls back to
    -- whole-file rewriting when the file is empty, and never builds the tool.
    local function run_one_iter(edit_input, edit_result)
        reset_log()
        pending_edit_input = edit_input
        fs_edit_result = edit_result

        local path = "/tmp/cl_obs_test_" .. tostring(os.time()) .. "_" .. tostring(math.random(1e6)) .. ".lua"
        local f = assert(io.open(path, "w"))
        f:write("local a = 1\nlocal b = 2\nreturn a + b\n")
        f:close()

        run_loop({
            target_files = { path },
            multi_file = false,
            edit_mode = "diff",
            lang = "lua",
            spec = "test",
            runner = function(_p)
                return { ok = true }
            end,
            max_iters = 1,
        })

        os.remove(path)
        pending_edit_input = nil
        return path
    end

    it("records the addressed ranges and the size of the change", function()
        local path = run_one_iter({
            path = "/tmp/target.lua",
            base = "v1",
            edits = {
                { start_line = 12, end_line = 15, expect = "old", replace = "a\nb" },
                { start_line = 40, end_line = 40, expect = "x", replace = "y" },
            },
        }, { ok = true, applied = 2, version = "v2" })

        local line = find_line("event=tool_use", "tool=fs_edit")
        expect(line ~= nil).to.be.truthy()
        expect(line:find("ranges=12-15,40-40", 1, true) ~= nil).to.be.truthy()
        expect(line:find("lines_removed=5", 1, true) ~= nil).to.be.truthy()
        expect(line:find("lines_added=3", 1, true) ~= nil).to.be.truthy()
        expect(path ~= nil).to.be.truthy()
    end)

    it("records where a rejected edit aimed, on the fail line", function()
        run_one_iter({
            path = "/tmp/target.lua",
            base = "stale",
            edits = {
                { start_line = 7, end_line = 9, expect = "no match", replace = "z" },
            },
        }, { ok = false, reason = "expect_mismatch", start_line = 7, end_line = 9, actual = "something else" })

        local line = find_line("event=tool_use_fail", "tool=fs_edit")
        expect(line ~= nil).to.be.truthy()
        expect(line:find("err=expect_mismatch", 1, true) ~= nil).to.be.truthy()
        expect(line:find("ranges=7-9", 1, true) ~= nil).to.be.truthy()
    end)
end)
