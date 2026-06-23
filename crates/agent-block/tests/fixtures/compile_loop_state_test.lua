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

-- =============================================================
-- Runtime stubs for all subsequent tests.
-- These globals are injected by the agent-block Lua runtime but are absent
-- in the mlua test harness. Stubs prevent crashes when init.lua code paths
-- (resolve_temperature, resolve_dump_mode, obs_event, etc.) are exercised.
-- =============================================================
if not log then
    log = { warn = function() end, info = function() end, debug = function() end }
end
if not tool then
    tool = { register = function() end }
end
if not std then
    std = {
        env  = {
            get    = function(_name) return nil end,
            get_or = function(_name, default) return default end,
        },
        json = { encode = function(v) return tostring(v) end },
    }
end

-- =============================================================
-- Temperature resolution tests (ST-temp-ax-A)
-- =============================================================

describe("resolve_temperature — default 0.0 when env unset", function()
    local h = compile_loop._test_helpers()
    local resolve_temperature = h.resolve_temperature

    it("returns 0.0 when no env override is set", function()
        -- Ensure no env override is active.
        compile_loop._test_reset_env_get()
        local t = resolve_temperature()
        expect(t).to.equal(0.0)
    end)

    it("returns env value when COMPILE_LOOP_LLM_TEMPERATURE is '0.3'", function()
        compile_loop._test_set_env_get(function(name)
            if name == "COMPILE_LOOP_LLM_TEMPERATURE" then return "0.3" end
            return nil
        end)
        local t = resolve_temperature()
        compile_loop._test_reset_env_get()
        expect(t > 0.29 and t < 0.31).to.equal(true)
    end)

    it("returns 0.0 and does not error on non-numeric env value", function()
        compile_loop._test_set_env_get(function(name)
            if name == "COMPILE_LOOP_LLM_TEMPERATURE" then return "not_a_number" end
            return nil
        end)
        local t = resolve_temperature()
        compile_loop._test_reset_env_get()
        -- Must fall back to 0.0 (env is user input, no crash allowed).
        expect(t).to.equal(0.0)
    end)

    it("returns 0.0 when env returns nil", function()
        compile_loop._test_set_env_get(function(_name) return nil end)
        local t = resolve_temperature()
        compile_loop._test_reset_env_get()
        expect(t).to.equal(0.0)
    end)
end)

describe("temperature in OpenAI body via _test_set_llm_call capture", function()
    -- Helper: capture the effective temperature (opts.temperature or resolve_temperature()).
    -- The llm_call override fires before the OpenAI body is constructed, so opts.temperature
    -- at that point is what the caller set.  The body logic is:
    --   body.temperature = opts.temperature or resolve_temperature()
    -- We replicate that logic in the capture function to test the full priority chain.
    local h_temp = compile_loop._test_helpers()
    local resolve_temperature = h_temp.resolve_temperature

    local function capture_temperature(caller_temp)
        local captured_temperature = nil
        compile_loop._test_set_llm_call(function(opts, _msgs)
            -- Mirror the production OpenAI body construction:
            --   temperature = opts.temperature or resolve_temperature()
            captured_temperature = opts.temperature or resolve_temperature()
            -- Return a minimal fake response to end the loop on first call.
            return {
                choices = { {
                    message = {
                        content = "```lua\nprint('hi')\n```",
                        role    = "assistant",
                    },
                } },
            }
        end)
        -- Write a temp target file so run_loop has a real path.
        local tmp = "/tmp/cl_temp_test_" .. tostring(os.time()) .. ".lua"
        local f = io.open(tmp, "w")
        if f then f:write("-- placeholder\n"); f:close() end

        local conf = {
            target_files = { tmp },
            multi_file   = false,
            edit_mode    = "full",
            lang         = "lua",
            spec         = "test",
            runner       = function(_path) return { ok = true } end,
            max_iters    = 5,
        }
        if caller_temp ~= nil then
            conf.temperature = caller_temp
        end
        local run_loop_fn = compile_loop._test_helpers().run_loop
        run_loop_fn(conf)
        compile_loop._test_reset_llm_call()
        os.remove(tmp)
        return captured_temperature
    end

    it("temperature defaults to 0.0 when caller and env are both unset", function()
        compile_loop._test_reset_env_get()
        local t = capture_temperature(nil)
        expect(t).to.equal(0.0)
    end)

    it("caller temperature=0.5 overrides env and default", function()
        compile_loop._test_set_env_get(function(name)
            if name == "COMPILE_LOOP_LLM_TEMPERATURE" then return "0.3" end
            return nil
        end)
        local t = capture_temperature(0.5)
        compile_loop._test_reset_env_get()
        expect(t > 0.49 and t < 0.51).to.equal(true)
    end)

    it("env COMPILE_LOOP_LLM_TEMPERATURE=0.3 is used when caller unset", function()
        compile_loop._test_set_env_get(function(name)
            if name == "COMPILE_LOOP_LLM_TEMPERATURE" then return "0.3" end
            return nil
        end)
        local t = capture_temperature(nil)
        compile_loop._test_reset_env_get()
        expect(t > 0.29 and t < 0.31).to.equal(true)
    end)
end)

