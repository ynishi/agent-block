-- empty_summary.lua — replay fixture: the log a summarising request is built
-- from, and the beats after it, for `policy.compact`.
--
-- NOT a spec: it declares no suite and calls nothing in the spec framework,
-- which is how the runner decides what to run (crates/lua-spec-runner/src/
-- main.rs, `is_spec`); `replay_spec.lua` reaches it with
-- `require("policy.spec.replay.empty_summary")`.
--
-- Where it comes from: exported from the log of a real run (see `source`)
-- with `agent-block knl export --as events`, seq 1-44, and converted as
-- `replay_spec.lua`'s header describes. Seq 1-30 is the record the run's
-- first summarising beat (seq 31-32) folded its request from; seq 33 is the
-- summary it wrote; seq 34-44 the two beats after it, the second of which
-- made the next compaction due.
--
-- What this run did not do is come back empty: it ran after the summarising
-- request carried its tool history as prose, and every summary it asked for
-- was written. The empty answer is the one measured before that change
-- (CHANGELOG [Unreleased]: gemini-2.5-flash answered a request carrying
-- `tool_use` / `tool_result` blocks with nothing at all), and the case puts
-- the `compact_skipped` mark a loop writes for it in the summary's place
-- itself — the mark is RECONSTRUCTED in the spec, the events here are not.
-- No length here is measured by the case: `due` reads the window reports
-- recorded on each `llm_request`, which are kept whole.

--- Filler of `n` bytes: a recorded string kept only for its rendered length.
local function fill(n)
    return string.rep("x", n)
end

return {
    source = "a real run against gemini-2.5-flash on 2026-10-02 (window 8000, max_tokens 2048, result_share 0.3, beat_share 0.5, compact at 0.6 / min_beats 2): seq 1-44",
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
    },
}
