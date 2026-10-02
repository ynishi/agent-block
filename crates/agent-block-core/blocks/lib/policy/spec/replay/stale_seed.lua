-- stale_seed.lua — replay fixture: a run whose seed says the build is
-- failing and whose log says the last verify passed, for
-- `policy.window{ from_summary }`.
--
-- NOT a spec: it declares no suite and calls nothing in the spec framework,
-- which is how the runner decides what to run (crates/lua-spec-runner/src/
-- main.rs, `is_spec`); `replay_spec.lua` reaches it with
-- `require("policy.spec.replay.stale_seed")`.
--
-- Where it comes from: exported from the log of a real run (see `source`)
-- with `agent-block knl export --as events`, seq 1-82, and converted as
-- `replay_spec.lua`'s header describes. The seed (seq 3-5) is a red verify
-- and the task message carrying "Current build status: FAILING"; three
-- summaries follow (seq 33, 48, 70), each with its ledger; the verify turns
-- green at seq 81 and the harness says so at seq 82.
--
-- In this run the verify turned green after the last summary, so no recorded
-- summary carries a ledger that says it passed. The case appends the next
-- summary the way the loop writes one — its ledger read off these events by
-- `policy.ledger` — and that summary is the only part not exported. The
-- failure it replays is the one measured on 2026-10-01 (CHANGELOG [0.41.0]:
-- after a compaction a model read the seed's "FAILING" as current while the
-- ledger beside it said the last verify passed).

--- Filler of `n` bytes: a recorded string kept only for its rendered length.
local function fill(n)
    return string.rep("x", n)
end

