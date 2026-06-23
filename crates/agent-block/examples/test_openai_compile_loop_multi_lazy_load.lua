-- test_openai_compile_loop_multi_lazy_load.lua
--
-- Mock LLM e2e: lazy load path verification (10 iters, <=24K char per iter).
-- provider: openai
--
-- Runs in two environments:
--   1. mcp__lua-debugger__test_launch (mlua-lspec): uses describe/it/expect.
--      mock globals (log, std, tool, agent) are injected at the top.
--   2. agent-block mlua host: log/std/tool/agent are real injected globals.
--      The `lust` detection guard skips mock injection.
--
-- Does NOT call the real OpenAI API (offline / no cost).
-- Acceptance criteria:
--   (alpha) read_file tool dispatched >= 3 times
--   (beta)  max_iters=10 completes or stagnation give-up
--   (gamma) all iter messages[] sizes <= 24,000 chars

-- ── Environment detection + mock globals for test_launch ─────────────────────
-- In test_launch, `lust` is pre-loaded. In the mlua host it is not.
local in_test_framework = (type(lust) == "table")

if in_test_framework then
    -- Inject globals that agent-block normally provides.
    tool = tool or { register = function() end }

    std = std
        or {
            env = {
                get = function(_k)
                    return nil
                end,
                get_or = function(_k, d)
                    return d
                end,
            },
            json = {
                -- Minimal JSON encoder sufficient for this test.
                encode = function(t)
                    if type(t) ~= "table" then
                        return tostring(t)
                    end
                    local parts = {}
                    for k, v in pairs(t) do
                        local vstr
                        if type(v) == "boolean" then
                            vstr = tostring(v)
                        elseif type(v) == "number" then
                            vstr = tostring(v)
                        elseif type(v) == "string" then
                            vstr = '"' .. v:gsub('"', '\\"') .. '"'
                        elseif type(v) == "nil" then
                            vstr = "null"
                        else
                            vstr = '"' .. tostring(v) .. '"'
                        end
                        parts[#parts + 1] = '"' .. tostring(k) .. '":' .. vstr
                    end
                    return "{" .. table.concat(parts, ",") .. "}"
                end,
                -- Minimal decoder: extracts top-level string/bool/number fields.
                -- Sufficient for reading ok, iters, failure_reason, summary from result JSON.
                decode = function(s)
                    if type(s) ~= "string" then
                        return {}
                    end
                    local t = {}
                    -- ok field
                    local ok_val = s:match('"ok"%s*:%s*(true)') or s:match('"ok"%s*:%s*(false)')
                    if ok_val == "true" then
                        t.ok = true
                    elseif ok_val == "false" then
                        t.ok = false
                    end
                    -- iters field
                    local iters_val = s:match('"iters"%s*:%s*(%d+)')
                    if iters_val then
                        t.iters = tonumber(iters_val)
                    end
                    -- failure_reason field
                    local fr = s:match('"failure_reason"%s*:%s*"([^"]+)"')
                    if fr then
                        t.failure_reason = fr
                    end
                    -- summary field
                    local summ = s:match('"summary"%s*:%s*"([^"]*)"')
                    if summ then
                        t.summary = summ
                    end
                    return t
                end,
            },
        }

    log = log
        or {
            info = function(msg)
                io.write("[INFO]  " .. tostring(msg) .. "\n")
            end,
            warn = function(msg)
                io.write("[WARN]  " .. tostring(msg) .. "\n")
            end,
            error = function(msg)
                io.write("[ERROR] " .. tostring(msg) .. "\n")
            end,
        }

    -- agent module mock: returns an LLM context pointing at "openai" so that
    -- run_loop uses the openai provider path.
    package.preload["agent"] = package.preload["agent"]
        or function()
            return {
                _llm_ctx_top = function()
                    return { provider = "openai", api_key = "mock-key" }
                end,
            }
        end

    -- search_paths is provided via test_launch's search_paths argument,
    -- which prepends the blocks/ directory to package.path automatically.
    -- No manual path manipulation needed here.
end

-- ── Load compile_loop ─────────────────────────────────────────────────────────
local compile_loop = require("compile_loop")

-- ── Helpers ──────────────────────────────────────────────────────────────────

local function write_file(path, content)
    local f, err = io.open(path, "w")
    if not f then
        error("cannot write " .. path .. ": " .. tostring(err))
    end
    f:write(content)
    f:close()
end

-- Measure total character size of a messages[] array.
-- Handles both string content and table content (tool_result/tool_use blocks).
local function measure_messages(messages)
    local total = 0
    for _, m in ipairs(messages) do
        if type(m.content) == "string" then
            total = total + #m.content
        elseif type(m.content) == "table" then
            for _, blk in ipairs(m.content) do
                if type(blk) == "table" then
                    -- text block or tool_result content
                    if type(blk.content) == "string" then
                        total = total + #blk.content
                    end
                    if type(blk.text) == "string" then
                        total = total + #blk.text
                    end
                    -- tool_use input: approximate via json encode
                    if type(blk.input) == "table" then
                        local enc_ok, enc = pcall(std.json.encode, blk.input)
                        if enc_ok then
                            total = total + #enc
                        end
                    end
                end
            end
        end
    end
    return total
end

-- ── Test suite ───────────────────────────────────────────────────────────────

local describe = lust.describe
local it = lust.it
local expect = lust.expect

describe("compile_loop multi-file lazy load e2e (openai)", function()
    -- ── R4: _test_set_llm_call must be exported ───────────────────────────────
    it("exports _test_set_llm_call (subtask-1 dependency)", function()
        expect(type(compile_loop._test_set_llm_call)).to.equal("function")
        expect(type(compile_loop._test_reset_llm_call)).to.equal("function")
    end)

    -- ── Main e2e: 10 iters with mock LLM ─────────────────────────────────────
    it("(alpha/beta/gamma) lazy load path verification — 10 iters mock", function()
        -- TARGET_FILES: 4 files, each with a single-line marker.
        -- Content is tiny (~20 chars) so messages stay well under 24K.
        -- oai_ prefix avoids /tmp/ collision with anthropic version on concurrent runs.
        local TARGET_FILES = {
            "/tmp/oai_lazy_a.lua",
            "/tmp/oai_lazy_b.lua",
            "/tmp/oai_lazy_c.lua",
            "/tmp/oai_lazy_d.lua",
        }

        -- Write initial content (exact-match SEARCH for Stage-1 hit in apply_blocks).
        for i, path in ipairs(TARGET_FILES) do
            write_file(path, "-- oai_lazy_file_" .. i .. "_v0\n")
        end

        -- Mock state.
        local mock_state = {
            call_count = 0,
            messages_size_log = {},
            max_seen_size = 0,
        }

        -- Per-file version tracker: SR SEARCH text must match current file content exactly.
        local file_versions = { 1, 1, 1, 1 }

        -- Build an SR block that exactly matches the current file content.
        -- Returns the SR text and bumps the version counter.
        local function make_sr_block(file_idx, call_tag)
            local path = TARGET_FILES[file_idx]
            local cur_ver = file_versions[file_idx]
            local search_line
            if cur_ver == 1 then
                search_line = "-- oai_lazy_file_" .. file_idx .. "_v0\n"
            else
                search_line = "-- oai_lazy_file_" .. file_idx .. "_v" .. (cur_ver - 1) .. "_c" .. (cur_ver - 1) .. "\n"
            end
            local replace_line = "-- oai_lazy_file_" .. file_idx .. "_v" .. cur_ver .. "_c" .. call_tag .. "\n"
            file_versions[file_idx] = cur_ver + 1

            return string.format(
                "<<< path=%s >>>\n<<<<<<< SEARCH\n%s=======\n%s>>>>>>> REPLACE\n",
                path,
                search_line,
                replace_line
            )
        end

        -- Mock LLM.
        -- Odd calls (1, 3, 5, …): return tool_use (read_file for one target file).
        -- Even calls (2, 4, 6, …): return SR text for the matching file.
        -- Pattern: each iter consumes 2 calls → up to 20 calls for 10 iters.
        -- NOTE: _test_set_llm_call replaces llm_call entirely, so mock returns
        -- the internal shape (tool_use_blocks), NOT raw OpenAI wire format (Crux #1 C5).
        local function mock_llm(_opts, messages)
            mock_state.call_count = mock_state.call_count + 1
            local cn = mock_state.call_count

            -- (gamma) measure and assert messages size.
            local sz = measure_messages(messages)
            mock_state.messages_size_log[cn] = sz
            if sz > mock_state.max_seen_size then
                mock_state.max_seen_size = sz
            end
            assert(sz <= 24000, string.format("(gamma) messages size %d > 24000 at llm_call #%d", sz, cn))

            if (cn % 2) == 1 then
                -- tool_use response: read_file for file indexed by cn.
                local file_idx = 1 + ((cn // 2) % #TARGET_FILES)
                return {
                    choices = {
                        {
                            message = {
                                content = "",
                                tool_use_blocks = {
                                    {
                                        id = "tid_" .. cn,
                                        name = "read_file",
                                        input = { path = TARGET_FILES[file_idx] },
                                    },
                                },
                                stop_reason = "tool_use",
                            },
                        },
                    },
                }
            else
                -- SR text response for the file matching this even call.
                local file_idx = 1 + (((cn // 2) - 1) % #TARGET_FILES)
                local sr_text = make_sr_block(file_idx, cn)
                return {
                    choices = { {
                        message = { content = sr_text },
                    } },
                }
            end
        end

        -- Runner: forced-fail for iters 1–9, ok=true on iter 10.
        -- Unique stderr per call ensures stagnation is not triggered early
        -- (is_stagnant_v2 needs >= 2 repeated sr_hash in the last 3 entries).
        local runner_call_count = 0
        local function lazy_runner(file_paths)
            runner_call_count = runner_call_count + 1
            local _n = type(file_paths) == "table" and #file_paths or 1
            if runner_call_count >= 10 then
                return { ok = true, stdout = "ALL_PASS", stderr = "", exit_code = 0 }
            end
            return {
                ok = false,
                stdout = "",
                stderr = "FAIL_iter_" .. runner_call_count .. "_unique",
                exit_code = 1,
            }
        end

        -- Install mock and build tool.
        compile_loop._test_set_llm_call(mock_llm)

        local td = compile_loop.make({
            runner = lazy_runner,
            llm = { provider = "openai", api_key = "mock-key" },
            target_files = TARGET_FILES,
            edit_mode = "diff",
            max_iters = 10,
        })

        -- Invoke the handler directly (no real agent.run, no API key needed).
        local handle_ok, result_json = pcall(td.handler, {
            spec = "Add version comments to each oai lazy file.",
            target_files = TARGET_FILES,
        })

        -- Restore production llm_call unconditionally.
        compile_loop._test_reset_llm_call()

        -- handler must not raise.
        if not handle_ok then
            error(
                "td.handler raised: "
                    .. tostring(result_json)
                    .. "\n  mock.call_count="
                    .. mock_state.call_count
                    .. " runner_call_count="
                    .. runner_call_count
            )
        end

        -- Decode result JSON.
        local dec_ok, tool_output = pcall(std.json.decode, result_json)
        if not dec_ok or type(tool_output) ~= "table" then
            error("result JSON decode failed: " .. tostring(result_json))
        end

        -- Diagnostic log (visible on failure).
        local size_str = {}
        for i, sz in ipairs(mock_state.messages_size_log) do
            size_str[i] = "c" .. i .. "=" .. sz
        end
        local diag = string.format(
            "ok=%s iters=%s failure_reason=%s summary=%s | mock.calls=%d runner.calls=%d max_size=%d | sizes: %s",
            tostring(tool_output.ok),
            tostring(tool_output.iters),
            tostring(tool_output.failure_reason),
            tostring(tool_output.summary),
            mock_state.call_count,
            runner_call_count,
            mock_state.max_seen_size,
            table.concat(size_str, " ")
        )
        io.write("[DIAG] " .. diag .. "\n")

        -- (alpha) At least 3 read_file dispatches.
        -- Each odd llm_call returns 1 tool_use block → 1 dispatch in run_loop.
        -- Odd calls = ceil(call_count / 2).
        local dispatches = math.ceil(mock_state.call_count / 2)
        expect(dispatches >= 3).to.be.truthy("(alpha) need >= 3 read_file dispatches, got " .. dispatches)

        -- (beta) Completed successfully or via stagnation/max_iters give-up.
        local beta_ok = (tool_output.ok == true)
            or (tool_output.failure_reason == "stagnation")
            or (tool_output.failure_reason == "max_iters")
        expect(beta_ok).to.be.truthy("(beta) unexpected failure_reason=" .. tostring(tool_output.failure_reason))

        -- (gamma) All messages[] sizes within 24K chars.
        expect(mock_state.max_seen_size <= 24000).to.be.truthy(
            "(gamma) max messages size " .. mock_state.max_seen_size .. " > 24000"
        )
    end)

    -- ── Regression: forced-fail stagnation path (mirrors multi_stagnation e2e) ──
    -- Verifies that subtask 1+2 changes did not break the stagnation detection path.
    -- Uses a mock LLM that always returns the same SR block (identical sr_hash),
    -- triggering is_stagnant_v2 after STAGNATION_WINDOW=3 iterations.
    -- oai_ prefix avoids /tmp/ collision with anthropic version on concurrent runs.
    it("stagnation regression — forced-fail runner triggers stagnation give-up", function()
        local TF_A = "/tmp/oai_stag_reg_a.lua"
        local TF_B = "/tmp/oai_stag_reg_b.lua"
        write_file(TF_A, "-- oai_stag_a_v0\n")
        write_file(TF_B, "-- oai_stag_b_v0\n")

        -- Constant SR block: same content every call → same sr_hash → triggers stagnation.
        local CONST_SR = "<<< path="
            .. TF_A
            .. " >>>\n<<<<<<< SEARCH\n-- oai_stag_a_v0\n=======\n-- oai_stag_a_patched\n>>>>>>> REPLACE\n"

        local stag_call_n = 0
        local function stag_mock(_opts, _messages)
            stag_call_n = stag_call_n + 1
            -- Always return the same SR block (same sr_hash after normalisation).
            return { choices = { { message = { content = CONST_SR } } } }
        end

        -- Runner always fails (forced-fail, mirrors test_anthropic_compile_loop_multi_stagnation.lua).
        local stag_runner_n = 0
        local function stag_runner(_file_paths)
            stag_runner_n = stag_runner_n + 1
            return { ok = false, stdout = "", stderr = "FORCED_FAIL", exit_code = 1 }
        end

        compile_loop._test_set_llm_call(stag_mock)

        local stag_td = compile_loop.make({
            runner = stag_runner,
            llm = { provider = "openai", api_key = "mock-key" },
            target_files = { TF_A, TF_B },
            edit_mode = "diff",
            max_iters = 10,
        })

        local stag_ok, stag_json = pcall(stag_td.handler, {
            spec = "Patch oai_stag_a.",
            target_files = { TF_A, TF_B },
        })

        compile_loop._test_reset_llm_call()

        if not stag_ok then
            error("stagnation handler raised: " .. tostring(stag_json))
        end

        local stag_dec_ok, stag_out = pcall(std.json.decode, stag_json)
        if not stag_dec_ok or type(stag_out) ~= "table" then
            error("stagnation result decode failed: " .. tostring(stag_json))
        end

        io.write(
            string.format(
                "[DIAG stagnation] ok=%s failure_reason=%s iters=%s runner_n=%d\n",
                tostring(stag_out.ok),
                tostring(stag_out.failure_reason),
                tostring(stag_out.iters),
                stag_runner_n
            )
        )

        -- ok must be false.
        expect(stag_out.ok).to_not.equal(true)

        -- failure_reason must be stagnation or max_iters (stagnation is expected here,
        -- but max_iters is also acceptable per subtask-3 spec).
        local stag_reason_ok = (stag_out.failure_reason == "stagnation") or (stag_out.failure_reason == "max_iters")
        expect(stag_reason_ok).to.be.truthy("expected stagnation|max_iters, got " .. tostring(stag_out.failure_reason))

        -- Note: runner may not be called at all when apply_blocks fails every iter
        -- (SEARCH text mismatch after first patch → all_failed path).
        -- is_stagnant_v2 fires on repeated sr_hash regardless of runner invocation.
        -- We only assert iters > 0 (the loop ran at least one iteration).
        expect((stag_out.iters or 0) > 0).to.be.truthy("expected iters > 0, got " .. tostring(stag_out.iters))
    end)
end)