-- =============================================================
-- Bad stagnation tests (ST-bad-stag-ax-B)
-- Uses single-file full mode via run_loop (exposed via _test_helpers).
-- Bypasses make()/handler() to avoid needing 'tool' and 'std' runtime globals.
-- =============================================================

local h_run = compile_loop._test_helpers()
local run_loop = h_run.run_loop

-- Build a minimal run_loop conf for single-file full mode.
local function make_run_conf(opts)
    local tmp = "/tmp/cl_rl_" .. tostring(os.time()) .. math.random(1000) .. ".lua"
    local f = io.open(tmp, "w")
    if f then f:write("print('placeholder')\n"); f:close() end
    local conf = {
        target_files = { tmp },
        multi_file   = false,
        edit_mode    = "full",
        lang         = "lua",
        spec         = "test",
        runner       = opts.runner or function(_path) return { ok = false, stderr = "fail", exit_code = 1 } end,
        max_iters    = opts.max_iters or 10,
        _tmp_path    = tmp,  -- keep for cleanup
    }
    return conf
end

-- Helper: run with LLM always returning empty code (bad stagnation every iter).
local function run_bad_stagnation(max_iters)
    compile_loop._test_set_llm_call(function(_opts, _msgs)
        return { choices = { { message = { content = "", role = "assistant" } } } }
    end)
    local conf = make_run_conf({ max_iters = max_iters })
    local result = run_loop(conf)
    compile_loop._test_reset_llm_call()
    os.remove(conf._tmp_path)
    return result
end

-- Helper: run with LLM returning valid code each iter, runner always fails with same stderr.
-- This exercises the good stagnation path (is_stagnant fires after 3 identical stderr runs).
local function run_good_stagnation()
    compile_loop._test_set_llm_call(function(_opts, _msgs)
        return { choices = { { message = { content = "```lua\nprint('x')\n```", role = "assistant" } } } }
    end)
    local conf = make_run_conf({
        runner = function(_path)
            return { ok = false, stderr = "same_error_always", exit_code = 1 }
        end,
        max_iters = 10,
    })
    local result = run_loop(conf)
    compile_loop._test_reset_llm_call()
    os.remove(conf._tmp_path)
    return result
end

describe("bad stagnation — no_edits_applied BLOCKED after STAGNATION_WINDOW (3) iters", function()
    it("failure_reason is no_edits_applied after 3 consecutive zero-edit iters", function()
        local result = run_bad_stagnation(10)
        expect(result.ok).to.equal(false)
        expect(result.failure_reason).to.equal("no_edits_applied")
    end)

    it("terminates at or before iter 3 (STAGNATION_WINDOW), not max_iters", function()
        local result = run_bad_stagnation(10)
        -- Should stop at iter 3, not 10.
        expect(result.iters <= 3).to.equal(true)
    end)

    it("result.ok is false", function()
        local result = run_bad_stagnation(10)
        expect(result.ok).to.equal(false)
    end)
end)

describe("bad stagnation reset — count resets on successful edit", function()
    it("failure_reason is no_edits_applied after another 3 bad iters post-reset", function()
        -- Scenario: 2 bad iters → 1 good (non-empty code) iter → 3 more bad iters → BLOCKED.
        -- Total iters: ≤6. bad_stagnation_count: 1,2 → reset(0) → 1,2,3 → BLOCKED.
        --
        -- IMPORTANT: To prevent is_stagnant() from firing on the good iter (iter 3),
        -- the runner returns a DIFFERENT stderr on the good iter than on bad iters.
        -- is_stagnant() checks the last 3 history entries for identical stderr;
        -- a varying stderr on iter 3 prevents the false-positive stagnation trigger.
        local call_n = 0
        -- seq: true=empty(bad), false=code(good+runner-fail with unique stderr)
        local seq = { true, true, false, true, true, true }
        compile_loop._test_set_llm_call(function(_opts, _msgs)
            call_n = call_n + 1
            local is_empty = (seq[call_n] ~= false)
            if is_empty then
                return { choices = { { message = { content = "", role = "assistant" } } } }
            else
                return { choices = { { message = { content = "```lua\nprint('hi')\n```", role = "assistant" } } } }
            end
        end)

        -- Runner returns unique stderr on good iter to avoid triggering is_stagnant.
        local runner_n = 0
        local conf = make_run_conf({
            runner = function(_path)
                runner_n = runner_n + 1
                -- The good iter (iter 3, runner call 3) produces different stderr.
                local stderr = (runner_n == 3) and "different_stderr" or "fail"
                return { ok = false, stderr = stderr, exit_code = 1 }
            end,
            max_iters = 20,
        })
        local result = run_loop(conf)
        compile_loop._test_reset_llm_call()
        os.remove(conf._tmp_path)

        expect(result.ok).to.equal(false)
        expect(result.failure_reason).to.equal("no_edits_applied")
        -- Should have taken at most 6 iters (2 bad + 1 good + 3 bad).
        expect(result.iters <= 6).to.equal(true)
    end)
end)

describe("good stagnation — failure_reason=stagnation preserved when edits succeed", function()
    it("is_stagnant fires with failure_reason=stagnation when code is non-empty each iter", function()
        local result = run_good_stagnation()
        expect(result.ok).to.equal(false)
        expect(result.failure_reason).to.equal("stagnation")
    end)
end)
