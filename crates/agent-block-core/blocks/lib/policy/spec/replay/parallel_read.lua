-- parallel_read.lua — replay fixture: the beat in which a model asked for six
-- reads at once, and `policy.beat_cap` refused three of them.
--
-- NOT a spec: it declares no suite and calls nothing in the spec framework,
-- which is how the runner decides what to run (crates/lua-spec-runner/src/
-- main.rs, `is_spec`); `replay_spec.lua` reaches it with
-- `require("policy.spec.replay.parallel_read")`, as the policy specs reach
-- `policy.spec.support`.
--
-- Where it comes from: exported from the log of a real run (see `source`)
-- with `agent-block knl export --as events`, seq 1-20 — the seed and the run's
-- first beat — and converted as `replay_spec.lua`'s header describes: the
-- kinds only the kernel writes dropped, the repo root rewritten to /repo,
-- `config` kept as its room and model name, an `llm_request` kept as its
-- window report, beat ids renamed, long text cut to a stand-in.
--
-- What it keeps whole: what `beat_cap` measures. Every tool_result in the
-- beat, and every answer below, has its `content` replaced by `fill(n)`, n
-- the length of the recorded content as JSON escapes it, so a result renders
-- to the bytes it rendered to in the run and the room's count gives the
-- tokens the run counted. The refusals are recorded verbatim — their
-- `tokens` / `used` / `limit` are what the case checks the replay against.
--
-- `answers` is what each handler answered, by call id: the run recorded a
-- refusal in place of a refused read, so the answer behind it comes from the
-- same file read on the next beat (beta, zeta — the counts agree with the
-- refusals), and for gamma, which was refused on both beats that asked for it
-- and never read, it is RECONSTRUCTED: a stand-in sized to the 1,598 tokens
-- its refusal names. `from` says which is which.

--- Filler of `n` bytes: a recorded string kept only for its rendered length.
local function fill(n)
    return string.rep("x", n)
end

return {
    source = "a real run against gemini-2.5-flash on 2026-10-02 (window 8000, max_tokens 2048, result_share 0.3, beat_share 0.5, compact at 0.6 / min_beats 2): seq 1-20",
    answers = {
        ["function-call-2064794409716433018"] = {
            from = "seq 18, this beat's own result",
            result = {
                content = fill(1362),
                end_line = 74,
                start_line = 1,
                total = 74,
                version = "2d8e5bc7fd368887",
            },
        },
        ["function-call-2064794409716433621"] = {
            from = "seq 29, the same file read on the next beat (445 tokens, as the refusal at seq 20 says)",
            result = {
                content = fill(1317),
                end_line = 74,
                start_line = 1,
                total = 74,
                version = "18b7a938f70c654f",
            },
        },
        ["function-call-2064794409716434702"] = {
            from = "seq 10, this beat's own result",
            result = {
                content = fill(5004),
                end_line = 244,
                start_line = 1,
                total = 244,
                version = "f069368350671114",
            },
        },
        ["function-call-2064794409716435305"] = {
            from = "seq 25, the same file read on the next beat (1585 tokens, as the refusal at seq 12 says)",
            result = {
                content = fill(4963),
                end_line = 244,
                start_line = 1,
                total = 244,
                version = "82ff2a9f1c68aad6",
            },
        },
        ["function-call-2064794409716435908"] = {
            from = "reconstructed: gamma was refused on both beats that asked for it, so its answer is a stand-in sized to the 1598 tokens its refusal at seq 14 names",
            result = {
                content = fill(5004),
                end_line = 244,
                start_line = 1,
                total = 244,
                version = "0000000000000000",
            },
        },
        ["function-call-2064794409716436511"] = {
            from = "seq 16, this beat's own result",
            result = {
                content = fill(1332),
                end_line = 74,
                start_line = 1,
                total = 74,
                version = "b5a932be4211ee1e",
            },
        },
    },
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
                    content = fill(5004),
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
                    error = "'fs_read' answered 1585 tokens, and the results of this turn already take 1598 of the 2976 one turn may take together — the whole conversation has to fit the model's window. Ask for this on your next turn, after the results you have.",
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
                    error = "'fs_read' answered 1598 tokens, and the results of this turn already take 1706 of the 2976 one turn may take together — the whole conversation has to fit the model's window. Ask for this on your next turn, after the results you have.",
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
                    content = fill(1332),
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
                    content = fill(1362),
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
                    error = "'fs_read' answered 445 tokens, and the results of this turn already take 2723 of the 2976 one turn may take together — the whole conversation has to fit the model's window. Ask for this on your next turn, after the results you have.",
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
    },
}
