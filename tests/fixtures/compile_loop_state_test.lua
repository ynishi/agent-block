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
--   4. mf_state.modified_set is an empty table (crux §3)
--   5. is_stagnant_v2 fires only on full-window identical hashes (crux §1)
--   6. sr_history can be appended via update_state on every path (crux §2)
--   7. collect_modified_paths converts a set to a sorted list
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

    it("modified_set is an empty table", function()
        expect(type(state.modified_set)).to.equal("table")
        local count = 0
        for _ in pairs(state.modified_set) do count = count + 1 end
        expect(count).to.equal(0)
    end)
end)

describe("is_stagnant_v2 full-window threshold (crux §1)", function()
    local h = compile_loop._test_helpers()
    local is_stagnant_v2 = h.is_stagnant_v2
    local compute_sr_hash = h.compute_sr_hash
    local update_state = h.update_state

    local hA = compute_sr_hash("block_A")
    local hB = compute_sr_hash("block_B")

    it("returns false when sr_history has fewer than STAGNATION_WINDOW entries", function()
        local state = compile_loop._test_make_mf_state()
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hA })
        -- Only 2 entries, window = 3: must not fire
        expect(is_stagnant_v2(state, true)).to.equal(false)
    end)

    it("returns false when 2-of-3 entries match (partial window, last_verify_failed=true)", function()
        -- [hA, hA, hB]: 2 of 3 are hA — must NOT fire (crux: strictly > 2 required)
        local state = compile_loop._test_make_mf_state()
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hB })
        expect(is_stagnant_v2(state, true)).to.equal(false)
    end)

    it("returns false when last_verify_failed=false even with full-window identical hashes", function()
        local state = compile_loop._test_make_mf_state()
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hA })
        expect(is_stagnant_v2(state, false)).to.equal(false)
    end)

    it("returns true when all 3 entries in window are identical (full-window match)", function()
        -- [hA, hA, hA]: all 3 identical — must fire
        local state = compile_loop._test_make_mf_state()
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hA })
        expect(is_stagnant_v2(state, true)).to.equal(true)
    end)

    it("uses only the last STAGNATION_WINDOW entries from sr_history", function()
        -- [hB, hA, hA, hA]: last 3 are all hA — must fire even with leading hB
        local state = compile_loop._test_make_mf_state()
        update_state(state, { sr_hash_append = hB })
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hA })
        expect(is_stagnant_v2(state, true)).to.equal(true)
    end)

    it("[hA, hB, hA] — 2-of-3 non-contiguous match — returns false", function()
        local state = compile_loop._test_make_mf_state()
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hB })
        update_state(state, { sr_hash_append = hA })
        expect(is_stagnant_v2(state, true)).to.equal(false)
    end)
end)

describe("sr_history appended via update_state (crux §2)", function()
    local h = compile_loop._test_helpers()
    local compute_sr_hash = h.compute_sr_hash
    local update_state = h.update_state

    it("update_state appends sr_hash_append to sr_history", function()
        local state = compile_loop._test_make_mf_state()
        local hash = compute_sr_hash("some content")
        update_state(state, { sr_hash_append = hash })
        expect(#state.sr_history).to.equal(1)
        expect(state.sr_history[1]).to.equal(hash)
    end)

    it("update_state appends multiple times preserving order", function()
        local state = compile_loop._test_make_mf_state()
        local hA = compute_sr_hash("content_A")
        local hB = compute_sr_hash("content_B")
        update_state(state, { sr_hash_append = hA })
        update_state(state, { sr_hash_append = hB })
        expect(#state.sr_history).to.equal(2)
        expect(state.sr_history[1]).to.equal(hA)
        expect(state.sr_history[2]).to.equal(hB)
    end)
end)

describe("collect_modified_paths (crux §3)", function()
    local h = compile_loop._test_helpers()
    local collect_modified_paths = h.collect_modified_paths

    it("returns empty list for empty set", function()
        local result = collect_modified_paths({})
        expect(type(result)).to.equal("table")
        expect(#result).to.equal(0)
    end)

    it("returns sorted list of paths from set", function()
        local set = { ["/b/file.lua"] = true, ["/a/file.lua"] = true }
        local result = collect_modified_paths(set)
        expect(#result).to.equal(2)
        expect(result[1]).to.equal("/a/file.lua")
        expect(result[2]).to.equal("/b/file.lua")
    end)

    it("each path appears exactly once", function()
        local set = { ["/x/foo.lua"] = true }
        local result = collect_modified_paths(set)
        expect(#result).to.equal(1)
        expect(result[1]).to.equal("/x/foo.lua")
    end)
end)
