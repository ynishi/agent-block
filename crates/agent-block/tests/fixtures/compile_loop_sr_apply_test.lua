-- compile_loop_sr_apply_test.lua — mlua-lspec unit tests for the pure
-- summary / feedback / normalization branches of blocks/tools/compile_loop/init.lua.
--
-- Run via:
--   mcp__lua-debugger__test_launch(
--     code_file    = "crates/agent-block/tests/fixtures/compile_loop_sr_apply_test.lua",
--     search_paths = [
--       "crates/agent-block-core/blocks/tools",   -- compile_loop
--       "crates/agent-block-core/blocks/lib",     -- llm_proto, tool_loop
--       "crates/agent-block-core/blocks",         -- agent
--     ]
--   )
--
-- Covers the I/O-free helpers exposed via compile_loop._test_helpers():
--   * make_summary           — PASS / give-up summary strings per failure_reason
--   * fnv1a_hash/compute_sr_hash — stable hashing + whitespace normalization
--   * build_failure_msg      — runner stdout/stderr/exit_code feedback body
--   * filter_for_tool_output — code/history stripping (context-contamination defence)
--   * cl_oai_normalize / cl_oai_convert_messages / cl_oai_map_finish_reason
--                            — OpenAI-compatible response normalization
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

-- The SEARCH/REPLACE machinery these tests used to cover (parse_search_replace,
-- ws_normalize, apply_blocks, group_blocks_by_path, build_edit_failure_msg,
-- build_multifile_edit_failure_msg) was removed when compile_loop's diff mode
-- moved onto the std.fs edit tools. Its successor — line-range edits checked
-- against `expect` and a `base` version — is covered by tests/e2e_fs_edit.rs
-- and the unit tests in agent-block-core/src/bridge/fs.rs.

-- extract_code is a one-line delegate to the `llm` bridge; the fence-matching
-- cases live with the implementation in agent-block-core/src/bridge/llm.rs.
-- Calling it here would need an `llm` stub, and the test would then assert
-- against the stub rather than the matcher.

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
-- build_failure_msg
-- ─────────────────────────────────────────────────────────────────────────────

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

-- The write channel no longer has a compile_loop-local handler: the edit is
-- std.fs's (tests/e2e_fs_edit.rs) and the loop only turns its structured
-- rejection into tool_result text (tests/e2e_compile_loop.rs).

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
                    name = "fs_edit",
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
        expect(blocks[1].id).to.equal("tc0000001")
        expect(blocks[2].id).to.equal("tc0000002")
    end)

    it("synthesizes an id when id is an empty string", function()
        local resp = normalize(oai_resp({
            { id = "", type = "function", ["function"] = { name = "read_file", arguments = '{"path":"/a"}' } },
        }))
        expect(resp.choices[1].message.tool_use_blocks[1].id).to.equal("tc0000001")
    end)

    it("keeps the malformed-string-arguments recovery path (empty input + hint)", function()
        local resp = normalize(oai_resp({
            { type = "function", ["function"] = { name = "read_file", arguments = "{not json" } },
        }))
        local blocks = resp.choices[1].message.tool_use_blocks
        expect(blocks[1].is_error_hint).to.equal("arguments_parse_failed")
        expect(next(blocks[1].input)).to.equal(nil)
        -- Synthetic id also applies on the malformed path.
        expect(blocks[1].id).to.equal("tc0000001")
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
                    { type = "tool_use", id = "tc0000001", name = "read_file", input = { path = "/a" } },
                },
            },
            {
                role = "user",
                content = {
                    { type = "tool_result", tool_use_id = "tc0000001", content = "file body" },
                },
            },
        }, nil)
        expect(#out).to.equal(2)
        expect(out[1].tool_calls[1].id).to.equal("tc0000001")
        expect(out[2].role).to.equal("tool")
        expect(out[2].tool_call_id).to.equal("tc0000001")
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
