-- compile_loop_helpers_test.lua — mlua-lspec unit tests for the pure helpers of
-- blocks/tools/compile_loop/init.lua.
--
-- Run via:
--   just test-lua compile_loop_helpers_test   # this file
--   just test-lua                             # every spec fixture
--
-- What is here and what is not. The loop itself needs a kernel — a session, a
-- device, a beat — and there is no stub for one: `knl.session` reaches the
-- syscall bridge, which the pure spec runner does not have. So the loop is
-- covered where it can actually run, by `tests/e2e_compile_loop.rs` against a
-- mock provider, and what is covered here is everything the loop decides
-- WITHOUT the kernel:
--
--   * extract_code            — fenced code extraction (lang fence / any / raw)
--   * text_of                 — the text blocks of an answer, joined
--   * make_summary            — the sentence each give-up reason produces
--   * build_failure_msg       — full mode's feedback turn
--   * build_verify_msg        — diff mode's feedback turn
--   * filter_for_tool_output  — code / transcript stripping (the output filter)
--   * collect_modified_paths  — the sorted path list the result carries
--   * with_line_numbers       — the numbering fs_edit addresses lines by
--   * read_file_range_handler — the guards, before it opens anything
--   * sized_read              — the "too large" branch over std.fs' read
--   * extra_spec              — the two accepted extra_tools shapes
--   * pair_failed             — a returned rejection counts as a failure
--   * verify_signature        — what stagnation compares beats by
--   * port_conf               — disable_thinking, and everything else verbatim
--   * beat_edits              — the count off the log, and its refusal to take
--                               one from a read the kernel's row cap cut short
--
-- The runtime injects std / log / tool as globals; the harness has none, and
-- none of the helpers below reads one.

local describe, it, expect = lust.describe, lust.it, lust.expect

if not log then
    log = { warn = function() end, info = function() end, debug = function() end, error = function() end }
end
if not tool then
    tool = { register = function() end }
end
if not std then
    std = {
        env = {
            get = function()
                return nil
            end,
            get_or = function(_n, d)
                return d
            end,
        },
        json = {
            encode = function(v)
                return tostring(v)
            end,
        },
        fs = {
            tool_specs = function(_opts)
                return {}
            end,
        },
    }
end

local CL = require("compile_loop")
local H = CL._test_helpers()

local function contains(haystack, needle)
    return haystack:find(needle, 1, true) ~= nil
end

-- ─────────────────────────────────────────────────────────────────────────────
-- extract_code / text_of
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.extract_code", function()
    local extract = H.extract_code

    it("extracts a language-specific fenced block", function()
        expect(extract("```lua\nprint('x')\n```", "lua")).to.equal("print('x')")
    end)

    it("falls back to any fence when the lang fence is absent", function()
        expect(extract("```python\nx = 1\n```", "lua")).to.equal("x = 1")
    end)

    it("returns raw text when no fence is present", function()
        expect(extract("no fences here", "lua")).to.equal("no fences here")
    end)
end)