return {
    source = "a real run against gemini-2.5-flash on 2026-10-02 (window 8000, max_tokens 2048, result_share 0.3, beat_share 0.5, compact at 0.6 / min_beats 2): seq 1-82",
    events = {
        {
            data = {
                exit_code = 1,
                ok = false,
                ran = true,
                stderr = "lua5.4: test/run.lua:10: strutil.titlecase is missing\nstack traceback:\n\t[C]: in function 'assert'\n\ttest/run.lua:10: in main chunk\n\t[C]: in ?\n",
                stdout = "",
                timeout_s = 60,
            },
            kind = "verify",
            seq = 3,
        },
        {
            data = {
                content = "Two changes. (1) In src/strutil.lua add `M.titlecase(s)`: trim the string, then make every word start with an upper-case letter and the rest lower-case, words separated by single spaces. (2) In src/report.lua make `M.heading(name)` return the titlecased name wrapped as `== Name ==`. Before editing, read every helper file (src/alpha.lua, src/beta.lua, src/gamma.lua, src/delta.lua, src/epsilon.lua, src/zeta.lua) and confirm none of them already defines a titlecase helper you should reuse.\n\n## Target files (edit these; read the parts you need first — their content is not included here)\n/repo/src/strutil.lua\n/repo/src/report.lua\n/repo/src/alpha.lua\n/repo/src/beta.lua\n/repo/src/gamma.lua\n/repo/src/delta.lua\n/repo/src/epsilon.lua\n/repo/src/zeta.lua\n\n\n## Current build status: FAILING\nThe verify command ALREADY fails on the current state of the files, before any edit of yours. Fix these errors FIRST — the output names the lines to edit:\n\nlua5.4: test/run.lua:10: strutil.titlecase is missing\nstack traceback:\n\t[C]: in function 'assert'\n\ttest/run.lua:10: in main chunk\n\t[C]: in ?\n",
            },
            kind = "msg_user",
            meta = { label = "spec" },
            seq = 4,
        },
        {
            data = {
                room = {
                    beat_budget = 2976,
                    held = 0,
                    limit = 5952,
                    max_output = 2048,
                    reserve = 0,
                    result_limit = 1785,
                    window = 8000,
                },
                values = { model = { from = "caller", value = "gemini-2.5-flash" } },
            },
            kind = "config",
            seq = 5,
        },
        {
            data = {
                window = {
                    after = 2395,
                    before = 2395,
                    dropped = {},
                    kept = 0,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-01" },
            seq = 7,
        },
        {
            data = {
                content = {
                    {
                        id = "function-call-2064794409716434702",
                        input = { path = "/repo/src/alpha.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                    {
                        id = "function-call-2064794409716435305",
                        input = { path = "/repo/src/beta.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                    {
                        id = "function-call-2064794409716435908",
                        input = { path = "/repo/src/gamma.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                    {
                        id = "function-call-2064794409716436511",
                        input = { path = "/repo/src/delta.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                    {
                        id = "function-call-2064794409716433018",
                        input = { path = "/repo/src/epsilon.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                    {
                        id = "function-call-2064794409716433621",
                        input = { path = "/repo/src/zeta.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                },
                finish_reason = "tool_calls",
                stop_reason = "tool_use",
                usage = { input_tokens = 2302, output_tokens = 270, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-01" },
            seq = 8,
        },
        {
            data = {
                args = { path = "/repo/src/alpha.lua" },
                call_id = "function-call-2064794409716434702",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-01" },
            seq = 9,
        },
        {
            data = {
                call_id = "function-call-2064794409716434702",
                ok = true,
                result = {
                    content = "-- alpha: small helpers, one per function, nothing shared.\nlocal M = {}\n\n--- alp [... cut; 4761 bytes in the run]",
                    end_line = 244,
                    start_line = 1,
                    total = 244,
                    version = "f069368350671114",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-01" },
            seq = 10,
        },
        {
            data = {
                args = { path = "/repo/src/beta.lua" },
                call_id = "function-call-2064794409716435305",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-01" },
            seq = 11,
        },
        {
            data = {
                call_id = "function-call-2064794409716435305",
                ok = true,
                result = {
                    error = "'fs_read' answered 1585 tokens, and the results of this turn already take 1598 o [... cut; 235 bytes in the run]",
                    limit = 2976,
                    ok = false,
                    reason = "beat_budget",
                    tokens = 1585,
                    used = 1598,
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-01" },
            seq = 12,
        },
        {
            data = {
                args = { path = "/repo/src/gamma.lua" },
                call_id = "function-call-2064794409716435908",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-01" },
            seq = 13,
        },
        {
            data = {
                call_id = "function-call-2064794409716435908",
                ok = true,
                result = {
                    error = "'fs_read' answered 1598 tokens, and the results of this turn already take 1706 o [... cut; 235 bytes in the run]",
                    limit = 2976,
                    ok = false,
                    reason = "beat_budget",
                    tokens = 1598,
                    used = 1706,
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-01" },
            seq = 14,
        },
        {
            data = {
                args = { path = "/repo/src/delta.lua" },
                call_id = "function-call-2064794409716436511",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-01" },
            seq = 15,
        },
        {
            data = {
                call_id = "function-call-2064794409716436511",
                ok = true,
                result = {
                    content = "-- delta: small helpers, one per function, nothing shared.\nlocal M = {}\n\n--- del [... cut; 1259 bytes in the run]",
                    end_line = 74,
                    start_line = 1,
                    total = 74,
                    version = "b5a932be4211ee1e",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-01" },
            seq = 16,
        },
        {
            data = {
                args = { path = "/repo/src/epsilon.lua" },
                call_id = "function-call-2064794409716433018",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-01" },
            seq = 17,
        },
        {
            data = {
                call_id = "function-call-2064794409716433018",
                ok = true,
                result = {
                    content = "-- epsilon: small helpers, one per function, nothing shared.\nlocal M = {}\n\n--- e [... cut; 1289 bytes in the run]",
                    end_line = 74,
                    start_line = 1,
                    total = 74,
                    version = "2d8e5bc7fd368887",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-01" },
            seq = 18,
        },
        {
            data = {
                args = { path = "/repo/src/zeta.lua" },
                call_id = "function-call-2064794409716433621",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-01" },
            seq = 19,
        },
        {
            data = {
                call_id = "function-call-2064794409716433621",
                ok = true,
                result = {
                    error = "'fs_read' answered 445 tokens, and the results of this turn already take 2723 of [... cut; 234 bytes in the run]",
                    limit = 2976,
                    ok = false,
                    reason = "beat_budget",
                    tokens = 445,
                    used = 2723,
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-01" },
            seq = 20,
        },
        {
            data = {
                window = {
                    after = 5593,
                    before = 5593,
                    dropped = {},
                    kept = 1,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-02" },
            seq = 22,
        },
        {
            data = {
                content = {
                    {
                        id = "function-call-8509317854273785897",
                        input = { path = "/repo/src/beta.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                    {
                        id = "function-call-8509317854273789690",
                        input = { path = "/repo/src/gamma.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                    {
                        id = "function-call-8509317854273789387",
                        input = { path = "/repo/src/zeta.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                },
                finish_reason = "tool_calls",
                stop_reason = "tool_use",
                usage = { input_tokens = 6310, output_tokens = 135, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-02" },
            seq = 23,
        },
        {
            data = {
                args = { path = "/repo/src/beta.lua" },
                call_id = "function-call-8509317854273785897",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-02" },
            seq = 24,
        },
        {
            data = {
                call_id = "function-call-8509317854273785897",
                ok = true,
                result = {
                    content = "-- beta: small helpers, one per function, nothing shared.\nlocal M = {}\n\n--- beta [... cut; 4720 bytes in the run]",
                    end_line = 244,
                    start_line = 1,
                    total = 244,
                    version = "82ff2a9f1c68aad6",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-02" },
            seq = 25,
        },
        {
            data = {
                args = { path = "/repo/src/gamma.lua" },
                call_id = "function-call-8509317854273789690",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-02" },
            seq = 26,
        },
        {
            data = {
                call_id = "function-call-8509317854273789690",
                ok = true,
                result = {
                    error = "'fs_read' answered 1598 tokens, and the results of this turn already take 1585 o [... cut; 235 bytes in the run]",
                    limit = 2976,
                    ok = false,
                    reason = "beat_budget",
                    tokens = 1598,
                    used = 1585,
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-02" },
            seq = 27,
        },
        {
            data = {
                args = { path = "/repo/src/zeta.lua" },
                call_id = "function-call-8509317854273789387",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-02" },
            seq = 28,
        },
        {
            data = {
                call_id = "function-call-8509317854273789387",
                ok = true,
                result = {
                    content = "-- zeta: small helpers, one per function, nothing shared.\nlocal M = {}\n\n--- zeta [... cut; 1244 bytes in the run]",
                    end_line = 74,
                    start_line = 1,
                    total = 74,
                    version = "18b7a938f70c654f",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-02" },
            seq = 29,
        },
        {
            data = {
                window = {
                    after = 3239,
                    before = 6436,
                    dropped = { "beat-01" },
                    kept = 1,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-03" },
            seq = 31,
        },
        {
            data = {
                content = {
                    {
                        text = "The user wants two changes:\n1. Add `M.titlecase(s)` to `src/strutil.lua`: trim t [... cut; 1104 bytes in the run]",
                        type = "text",
                    },
                },
                finish_reason = "stop",
                stop_reason = "end_turn",
                usage = { input_tokens = 3768, output_tokens = 420, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-03" },
            seq = 32,
        },
        {
            data = {
                content = "The user wants two changes:\n1. Add `M.titlecase(s)` to `src/strutil.lua`: trim t [... cut; 1104 bytes in the run]",
                ledger = {
                    beats = 3,
                    calls = { fs_read = 9 },
                    checks = { verify = { count = 1, last_ok = false, passed = 0 } },
                    summaries = 0,
                    touched = {
                        fs_read = {
                            path = {
                                "/repo/src/alpha.lua",
                                "/repo/src/beta.lua",
                                "/repo/src/delta.lua",
                                "/repo/src/epsilon.lua",
                                "/repo/src/gamma.lua",
                                "/repo/src/zeta.lua",
                            },
                        },
                    },
                },
            },
            kind = "summary",
            meta = { label = "compact", reason = "share" },
            seq = 33,
        },
        {
            data = {
                window = {
                    after = 3090,
                    before = 3090,
                    dropped = {},
                    kept = 0,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-04" },
            seq = 35,
        },
        {
            data = {
                content = {
                    {
                        id = "function-call-6724460390614736188",
                        input = { path = "/repo/src/epsilon.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                },
                finish_reason = "tool_calls",
                stop_reason = "tool_use",
                usage = { input_tokens = 3024, output_tokens = 45, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-04" },
            seq = 36,
        },
        {
            data = {
                args = { path = "/repo/src/epsilon.lua" },
                call_id = "function-call-6724460390614736188",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-04" },
            seq = 37,
        },
        {
            data = {
                call_id = "function-call-6724460390614736188",
                ok = true,
                result = {
                    content = "-- epsilon: small helpers, one per function, nothing shared.\nlocal M = {}\n\n--- e [... cut; 1289 bytes in the run]",
                    end_line = 74,
                    start_line = 1,
                    total = 74,
                    version = "2d8e5bc7fd368887",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-04" },
            seq = 38,
        },
        {
            data = {
                content = "You have been reading without editing; old reads are already gone. Apply an edit NOW with fs_search_replace to the region you most recently read.",
            },
            kind = "msg_user",
            seq = 39,
        },
        {
            data = {
                window = {
                    after = 3670,
                    before = 3670,
                    dropped = {},
                    kept = 1,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-05" },
            seq = 41,
        },
        {
            data = {
                content = {
                    {
                        id = "function-call-7737652335346363336",
                        input = { path = "/repo/src/strutil.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                },
                finish_reason = "tool_calls",
                stop_reason = "tool_use",
                usage = { input_tokens = 3699, output_tokens = 46, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-05" },
            seq = 42,
        },
        {
            data = {
                args = { path = "/repo/src/strutil.lua" },
                call_id = "function-call-7737652335346363336",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-05" },
            seq = 43,
        },
        {
            data = {
                call_id = "function-call-7737652335346363336",
                ok = true,
                result = {
                    content = "-- strutil: string helpers for the report.\nlocal M = {}\n\n--- Trim whitespace at  [... cut; 172 bytes in the run]",
                    end_line = 9,
                    start_line = 1,
                    total = 9,
                    version = "5bd92547b7e7ce02",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-05" },
            seq = 44,
        },
        {
            data = {
                window = {
                    after = 2350,
                    before = 2350,
                    dropped = {},
                    kept = 2,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-06" },
            seq = 46,
        },
        {
            data = {
                content = {
                    {
                        text = "Summary:\nThe user wants to add a `M.titlecase(s)` function to `src/strutil.lua`  [... cut; 1231 bytes in the run]",
                        type = "text",
                    },
                },
                finish_reason = "stop",
                stop_reason = "end_turn",
                usage = { input_tokens = 2454, output_tokens = 373, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-06" },
            seq = 47,
        },
        {
            data = {
                content = "Summary:\nThe user wants to add a `M.titlecase(s)` function to `src/strutil.lua`  [... cut; 1231 bytes in the run]",
                ledger = {
                    beats = 6,
                    calls = { fs_read = 11 },
                    checks = { verify = { count = 1, last_ok = false, passed = 0 } },
                    summaries = 1,
                    touched = {
                        fs_read = {
                            path = {
                                "/repo/src/alpha.lua",
                                "/repo/src/beta.lua",
                                "/repo/src/delta.lua",
                                "/repo/src/epsilon.lua",
                                "/repo/src/gamma.lua",
                                "/repo/src/strutil.lua",
                                "/repo/src/zeta.lua",
                            },
                        },
                    },
                },
            },
            kind = "summary",
            meta = { label = "compact", reason = "share" },
            seq = 48,
        },
        {
            data = {
                window = {
                    after = 3097,
                    before = 3097,
                    dropped = {},
                    kept = 0,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-07" },
            seq = 50,
        },
        {
            data = {
                content = {
                    {
                        id = "function-call-11303286528027742791",
                        input = {
                            edits = {
                                {
                                    replace = 'function M.trim(s)\n  return s:match("^%s*(.*%S?)%s*$") or ""\nend\n\nfunction M.tit [... cut; 259 bytes in the run]',
                                    search = 'function M.trim(s)\n  return s:match("^%s*(.*%S?)%s*$") or ""\nend',
                                },
                            },
                            path = "/repo/src/strutil.lua",
                        },
                        name = "fs_search_replace",
                        type = "tool_use",
                    },
                },
                finish_reason = "tool_calls",
                stop_reason = "tool_use",
                usage = { input_tokens = 3013, output_tokens = 225, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-07" },
            seq = 51,
        },
        {
            data = {
                args = {
                    edits = {
                        {
                            replace = 'function M.trim(s)\n  return s:match("^%s*(.*%S?)%s*$") or ""\nend\n\nfunction M.tit [... cut; 259 bytes in the run]',
                            search = 'function M.trim(s)\n  return s:match("^%s*(.*%S?)%s*$") or ""\nend',
                        },
                    },
                    path = "/repo/src/strutil.lua",
                },
                call_id = "function-call-11303286528027742791",
                name = "fs_search_replace",
            },
            kind = "tool_call",
            meta = { beat = "beat-07" },
            seq = 52,
        },
        {
            data = {
                call_id = "function-call-11303286528027742791",
                ok = true,
                result = {
                    edit_index = 1,
                    failures = {
                        {
                            actual = 'function M.trim(s)\n    return (s:gsub("^%s+", ""):gsub("%s+$", ""))\nend',
                            edit_index = 1,
                            matched_prefix_lines = 1,
                            reason = "search_not_found",
                        },
                    },
                    ok = false,
                    reason = "search_not_found",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-07" },
            seq = 53,
        },
        {
            data = {
                window = {
                    after = 3359,
                    before = 3359,
                    dropped = {},
                    kept = 1,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-08" },
            seq = 55,
        },
        {
            data = {
                content = {
                    {
                        id = "function-call-17219139053103225365",
                        input = { path = "/repo/src/strutil.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                },
                finish_reason = "tool_calls",
                stop_reason = "tool_use",
                usage = { input_tokens = 3450, output_tokens = 46, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-08" },
            seq = 56,
        },
        {
            data = {
                args = { path = "/repo/src/strutil.lua" },
                call_id = "function-call-17219139053103225365",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-08" },
            seq = 57,
        },
        {
            data = {
                call_id = "function-call-17219139053103225365",
                ok = true,
                result = {
                    content = "-- strutil: string helpers for the report.\nlocal M = {}\n\n--- Trim whitespace at  [... cut; 172 bytes in the run]",
                    end_line = 9,
                    start_line = 1,
                    total = 9,
                    version = "5bd92547b7e7ce02",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-08" },
            seq = 58,
        },
        {
            data = {
                content = "You have been reading without editing; old reads are already gone. Apply an edit NOW with fs_search_replace to the region you most recently read.",
            },
            kind = "msg_user",
            seq = 59,
        },
        {
            data = {
                window = {
                    after = 3572,
                    before = 3572,
                    dropped = {},
                    kept = 2,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-09" },
            seq = 61,
        },
        {
            data = {
                content = {
                    {
                        id = "function-call-6545448074308676284",
                        input = {
                            base = "5bd92547b7e7ce02",
                            edits = {
                                {
                                    replace = 'function M.trim(s)\n    return (s:gsub("^%s+", ""):gsub("%s+$", ""))\nend\n\nfunctio [... cut; 266 bytes in the run]',
                                    search = 'function M.trim(s)\n    return (s:gsub("^%s+", ""):gsub("%s+$", ""))\nend',
                                },
                            },
                            path = "/repo/src/strutil.lua",
                        },
                        name = "fs_search_replace",
                        type = "tool_use",
                    },
                },
                finish_reason = "tool_calls",
                stop_reason = "tool_use",
                usage = { input_tokens = 3552, output_tokens = 250, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-09" },
            seq = 62,
        },
        {
            data = {
                args = {
                    base = "5bd92547b7e7ce02",
                    edits = {
                        {
                            replace = 'function M.trim(s)\n    return (s:gsub("^%s+", ""):gsub("%s+$", ""))\nend\n\nfunctio [... cut; 266 bytes in the run]',
                            search = 'function M.trim(s)\n    return (s:gsub("^%s+", ""):gsub("%s+$", ""))\nend',
                        },
                    },
                    path = "/repo/src/strutil.lua",
                },
                call_id = "function-call-6545448074308676284",
                name = "fs_search_replace",
            },
            kind = "tool_call",
            meta = { beat = "beat-09" },
            seq = 63,
        },
        {
            data = {
                call_id = "function-call-6545448074308676284",
                ok = true,
                result = {
                    applied = 1,
                    net_line_delta = 10,
                    ok = true,
                    version = "2845b525870f468f",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-09" },
            seq = 64,
        },
        {
            data = {
                exit_code = 1,
                ok = false,
                ran = true,
                stderr = 'FAIL heading uses titlecase with == fences: expected "== Quarterly Report ==", got "quarterly report"\n',
                stdout = "",
                timeout_s = 60,
            },
            kind = "verify",
            meta = { beat = "beat-09" },
            seq = 65,
        },
        {
            data = {
                content = 'The verify still fails, as it did before you started:\nFAIL heading uses titlecase with == fences: expected "== Quarterly Report ==", got "quarterly report"\n',
            },
            kind = "msg_user",
            seq = 66,
        },
        {
            data = {
                window = {
                    after = 2364,
                    before = 2364,
                    dropped = {},
                    kept = 3,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-10" },
            seq = 68,
        },
        {
            data = {
                content = {
                    {
                        text = "Summary:\nThe user wants to implement `M.titlecase(s)` in `src/strutil.lua` and t [... cut; 1307 bytes in the run]",
                        type = "text",
                    },
                },
                finish_reason = "stop",
                stop_reason = "end_turn",
                usage = { input_tokens = 2444, output_tokens = 377, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-10" },
            seq = 69,
        },
        {
            data = {
                content = "Summary:\nThe user wants to implement `M.titlecase(s)` in `src/strutil.lua` and t [... cut; 1307 bytes in the run]",
                ledger = {
                    beats = 10,
                    calls = { fs_read = 12, fs_search_replace = 2 },
                    checks = { verify = { count = 2, last_ok = false, passed = 0 } },
                    summaries = 2,
                    touched = {
                        fs_read = {
                            path = {
                                "/repo/src/alpha.lua",
                                "/repo/src/beta.lua",
                                "/repo/src/delta.lua",
                                "/repo/src/epsilon.lua",
                                "/repo/src/gamma.lua",
                                "/repo/src/strutil.lua",
                                "/repo/src/zeta.lua",
                            },
                        },
                        fs_search_replace = { path = { "/repo/src/strutil.lua" } },
                    },
                },
            },
            kind = "summary",
            meta = { label = "compact", reason = "share" },
            seq = 70,
        },
        {
            data = {
                window = {
                    after = 3161,
                    before = 3161,
                    dropped = {},
                    kept = 0,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-11" },
            seq = 72,
        },
        {
            data = {
                content = {
                    {
                        id = "function-call-11787983188132431835",
                        input = { path = "/repo/src/report.lua" },
                        name = "fs_read",
                        type = "tool_use",
                    },
                },
                finish_reason = "tool_calls",
                stop_reason = "tool_use",
                usage = { input_tokens = 3065, output_tokens = 45, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-11" },
            seq = 73,
        },
        {
            data = {
                args = { path = "/repo/src/report.lua" },
                call_id = "function-call-11787983188132431835",
                name = "fs_read",
            },
            kind = "tool_call",
            meta = { beat = "beat-11" },
            seq = 74,
        },
        {
            data = {
                call_id = "function-call-11787983188132431835",
                ok = true,
                result = {
                    content = '-- report: renders a heading for a section name.\nlocal strutil = require("src.st [... cut; 225 bytes in the run]',
                    end_line = 10,
                    start_line = 1,
                    total = 10,
                    version = "cf57f14c145f9731",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-11" },
            seq = 75,
        },
        {
            data = {
                window = {
                    after = 3340,
                    before = 3340,
                    dropped = {},
                    kept = 1,
                    limit = 5952,
                    reserve = 0,
                    seed_kept = true,
                },
            },
            kind = "llm_request",
            meta = { beat = "beat-12" },
            seq = 77,
        },
        {
            data = {
                content = {
                    {
                        id = "function-call-6737118862254217941",
                        input = {
                            base = "cf57f14c145f9731",
                            edits = {
                                {
                                    replace = '    return "== " .. strutil.titlecase(name) .. " =="',
                                    search = "    return strutil.trim(name)",
                                },
                            },
                            path = "/repo/src/report.lua",
                        },
                        name = "fs_search_replace",
                        type = "tool_use",
                    },
                },
                finish_reason = "tool_calls",
                stop_reason = "tool_use",
                usage = { input_tokens = 3239, output_tokens = 104, thinking_tokens = 0 },
            },
            kind = "llm_response",
            meta = { beat = "beat-12" },
            seq = 78,
        },
        {
            data = {
                args = {
                    base = "cf57f14c145f9731",
                    edits = {
                        {
                            replace = '    return "== " .. strutil.titlecase(name) .. " =="',
                            search = "    return strutil.trim(name)",
                        },
                    },
                    path = "/repo/src/report.lua",
                },
                call_id = "function-call-6737118862254217941",
                name = "fs_search_replace",
            },
            kind = "tool_call",
            meta = { beat = "beat-12" },
            seq = 79,
        },
        {
            data = {
                call_id = "function-call-6737118862254217941",
                ok = true,
                result = {
                    applied = 1,
                    net_line_delta = 0,
                    ok = true,
                    version = "6b6bd5f22fe7accf",
                },
            },
            kind = "tool_result",
            meta = { beat = "beat-12" },
            seq = 80,
        },
        {
            data = {
                exit_code = 0,
                ok = true,
                ran = true,
                stderr = "ok\n",
                stdout = "",
                timeout_s = 60,
            },
            kind = "verify",
            meta = { beat = "beat-12" },
            seq = 81,
        },
        { data = { content = "The verify passes." }, kind = "msg_user", seq = 82 },
    },
}
