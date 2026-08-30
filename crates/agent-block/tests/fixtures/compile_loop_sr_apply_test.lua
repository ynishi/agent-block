-- compile_loop_sr_apply_test.lua — mlua-lspec unit tests for the pure SEARCH/REPLACE
-- parse + apply + summary/feedback branches of blocks/compile_loop/init.lua.
--
-- Run via:
--   mcp__lua-debugger__test_launch(
--     code_file    = "crates/agent-block/tests/fixtures/compile_loop_sr_apply_test.lua",
--     search_paths = ["crates/agent-block-core/blocks"]
--   )
--
-- Covers the I/O-free helpers exposed via compile_loop._test_helpers():
--   * parse_search_replace   — single / multi-file marker parsing + malformed inputs
--   * ws_normalize           — whitespace collapse
--   * apply_blocks           — exact + whitespace-normalized match, failures
--   * extract_code           — fenced code extraction (lang fence / any fence / raw)
--   * make_summary           — PASS / give-up summary strings per failure_reason
--   * fnv1a_hash/compute_sr_hash — stable hashing + whitespace normalization
--   * group_blocks_by_path   — grouping SR blocks by path (nil → false key)
--   * build_edit_failure_msg / build_multifile_edit_failure_msg / build_failure_msg
--   * filter_for_tool_output — code/history stripping (context-contamination defence)
--   * cl_oai_map_finish_reason — OpenAI finish_reason mapping
--
-- These helpers are pure string/table transforms and read no runtime globals.
-- require("compile_loop") does not touch std/log/tool at load time; harmless stubs
-- are installed only to guard against any lazily-referenced globals.

local describe, it, expect = lust.describe, lust.it, lust.expect

if not log then
    log = { warn = function() end, info = function() end, debug = function() end }
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
            -- Minimal decoder for the flat string-object shapes these tests feed
            -- into cl_oai_normalize ('{"k":"v",...}' / '{}'); errors on anything
            -- else so the malformed-arguments recovery path stays testable.
            -- The real runtime provides the full std.json; this stub only exists
            -- under mlua-probe test_launch.
            decode = function(s)
                if type(s) ~= "string" then
                    error("not a string")
                end
                if s:match("^%s*{%s*}%s*$") then
                    return {}
                end
                local t = {}
                local found = false
                for k, v in s:gmatch('"([^"]+)"%s*:%s*"([^"]*)"') do
                    t[k] = v
                    found = true
                end
                if not found then
                    error("invalid json")
                end
                return t
            end,
        },
    }
end

local CL = require("compile_loop")
local H = CL._test_helpers()

-- Substring assertion helper.
local function contains(haystack, needle)
    return haystack:find(needle, 1, true) ~= nil
end

-- SEARCH/REPLACE markers (must match the block grammar exactly).
local function sr_block(search, replace)
    return "<<<<<<< SEARCH\n" .. search .. "\n=======\n" .. replace .. "\n>>>>>>> REPLACE"
end