describe("compile_loop.text_of", function()
    it("joins the text blocks and skips the rest", function()
        local text = H.text_of({
            { type = "thinking", thinking = "hidden" },
            { type = "text", text = "one" },
            { type = "tool_use", id = "t1", name = "fs_edit", input = {} },
            { type = "text", text = "two" },
        })
        expect(text).to.equal("one\ntwo")
    end)

    it("answers the empty string for an answer with no blocks", function()
        expect(H.text_of({})).to.equal("")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- make_summary
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.make_summary", function()
    local summary = H.make_summary

    it("reports PASS with iter count on success", function()
        expect(summary(true, 3, 10, nil)).to.equal("PASS in 3 iters")
    end)

    it("reports stagnation give-up", function()
        expect(contains(summary(false, 5, 10, "stagnation"), "give-up: stagnation at iter 5/10")).to.equal(true)
    end)

    it("reports the no-edits give-up separately from stagnation", function()
        expect(contains(summary(false, 3, 10, "no_edits_applied"), "no edits applied")).to.equal(true)
    end)

    it("reports max_iters give-up", function()
        expect(summary(false, 10, 10, "max_iters")).to.equal("give-up: max_iters reached (10)")
    end)

    it("reports llm_call give-up", function()
        expect(summary(false, 2, 10, "llm_call")).to.equal("give-up: llm_call failed at iter 2/10")
    end)

    it("reports the log-outgrew-a-read give-up in its own words", function()
        expect(contains(summary(false, 4, 10, "log_truncated"), "outgrew one read at iter 4/10")).to.equal(true)
    end)

    it("reports an unknown reason verbatim", function()
        expect(summary(false, 1, 10, "weird_reason")).to.equal("give-up: weird_reason")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- beat_edits
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.beat_edits", function()
    local beat_edits = H.beat_edits

    --- A session stand-in answering one method: the rows, and whether the
    --- kernel's row cap cut the read short.
    local function session_of(events, truncated)
        return {
            events = function()
                return events, truncated
            end,
        }
    end

    --- One applied edit of `path`, as the pair the log records for it.
    local function edit_pair(beat, call_id, path)
        return {
            { kind = "tool_call", beat = beat, data = { name = "fs_edit", call_id = call_id, args = { path = path } } },
            { kind = "tool_result", beat = beat, data = { call_id = call_id, result = { ok = true } } },
        }
    end

    it("counts the applied edits of the named beat and collects their paths", function()
        local a, b = edit_pair("b1", "c1", "a.lua"), edit_pair("b1", "c2", "b.lua")
        local other = edit_pair("b0", "c0", "old.lua")
        local log = { other[1], other[2], a[1], a[2], b[1], b[2] }

        local modified = {}
        local applied, why = beat_edits(session_of(log, false), "b1", "fs_edit", modified)
        expect(applied).to.equal(2)
        expect(why).to.equal(nil)
        expect(modified["a.lua"]).to.equal(true)
        expect(modified["b.lua"]).to.equal(true)
        expect(modified["old.lua"]).to.equal(nil)
    end)

    it("answers nil and a reason when the read hit the row cap — never a count of zero", function()
        -- The cap counts forward, so what a truncated read is missing is the
        -- END of the log: the beat asked about here. Reading it as "no edits
        -- applied" would make the loop give up (or count a stagnation) over a
        -- beat that edited every target, which is the worst answer available.
        local pair = edit_pair("b1", "c1", "a.lua")
        local modified = {}
        local applied, why = beat_edits(session_of({ pair[1], pair[2] }, true), "b1", "fs_edit", modified)

        expect(applied).to.equal(nil)
        expect(type(why)).to.equal("string")
        expect(contains(why, "longer than one read")).to.equal(true)
        expect(contains(why, "2 events")).to.equal(true)
        -- And nothing was collected on the way to refusing.
        expect(next(modified)).to.equal(nil)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- the feedback turns
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.build_failure_msg", function()
    it("embeds the lang fence hint, stdout, stderr and exit_code", function()
        local msg = H.build_failure_msg("lua", { stdout = "out-text", stderr = "err-text", exit_code = 2 })
        expect(contains(msg, "```lua")).to.equal(true)
        expect(contains(msg, "out-text")).to.equal(true)
        expect(contains(msg, "err-text")).to.equal(true)
        expect(contains(msg, "2")).to.equal(true)
    end)
end)

describe("compile_loop.build_verify_msg", function()
    it("carries the build's own output and no fence instruction", function()
        local msg = H.build_verify_msg({ stdout = "out-text", stderr = "err-text", exit_code = 1 })
        expect(contains(msg, "out-text")).to.equal(true)
        expect(contains(msg, "err-text")).to.equal(true)
        expect(contains(msg, "```")).to.equal(false)
    end)

    it("says so rather than inventing an exit code the runner did not give", function()
        expect(contains(H.build_verify_msg({}), "unknown")).to.equal(true)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- filter_for_tool_output
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.filter_for_tool_output", function()
    local filter = H.filter_for_tool_output

    it("preserves the reporting fields", function()
        local out = filter({
            ok = true,
            artifact_path = "/abs/x.lua",
            iters = 4,
            summary = "PASS in 4 iters",
        })
        expect(out.ok).to.equal(true)
        expect(out.artifact_path).to.equal("/abs/x.lua")
        expect(out.iters).to.equal(4)
        expect(out.summary).to.equal("PASS in 4 iters")
    end)

    it("strips code and history to prevent context contamination", function()
        local out = filter({
            ok = false,
            iters = 1,
            summary = "give-up: max_iters reached (1)",
            code = "print('leaked source')",
            history = { { anything = true } },
        })
        expect(out.code).to.equal(nil)
        expect(out.history).to.equal(nil)
    end)
end)

describe("compile_loop.collect_modified_paths", function()
    it("answers the set as a sorted list", function()
        local paths = H.collect_modified_paths({ ["/b.lua"] = true, ["/a.lua"] = true })
        expect(#paths).to.equal(2)
        expect(paths[1]).to.equal("/a.lua")
        expect(paths[2]).to.equal("/b.lua")
    end)

    it("answers an empty list for a run that edited nothing", function()
        expect(#H.collect_modified_paths({})).to.equal(0)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- the read tools
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.with_line_numbers", function()
    it("numbers from the line the slice starts at", function()
        expect(H.with_line_numbers("a\nb", 10)).to.equal("10\ta\n11\tb")
    end)

    it("does not number a phantom line after a trailing newline", function()
        expect(H.with_line_numbers("a\n", 1)).to.equal("1\ta")
    end)
end)

describe("compile_loop.read_file_range_handler", function()
    local range = H.read_file_range_handler
    local allowed = { ["/allowed.lua"] = true }

    it("refuses a path outside the target files", function()
        local res = range("/elsewhere.lua", 1, 2, allowed)
        expect(res.ok).to.equal(false)
        expect(contains(res.error, "not one of the target files")).to.equal(true)
    end)

    it("refuses a non-integer range", function()
        expect(range("/allowed.lua", 1.5, 2, allowed).ok).to.equal(false)
        expect(range("/allowed.lua", "1", 2, allowed).ok).to.equal(false)
    end)

    it("refuses a range that runs backwards or starts before line 1", function()
        expect(range("/allowed.lua", 5, 4, allowed).ok).to.equal(false)
        expect(range("/allowed.lua", 0, 4, allowed).ok).to.equal(false)
    end)

    it("refuses more lines than one call may take", function()
        local res = range("/allowed.lua", 1, 501, allowed)
        expect(res.ok).to.equal(false)
        expect(contains(res.error, "more than 500 lines")).to.equal(true)
    end)
end)

describe("compile_loop.sized_read", function()
    -- A stand-in for std.fs' read: it answers the triple that one answers.
    local function reader(content)
        return function(input)
            if input.start_line then
                return { content = "slice", lines = 1, version = "v1" }
            end
            return { content = content, lines = 3, version = "v1" }
        end
    end

    it("hands a small file back untouched", function()
        local res = H.sized_read(reader("short"))({ path = "/a.lua" })
        expect(res.content).to.equal("short")
    end)

    it("answers a large file with its length and where to go instead", function()
        local res = H.sized_read(reader(string.rep("x", 10001)))({ path = "/a.lua" })
        expect(res.ok).to.equal(false)
        expect(contains(res.error, "too large: 3 lines")).to.equal(true)
        expect(contains(res.error, "read_file_range")).to.equal(true)
    end)

    it("leaves a ranged read alone — that is the request it points at", function()
        local res = H.sized_read(reader(string.rep("x", 10001)))({ path = "/a.lua", start_line = 1, end_line = 2 })
        expect(res.content).to.equal("slice")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- the loop's own judgements
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.extra_spec", function()
    it("flattens the nested agent-layer form", function()
        local spec = H.extra_spec({
            name = "get_hint",
            schema = { description = "d", input_schema = { type = "object" } },
            handler = function() end,
        })
        expect(spec.name).to.equal("get_hint")
        expect(spec.description).to.equal("d")
        expect(spec.schema).to.equal(nil)
        expect(spec.input_schema.type).to.equal("object")
    end)

    it("passes a flat spec through", function()
        local spec = H.extra_spec({
            name = "get_hint",
            description = "d",
            input_schema = { type = "object" },
            handler = function() end,
        })
        expect(spec.description).to.equal("d")
        expect(spec.input_schema.type).to.equal("object")
    end)
end)

describe("compile_loop.pair_failed", function()
    it("reads the kernel's flag — a handler that raised", function()
        expect(H.pair_failed({ ok = false, result = "boom" })).to.equal(true)
    end)

    it("reads a rejection the tool RETURNED, which the kernel closed ok", function()
        expect(H.pair_failed({ ok = true, result = { ok = false, reason = "expect_mismatch" } })).to.equal(true)
    end)

    it("leaves an applied edit alone", function()
        expect(H.pair_failed({ ok = true, result = { ok = true, applied = 1 } })).to.equal(false)
    end)

    it("leaves a plain text result alone", function()
        expect(H.pair_failed({ ok = true, result = "1\tprint('x')" })).to.equal(false)
    end)
end)

describe("compile_loop.verify_signature", function()
    it("is the verify's stderr, so the same failure twice reads as one thing", function()
        local beat = {
            id = "b1",
            events = {
                { kind = "llm_response", data = { content = {} } },
                { kind = "verify", data = { ok = false, stderr = "boom", stdout = "" } },
            },
        }
        expect(H.verify_signature(beat)).to.equal("boom")
    end)

    it("has no signature for a beat the verify never reached", function()
        expect(H.verify_signature({ id = "b1", events = { { kind = "llm_response", data = {} } } })).to.equal(nil)
    end)
end)

describe("compile_loop.port_conf", function()
    it("translates disable_thinking into the shared vocabulary", function()
        local conf = H.port_conf({ disable_thinking = true, model = "m" })
        expect(conf.thinking.enabled).to.equal(false)
        expect(conf.disable_thinking).to.equal(nil)
        expect(conf.model).to.equal("m")
    end)

    it("lets an explicit thinking win", function()
        local conf = H.port_conf({ disable_thinking = true, thinking = { effort = "medium" } })
        expect(conf.thinking.effort).to.equal("medium")
    end)

    it("forwards everything else verbatim, including what it has never heard of", function()
        local conf = H.port_conf({ temperature = 0.2, some_new_knob = "x" })
        expect(conf.temperature).to.equal(0.2)
        expect(conf.some_new_knob).to.equal("x")
    end)

    it("answers a table for no conf at all", function()
        expect(type(H.port_conf(nil))).to.equal("table")
    end)
end)
