-- compile_loop_state_test.lua — mlua-lspec unit tests for ST1 mf_state field additions.
--
-- Run via:
--   mcp__lua-debugger__test_launch(
--     code_file = "tests/fixtures/compile_loop_state_test.lua",
--     search_paths = ["blocks"]
--   )
--
-- Verifies:
--   1. mf_state.file_digest is an empty table (not nil, not any other type)
--   2. mf_state.file_digest_refresh is "auto"
--   3. Module-level constants introduced in ST1 have the expected default values
--
-- NOTE: mf_state is internal to run_loop; M._test_make_mf_state() exposes the
-- initial defaults for unit-testing purposes without running the full pipeline.

local describe, it, expect = lust.describe, lust.it, lust.expect

local compile_loop = require("compile_loop")

describe("mf_state initial fields (ST1)", function()
    local state = compile_loop._test_make_mf_state()

    it("file_digest is a table", function()
        expect(type(state.file_digest)).to.equal("table")
    end)

    it("file_digest is empty on init", function()
        local count = 0
        for _ in pairs(state.file_digest) do count = count + 1 end
        expect(count).to.equal(0)
    end)

    it("file_digest_refresh defaults to 'auto'", function()
        expect(state.file_digest_refresh).to.equal("auto")
    end)

    it("iter starts at 0", function()
        expect(state.iter).to.equal(0)
    end)

    it("last_err starts nil", function()
        expect(state.last_err).to.equal(nil)
    end)

    it("sr_history is an empty table", function()
        expect(type(state.sr_history)).to.equal("table")
        local count = 0
        for _ in pairs(state.sr_history) do count = count + 1 end
        expect(count).to.equal(0)
    end)
end)