-- ─────────────────────────────────────────────────────────────────────────────
-- parse_search_replace
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.parse_search_replace (single-file)", function()
    local parse = H.parse_search_replace

    it("parses one well-formed block with nil path", function()
        local blocks, err = parse(sr_block("old line", "new line"), false, {})
        expect(err).to.equal(nil)
        expect(#blocks).to.equal(1)
        expect(blocks[1].path).to.equal(nil)
        expect(blocks[1].search).to.equal("old line")
        expect(blocks[1].replace).to.equal("new line")
    end)

    it("parses two consecutive blocks", function()
        local text = sr_block("a", "A") .. "\n" .. sr_block("b", "B")
        local blocks, err = parse(text, false, {})
        expect(err).to.equal(nil)
        expect(#blocks).to.equal(2)
        expect(blocks[1].replace).to.equal("A")
        expect(blocks[2].replace).to.equal("B")
    end)

    it("errors with no blocks found on plain text", function()
        local blocks, err = parse("just some prose, no markers", false, {})
        expect(blocks).to.equal(nil)
        expect(contains(err, "no SEARCH/REPLACE blocks found")).to.equal(true)
    end)

    it("errors on a missing ======= separator", function()
        local malformed = "<<<<<<< SEARCH\nold\n>>>>>>> REPLACE"
        local blocks, err = parse(malformed, false, {})
        expect(blocks).to.equal(nil)
        expect(contains(err, "missing ======= separator")).to.equal(true)
    end)

    it("errors on a missing >>>>>>> REPLACE marker", function()
        local malformed = "<<<<<<< SEARCH\nold\n=======\nnew\n"
        local blocks, err = parse(malformed, false, {})
        expect(blocks).to.equal(nil)
        expect(contains(err, "missing >>>>>>> REPLACE marker")).to.equal(true)
    end)

    it("tolerates the no-space SEARCH marker variant (<<<<<<<SEARCH)", function()
        local text = "<<<<<<<SEARCH\nold line\n=======\nnew line\n>>>>>>> REPLACE"
        local blocks, err = parse(text, false, {})
        expect(err).to.equal(nil)
        expect(#blocks).to.equal(1)
        expect(blocks[1].search).to.equal("old line")
        expect(blocks[1].replace).to.equal("new line")
    end)

    it("tolerates the no-space REPLACE marker variant (>>>>>>>REPLACE)", function()
        local text = "<<<<<<< SEARCH\nold line\n=======\nnew line\n>>>>>>>REPLACE"
        local blocks, err = parse(text, false, {})
        expect(err).to.equal(nil)
        expect(#blocks).to.equal(1)
        expect(blocks[1].replace).to.equal("new line")
    end)
end)

describe("compile_loop.parse_search_replace (multi-file)", function()
    local parse = H.parse_search_replace

    it("attaches the preceding path header to the block", function()
        local text = "<<< path=foo.lua >>>\n" .. sr_block("x", "y")
        local blocks, err = parse(text, true, { ["foo.lua"] = true })
        expect(err).to.equal(nil)
        expect(#blocks).to.equal(1)
        expect(blocks[1].path).to.equal("foo.lua")
    end)

    it("errors when a path is not in the allowlist", function()
        local text = "<<< path=evil.lua >>>\n" .. sr_block("x", "y")
        local blocks, err = parse(text, true, { ["foo.lua"] = true })
        expect(blocks).to.equal(nil)
        expect(contains(err, "not in target_files allowlist")).to.equal(true)
    end)

    it("errors on a SEARCH block with no preceding path header", function()
        local blocks, err = parse(sr_block("x", "y"), true, { ["foo.lua"] = true })
        expect(blocks).to.equal(nil)
        expect(contains(err, "missing path header for multi-file mode")).to.equal(true)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- ws_normalize
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.ws_normalize", function()
    local norm = H.ws_normalize

    it("collapses internal whitespace runs to a single space", function()
        expect(norm("a   b\t c")).to.equal("a b c")
    end)

    it("strips leading and trailing whitespace", function()
        expect(norm("  hi  ")).to.equal("hi")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- apply_blocks
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.apply_blocks", function()
    local apply = H.apply_blocks

    it("applies an exact-match block", function()
        local content = "foo\nbar\nbaz"
        local new, failed = apply(content, { { search = "bar", replace = "BAR" } })
        expect(new).to.equal("foo\nBAR\nbaz")
        expect(#failed).to.equal(0)
    end)

    it("records a failed index when the SEARCH text is absent", function()
        local content = "foo\nbar"
        local new, failed = apply(content, { { search = "missing", replace = "X" } })
        expect(new).to.equal(content) -- unchanged
        expect(#failed).to.equal(1)
        expect(failed[1]).to.equal(1)
    end)

    it("falls back to a whitespace-normalized match when exact fails", function()
        -- content has a 3-space run; search uses a single space → exact miss, ws hit.
        local new, failed = apply("hello   world", { { search = "hello world", replace = "ok" } })
        expect(#failed).to.equal(0)
        expect(new).to.equal("ok")
    end)

    it("applies multiple blocks in order, updating content between each", function()
        local content = "one\ntwo\nthree"
        local new, failed = apply(content, {
            { search = "one", replace = "1" },
            { search = "three", replace = "3" },
        })
        expect(#failed).to.equal(0)
        expect(new).to.equal("1\ntwo\n3")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- extract_code
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

-- ─────────────────────────────────────────────────────────────────────────────
-- make_summary
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.make_summary", function()
    local summary = H.make_summary

    it("reports PASS with iter count on success", function()
        expect(summary(true, 3, 10, nil)).to.equal("PASS in 3 iters")
    end)

    it("reports stagnation give-up", function()
        local s = summary(false, 5, 10, "stagnation")
        expect(contains(s, "give-up: stagnation at iter 5/10")).to.equal(true)
    end)

    it("reports max_iters give-up", function()
        expect(summary(false, 10, 10, "max_iters")).to.equal("give-up: max_iters reached (10)")
    end)

    it("reports llm_call give-up", function()
        expect(summary(false, 2, 10, "llm_call")).to.equal("give-up: llm_call failed at iter 2/10")
    end)

    it("reports an unknown reason verbatim", function()
        expect(summary(false, 1, 10, "weird_reason")).to.equal("give-up: weird_reason")
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- fnv1a_hash / compute_sr_hash
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.fnv1a_hash / compute_sr_hash", function()
    local hash = H.fnv1a_hash
    local sr_hash = H.compute_sr_hash

    it("returns the FNV offset basis for the empty string", function()
        expect(hash("")).to.equal("2166136261")
    end)

    it("is deterministic for identical inputs", function()
        expect(hash("abc")).to.equal(hash("abc"))
    end)

    it("differs for different inputs", function()
        expect(hash("abc") ~= hash("abd")).to.equal(true)
    end)

    it("compute_sr_hash is whitespace-insensitive", function()
        expect(sr_hash("a  b")).to.equal(sr_hash("a b"))
        expect(sr_hash("  a b  ")).to.equal(sr_hash("a b"))
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- group_blocks_by_path
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.group_blocks_by_path", function()
    local group = H.group_blocks_by_path

    it("groups blocks by their path key", function()
        local grouped = group({
            { path = "a.lua", search = "s1" },
            { path = "a.lua", search = "s2" },
            { path = "b.lua", search = "s3" },
        })
        expect(#grouped["a.lua"]).to.equal(2)
        expect(#grouped["b.lua"]).to.equal(1)
    end)

    it("uses the boolean false key for nil-path blocks", function()
        local grouped = group({ { path = nil, search = "s" } })
        expect(#grouped[false]).to.equal(1)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- build_edit_failure_msg / build_multifile_edit_failure_msg / build_failure_msg
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.build_edit_failure_msg", function()
    local build = H.build_edit_failure_msg

    it("names the failing block, echoes its SEARCH, and asks for a re-emit", function()
        local blocks = { { search = "target_text", replace = "r" } }
        local msg = build({ 1 }, blocks, "current file body")
        expect(contains(msg, "block 1")).to.equal(true)
        expect(contains(msg, "target_text")).to.equal(true)
        expect(contains(msg, "Current file content")).to.equal(true)
        expect(contains(msg, "current file body")).to.equal(true)
        expect(contains(msg, "Re-emit ALL blocks")).to.equal(true)
    end)
end)

describe("compile_loop.build_multifile_edit_failure_msg", function()
    local build = H.build_multifile_edit_failure_msg

    it("scopes the failure message to the file path", function()
        local all_failed = {
            { path = "mod.lua", indices = { 1 }, blocks = { { search = "needle" } } },
        }
        local msg = build(all_failed, { ["mod.lua"] = "file contents here" })
        expect(contains(msg, "Edit FAILED in mod.lua")).to.equal(true)
        expect(contains(msg, "needle")).to.equal(true)
        expect(contains(msg, "file contents here")).to.equal(true)
    end)
end)

describe("compile_loop.build_failure_msg", function()
    local build = H.build_failure_msg

    it("embeds lang fence hint, stdout, stderr and exit_code", function()
        local msg = build("lua", { stdout = "out-text", stderr = "err-text", exit_code = 2 })
        expect(contains(msg, "```lua")).to.equal(true)
        expect(contains(msg, "out-text")).to.equal(true)
        expect(contains(msg, "err-text")).to.equal(true)
        expect(contains(msg, "2")).to.equal(true)
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
            modified_files = nil,
            iters = 4,
            summary = "PASS in 4 iters",
            failure_reason = nil,
            last_error = nil,
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
            code = "print('leaked source')",
            history = { { anything = true } },
        })
        expect(out.code).to.equal(nil)
        expect(out.history).to.equal(nil)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- apply_search_replace_tool_handler (write-channel tool, issue #1)
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.apply_search_replace_tool_handler", function()
    local handler = H.apply_search_replace_tool_handler

    -- Create a temp file with content; returns its absolute path.
    local function make_tmp(content)
        local path = os.tmpname()
        local f = assert(io.open(path, "w"))
        f:write(content)
        f:close()
        return path
    end

    local function read_all(path)
        local f = assert(io.open(path, "r"))
        local content = f:read("*a") or ""
        f:close()
        return content
    end

    it("applies an exact-match edit and writes it to disk", function()
        local path = make_tmp("foo\nbar\nbaz\n")
        local res = handler(path, "bar", "BAR", { [path] = true })
        expect(res.ok).to.equal(true)
        expect(contains(res.content, "applied: " .. path)).to.equal(true)
        expect(read_all(path)).to.equal("foo\nBAR\nbaz\n")
        os.remove(path)
    end)

    it("rejects a path outside the target_files allowlist", function()
        local path = make_tmp("foo\n")
        local res = handler(path, "foo", "X", { ["/some/other/file.lua"] = true })
        expect(res.ok).to.equal(false)
        expect(contains(res.error, "not in target_files allowlist")).to.equal(true)
        expect(read_all(path)).to.equal("foo\n") -- untouched
        os.remove(path)
    end)

    it("returns a recoverable error when SEARCH does not match", function()
        local path = make_tmp("foo\n")
        local res = handler(path, "does-not-exist", "X", { [path] = true })
        expect(res.ok).to.equal(false)
        expect(contains(res.error, "did not match")).to.equal(true)
        expect(contains(res.error, "Re-read the file")).to.equal(true)
        expect(read_all(path)).to.equal("foo\n") -- untouched
        os.remove(path)
    end)

    it("rejects an empty search string", function()
        local path = make_tmp("foo\n")
        local res = handler(path, "", "X", { [path] = true })
        expect(res.ok).to.equal(false)
        expect(contains(res.error, "search must be a non-empty string")).to.equal(true)
        os.remove(path)
    end)

    it("rejects a non-string replace", function()
        local path = make_tmp("foo\n")
        local res = handler(path, "foo", nil, { [path] = true })
        expect(res.ok).to.equal(false)
        expect(contains(res.error, "replace must be a string")).to.equal(true)
        os.remove(path)
    end)

    it("falls back to a whitespace-normalized match", function()
        local path = make_tmp("hello   world\n")
        local res = handler(path, "hello world", "ok", { [path] = true })
        expect(res.ok).to.equal(true)
        expect(contains(read_all(path), "ok")).to.equal(true)
        os.remove(path)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- cl_oai_map_finish_reason
-- ─────────────────────────────────────────────────────────────────────────────

-- ─────────────────────────────────────────────────────────────────────────────
-- cl_oai_normalize — wire-shape tolerance (tool-calling spec survey follow-up)
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop.cl_oai_normalize", function()
    local normalize = H.cl_oai_normalize

    -- Build a minimal OpenAI-shaped response with the given tool_calls array.
    local function oai_resp(tool_calls, content)
        return {
            choices = {
                {
                    message = { role = "assistant", content = content, tool_calls = tool_calls },
                    finish_reason = tool_calls and "tool_calls" or "stop",
                },
            },
        }
    end

    it("normalizes the spec-conformant OpenAI form (string arguments + id)", function()
        local resp = normalize(oai_resp({
            { id = "call_1", type = "function", ["function"] = { name = "read_file", arguments = '{"path":"/a"}' } },
        }))
        local blocks = resp.choices[1].message.tool_use_blocks
        expect(#blocks).to.equal(1)
        expect(blocks[1].id).to.equal("call_1")
        expect(blocks[1].input.path).to.equal("/a")
        expect(resp.choices[1].message.stop_reason).to.equal("tool_use")
    end)

    it("accepts object arguments (Ollama native / Gemini args / vLLM parser variants)", function()
        local resp = normalize(oai_resp({
            {
                id = "call_obj",
                type = "function",
                ["function"] = {
                    name = "apply_search_replace",
                    arguments = { path = "/a", search = "x", replace = "y" },
                },
            },
        }))
        local blocks = resp.choices[1].message.tool_use_blocks
        expect(#blocks).to.equal(1)
        expect(blocks[1].input.path).to.equal("/a")
        expect(blocks[1].input.replace).to.equal("y")
        expect(blocks[1].is_error_hint).to.equal(nil)
    end)

    it("synthesizes deterministic ids when id is missing (Ollama native shape)", function()
        local resp = normalize(oai_resp({
            { type = "function", ["function"] = { name = "read_file", arguments = '{"path":"/a"}' } },
            { type = "function", ["function"] = { name = "read_file", arguments = { path = "/b" } } },
        }))
        local blocks = resp.choices[1].message.tool_use_blocks
        expect(blocks[1].id).to.equal("call_synth_1")
        expect(blocks[2].id).to.equal("call_synth_2")
    end)

    it("synthesizes an id when id is an empty string", function()
        local resp = normalize(oai_resp({
            { id = "", type = "function", ["function"] = { name = "read_file", arguments = '{"path":"/a"}' } },
        }))
        expect(resp.choices[1].message.tool_use_blocks[1].id).to.equal("call_synth_1")
    end)

    it("keeps the malformed-string-arguments recovery path (empty input + hint)", function()
        local resp = normalize(oai_resp({
            { type = "function", ["function"] = { name = "read_file", arguments = "{not json" } },
        }))
        local blocks = resp.choices[1].message.tool_use_blocks
        expect(blocks[1].is_error_hint).to.equal("arguments_parse_failed")
        expect(next(blocks[1].input)).to.equal(nil)
        -- Synthetic id also applies on the malformed path.
        expect(blocks[1].id).to.equal("call_synth_1")
    end)

    it("errors on a response with no choices", function()
        local resp, err = normalize({ choices = {} })
        expect(resp).to.equal(nil)
        expect(contains(err, "missing choices")).to.equal(true)
    end)
end)

describe("compile_loop.cl_oai_convert_messages round-trips synthetic ids", function()
    local convert = H.cl_oai_convert_messages

    it("carries a synthetic id through tool_calls and role=tool pairing", function()
        local out = convert({
            {
                role = "assistant",
                content = {
                    { type = "tool_use", id = "call_synth_1", name = "read_file", input = { path = "/a" } },
                },
            },
            {
                role = "user",
                content = {
                    { type = "tool_result", tool_use_id = "call_synth_1", content = "file body" },
                },
            },
        }, nil)
        expect(#out).to.equal(2)
        expect(out[1].tool_calls[1].id).to.equal("call_synth_1")
        expect(out[2].role).to.equal("tool")
        expect(out[2].tool_call_id).to.equal("call_synth_1")
    end)
end)

describe("compile_loop.cl_oai_map_finish_reason", function()
    local map = H.cl_oai_map_finish_reason

    it("maps standard OpenAI finish reasons", function()
        expect(map("stop")).to.equal("end_turn")
        expect(map("tool_calls")).to.equal("tool_use")
        expect(map("length")).to.equal("max_tokens")
    end)

    it("defaults nil to end_turn", function()
        expect(map(nil)).to.equal("end_turn")
    end)
end)
