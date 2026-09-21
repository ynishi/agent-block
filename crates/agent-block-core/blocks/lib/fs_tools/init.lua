-- blocks/lib/fs_tools/init.lua — the Lua half of the `std.fs` bridge.
--
-- `src/bridge/fs.rs` registers the Rust half and then `require`s this module by
-- name, so the tier order every other module follows applies here too: a
-- project's `.agent-block/lib/fs_tools/` wins, this source is the fallback.
-- `agent-block vendor fs_tools` therefore changes the tool surface a model
-- is handed with no install in between.
--
-- Defines std.fs.tool_specs(opts) / std.fs.register_tools(opts) — the
-- LLM-facing file tools.
--
-- opts:
--   allowed   : array of op names  — REQUIRED. "read", "write", "edit",
--                                   "rollback", "search_replace"
--   prefix    : tool name prefix   (default: "fs_")
--   path_lock : array of paths     (restricts every op to these files; the
--                                   model cannot reach anything else)
--   limits    : { result_tokens, count } — what one result may cost, from
--                                   `policy.result_budget`; optional
--
-- Returns: array of registered tool names.
--
-- `allowed` has no default, and there is no "opt-in" tier among the ops.
-- Which tools a model is handed is the caller's decision and nobody else's:
-- a default here means a caller that said nothing still gets a tool surface
-- someone else picked, and the caller then has no way to know what its model
-- is holding. `{"read","edit"}` was that default, which handed out the
-- line-addressed edit even though the note below records it losing to
-- `search_replace` on a real task — and the loop compensated by TELLING the
-- model in prose which tool to call, which is how a spec's own "read this
-- file first" came to be overridden by the harness
-- [実測 2026-09-12: model は ref を読もうとして 8 回以上ためらった末、
--  "the harness says to start this reply with an fs_search_replace call" を
--  理由に read を捨てた]。
--
-- The tool set is the control. Name it.
--
-- `write` is the one to think twice about: whole-file replacement is how a
-- model silently discards code it did not think to reproduce. That is a reason
-- to weigh it, not a reason for this file to withhold it.
--
-- `search_replace` is the same edit addressed differently: the model names a
-- verbatim snippet instead of a line range, and the handler finds the snippet
-- in the file as it is now and hands `std.fs.edit` the line range and the
-- `expect` text that snippet implies. Nothing is applied that `edit` would
-- not apply. It exists because some models cannot produce an exact `expect`
-- for lines they have read — they reconstruct it from memory and churn on
-- `expect_mismatch` — while copying a snippet they have just seen is within
-- reach; on one real-repository task the line-addressed form did not reach
-- green in four runs and this form did in one to three, three times out of
-- three. Which form a loop offers is the caller's choice: it is a tool spec,
-- not a policy.

-- Turn a snippet matched at `pos` into the line-addressed edit `std.fs.edit`
-- takes. The match is widened to whole lines: `expect` is the exact text of
-- the lines the snippet touches, read off the same content the match was
-- found in, so it cannot disagree with the file; the replacement keeps
-- whatever stood before and after the snippet on its first and last line.
--
-- A snippet that ends with a newline reaches the start of the following line
-- without including any of it; the line address stops at the newline, and a
-- replacement that also ends with one is trimmed to match, so the edit does
-- not leave an extra blank line behind.
-- Where a `search` that did not match stops agreeing with the file, and what
-- the file says from there.
--
-- "not found" on its own is the one failure the caller cannot act on: it says
-- the guess was wrong without saying what is actually there, so a model that
-- believes the code reads one way re-sends the same belief. The sibling
-- failures already do better — `search_ambiguous` returns the match count,
-- `result_too_large` returns the size and the limit — and this closes the
-- last one [実測 2026-09-11: `collect::<Vec<u32>>()` を 4 回送り続けた run。
--  file の実体は model 自身が前の編集で書いた `collect::<Vec<u32>()))` で、
--  読み幅を 1378..1383 → 1379..1382 と狭めて 6 回目にやっと写せた]。
--
-- The longest whole-line prefix of `search` that does occur is found by
-- halving the line count, then the file's own text from that point is
-- returned — as many lines as the search had, so the caller sees the region
-- in the shape it tried to name. Returns `nil` when not even the first line
-- occurs: there is no region to point at, and the caller should re-read.
local function diverges_at(content, search)
    local lines = {}
    for line in (search .. "\n"):gmatch("([^\n]*)\n") do
        lines[#lines + 1] = line
    end
    if #lines > 0 and lines[#lines] == "" then
        table.remove(lines)
    end
    if #lines == 0 then
        return nil, nil
    end

    local function prefix_pos(n)
        return content:find(table.concat(lines, "\n", 1, n), 1, true)
    end
    if not prefix_pos(1) then
        return nil, nil
    end
    local lo, hi = 1, #lines -- prefix_pos(lo) matched, prefix_pos(hi+1) did not
    while lo < hi do
        local mid = lo + math.ceil((hi - lo) / 2)
        if prefix_pos(mid) then
            lo = mid
        else
            hi = mid - 1
        end
    end

    -- Take the file's own text from where the matching prefix starts, for the
    -- number of lines the search claimed, so the difference is visible in place.
    local start = prefix_pos(lo)
    -- `()` captures the position OF the last newline, so the line after it
    -- starts one byte later.
    local nl = content:sub(1, start - 1):match(".*()\n")
    local line_start = nl and (nl + 1) or 1
    local out, at = {}, line_start
    for _ = 1, #lines do
        local eol = content:find("\n", at, true)
        out[#out + 1] = content:sub(at, eol and eol - 1 or #content)
        if not eol then
            break
        end
        at = eol + 1
    end
    return lo, table.concat(out, "\n")
end

-- When not even the first line of `search` occurs, `diverges_at` has no
-- region to point at — and that is the case a model working from memory
-- lands in: it paraphrases a line it has in the seed, and the paraphrase
-- shares its words with the real line but not its text
-- [実測 2026-09-13 dmdeclar beat 1: seed の line 501 は
--  `assert_eq!(ScheduleKind::parse("triangular"), None);` なのに、model は
--  `assert!(ScheduleKind::parse("triangular").is_none());` を anchor に書いた。
--  `actual` は出ず、次 beat は EOF 越えの read で 1 手、さらに 1 手で当てた]。
-- The file's line that shares the most identifiers with the search's first
-- line, ties broken by the longer common prefix; nil when nothing shares
-- two identifiers (one word in common points at nothing in particular).
local function nearest_line(content, search)
    local first = search:match("^[^\n]*") or ""
    local want, n_want = {}, 0
    for tok in first:gmatch("[%w_]+") do
        if #tok >= 3 and not want[tok] then
            want[tok] = true
            n_want = n_want + 1
        end
    end
    if n_want == 0 then
        return nil
    end
    local function prefix_len(a, b)
        local n = math.min(#a, #b)
        for i = 1, n do
            if a:byte(i) ~= b:byte(i) then
                return i - 1
            end
        end
        return n
    end
    local trimmed_first = first:match("^%s*(.-)%s*$")
    local best, best_score, best_prefix, best_text = nil, 0, 0, nil
    local lineno = 0
    for line in (content .. "\n"):gmatch("([^\n]*)\n") do
        lineno = lineno + 1
        local seen, score = {}, 0
        for tok in line:gmatch("[%w_]+") do
            if want[tok] and not seen[tok] then
                seen[tok] = true
                score = score + 1
            end
        end
        if score >= 2 then
            local p = prefix_len(trimmed_first, line:match("^%s*(.-)%s*$"))
            if score > best_score or (score == best_score and p > best_prefix) then
                best, best_score, best_prefix, best_text = lineno, score, p, line
            end
        end
    end
    if best == nil then
        return nil
    end
    return { line = best, text = best_text, shared_words = best_score }
end

-- `{` minus `}` in a text: a search / replace pair whose balance differs
-- moves a brace, and the tool says so rather than letting the verify find
-- the mismatch two beats later. Counts, not a judgement — braces in strings
-- count too, and an edit that means to open or close a block is legitimate
-- [実測 2026-09-13 dmdeclar beat 4: 1 行の anchor の直後に `}` と test 2 本を
--  足し、既存 test の残り (NAMES loop) が関数の外に出た。error は
--  "unexpected closing delimiter" で、修復に 2 beat].
local function brace_balance(text)
    local _, open = tostring(text or ""):gsub("{", "")
    local _, close = tostring(text or ""):gsub("}", "")
    return open - close
end

local function line_edit_for(content, pos, search, replace)
    local end_pos = pos + #search - 1
    if search:sub(-1) == "\n" and replace:sub(-1) == "\n" then
        replace = replace:sub(1, -2)
    end
    local before = content:sub(1, pos - 1)
    local last_nl = before:match(".*()\n")
    local span_start = last_nl and (last_nl + 1) or 1
    local nl_after = content:find("\n", end_pos, true)
    local span_end = nl_after and (nl_after - 1) or #content
    local expect = content:sub(span_start, span_end)
    local prefix = content:sub(span_start, pos - 1)
    local suffix = content:sub(end_pos + 1, span_end)
    local _, newlines_before = content:sub(1, span_start - 1):gsub("\n", "")
    local _, newlines_within = expect:gsub("\n", "")
    local start_line = 1 + newlines_before
    return {
        start_line = start_line,
        end_line = start_line + newlines_within,
        expect = expect,
        replace = prefix .. replace .. suffix,
    }
end

-- Build the tool definitions without touching the global registry.
--
-- Registration is global, but `path_lock` is per-caller: a block that
-- registers its own scoped tools inside a longer-lived VM would leak them to
-- whatever else runs there, carrying whichever lock the last caller set.
-- Callers that own the VM use `register_tools`; callers that hand the specs to
-- one LLM call use this and dispatch the handlers themselves.
--
-- Returns an array of { name, description, input_schema, handler }.
std.fs.tool_specs = function(opts)
    opts = opts or {}
    local allowed = opts.allowed
    if type(allowed) ~= "table" or #allowed == 0 then
        error("std.fs.tool_specs: `allowed` is required — name the ops this caller hands its model", 2)
    end
    local prefix = opts.prefix or "fs_"
    local path_lock = opts.path_lock

    -- What one result may cost, handed down by the shell that knows the
    -- window (`policy.result_budget`): how many tokens a result may take, and
    -- how to count them.
    --
    -- The tool is told rather than the shell clipping afterwards, because
    -- only the tool knows the result is a range of lines — a shell that sees
    -- a rendered string can cut it, but cannot say which line the cut landed
    -- on or where to resume. Before this the shell refused the whole call,
    -- which left the model nothing to look at and no way forward, and the
    -- limit appeared in neither the description nor the input_schema, so
    -- nobody knew it was there until it fired
    -- [実測 2026-09-12: `result_cap` は `{ok=false, reason="result_too_large"}`
    --  を返すだけだった]。
    --
    -- The one measured improvement in this area is the resume line: a read
    -- that reports the range returned, the total, what is left and the next
    -- offset cut a task from 6 tool calls to 2 (GPT-5.5) and 3 to 2
    -- (Sonnet 4.6) [langchain-ai/deepagents#4540]. Returning a small fixed
    -- page instead invites the opposite — agents "exhaustively call next
    -- through every match" [SWE-agent, arXiv:2405.15793 Table 3] — so the cut
    -- is at the budget, never at a page size.
    local budget_tokens, budget_count
    if type(opts.limits) == "table" then
        local rt = opts.limits.result_tokens
        if type(rt) == "function" then
            budget_tokens = rt
        elseif type(rt) == "number" then
            budget_tokens = function()
                return rt
            end
        end
        if type(opts.limits.count) == "function" then
            budget_count = opts.limits.count
        end
        if budget_tokens and not budget_count then
            error("std.fs.tool_specs: limits.result_tokens needs limits.count to measure against", 2)
        end
    end

    --- The largest whole-line prefix of `lines` that costs at most `budget`.
    --- Bisects because a measurement may cost a round trip to the server that
    --- owns the tokenizer — the same reason `policy.window` bisects.
    --- @return number  how many lines fit; 0 when not even the first does
    --- `render(k)` must answer the text the SHELL will measure for a result of
    --- `k` lines — the whole encoded table, not the content alone. Measuring
    --- the content and returning a table costs the difference, and the shell
    --- then refuses a result the tool believed it had fitted
    --- [実測 2026-09-12: content を 768 tok に収めたのに、metadata と JSON
    ---  escaping を足した実体は 914 tok で `result_too_large` になった]。
    local function fit_lines(lines, budget, count, render)
        local function cost(k)
            return count(render(k))
        end
        if #lines == 0 or cost(#lines) <= budget then
            return #lines
        end
        local lo, hi, best = 1, #lines, 0
        while lo <= hi do
            local mid = (lo + hi) // 2
            if cost(mid) <= budget then
                best, lo = mid, mid + 1
            else
                hi = mid - 1
            end
        end
        return best
    end

    local lock_set = nil
    local lock_list = nil
    if path_lock and #path_lock > 0 then
        lock_set = {}
        for _, p in ipairs(path_lock) do
            lock_set[p] = true
        end
        lock_list = table.concat(path_lock, ", ")
    end

    -- Returns nil when the path is allowed, or an error table when it is not.
    --
    -- **Two different mistakes reach here, and saying only "not allowed" hides
    -- which one happened.** A caller that named a file outside the lock has to
    -- pick a different file; a caller that left `path` out entirely has to add
    -- the argument — and to that one, a list of allowed paths reads as a
    -- contradiction: the path it meant is right there in the list, so the
    -- refusal looks like a bug in the tool rather than a missing field.
    --
    -- [measured 2026-09-11: a model called fs_read with `{start_line, end_line}`
    --  and no `path`, was told `path_not_allowed` with the allowed list, and
    --  spent the next beat reasoning that "the allowed_paths list includes
    --  exactly that path, yet the tool is saying the path is not permitted —
    --  this is strange". Two beats and ~10k tokens went to a missing argument.]
    --
    -- So name which one it is, and say what to do about it.
    --- Every refusal, on one line, at debug.
    ---
    --- The reason a call was turned away lived only inside the returned table,
    --- which reaches the model but nowhere a human reads: working out that
    --- `no_edits` and `path_missing` were the same event (a call cut at the
    --- output ceiling) meant opening `knl.sqlite` per run
    --- [実測 2026-09-11: 11 件すべて out=4096 の beat だったと分かるまで、
    ---  run dir を 1 本ずつ SQL で舐めていた]。
    local function refused(res, ctx)
        log.debug(
            "[fs refused] "
                .. tostring(res.reason)
                .. " "
                .. tostring(ctx or "")
                .. (res.matches and (" matches=" .. tostring(res.matches)) or "")
                .. (res.edit_index and (" edit_index=" .. tostring(res.edit_index)) or "")
        )
        return res
    end

    local function check_path(path)
        if path == nil or path == "" then
            return refused({
                ok = false,
                reason = "path_missing",
                allowed_paths = path_lock,
                error = "No `path` argument was given. Every call needs the absolute path of the "
                    .. "file to act on, alongside any line range."
                    .. (lock_list and (" Use: " .. lock_list) or ""),
            }, "path=<none>")
        end
        if lock_set and not lock_set[path] then
            return refused({
                ok = false,
                reason = "path_not_allowed",
                path = path,
                allowed_paths = path_lock,
                error = string.format(
                    "'%s' is not one of the files this task may touch. Use %s exactly as written.",
                    tostring(path),
                    lock_list or "an allowed path"
                ),
            }, "path=" .. tostring(path))
        end
        return nil
    end

    local function path_prop()
        local desc = "Absolute path of the file."
        if lock_list then
            desc = desc .. " Must be one of: " .. lock_list
        end
        return { type = "string", description = desc }
    end

    local defs = {
        read = {
            description = "Read a file, or a range of its lines. Returns { content, start_line, end_line, "
                .. "total, version }: the range actually returned and the file's total line count, the "
                .. "way an HTTP Content-Range says start-end/total. `version` identifies the exact "
                .. "content read — pass it back as `base` when editing so the edit is rejected if the "
                .. "file changed meanwhile. Line numbers are 1-based, inclusive, and are what "
                .. prefix
                .. "edit addresses. A result cut short (by `limit`"
                .. (budget_count and ", by `max_tokens`" or "")
                .. (budget_tokens and ", or by the share of the window one result may take" or "")
                .. ") carries `truncated = true` and `cut_by`; `end_line` is the last line it holds.",
            input_schema = {
                type = "object",
                properties = (function()
                    local props = {
                        path = path_prop(),
                        start_line = {
                            type = "integer",
                            description = "First line to return, 1-based (default 1).",
                        },
                        end_line = {
                            type = "integer",
                            description = "Last line to return, inclusive (default: the last line of the file).",
                        },
                        limit = {
                            type = "integer",
                            description = "At most this many lines in this result.",
                        },
                    }
                    if budget_count then
                        props.max_tokens = {
                            type = "integer",
                            description = "At most this many tokens in this result"
                                .. (budget_tokens and " (the window share applies as well; the smaller wins)" or "")
                                .. ".",
                        }
                    end
                    return props
                end)(),
                required = { "path" },
            },
            handler = function(input)
                local denied = check_path(input.path)
                if denied then
                    return denied
                end
                local res = std.fs.read_versioned(input.path)
                local total = res.lines
                local first = tonumber(input.start_line) or 1
                local last = tonumber(input.end_line) or total
                if res.content == "" then
                    -- An empty file is a range of nothing, not a bad range
                    -- (`read_versioned` counts it as one line; here it is 0 of 0).
                    if input.start_line ~= nil and first > 1 then
                        return refused({
                            ok = false,
                            reason = "range_invalid",
                            total = 0,
                            error = string.format(
                                "start_line=%s is not inside an empty file.",
                                tostring(input.start_line)
                            ),
                        }, "path=" .. tostring(input.path))
                    end
                    return { content = "", version = res.version, start_line = 1, end_line = 0, total = 0 }
                end
                if first < 1 or last < first or first > total then
                    return refused({
                        ok = false,
                        reason = "range_invalid",
                        total = total,
                        error = string.format(
                            "start_line=%s end_line=%s is not a range inside a %d-line file.",
                            tostring(input.start_line),
                            tostring(input.end_line),
                            total
                        ),
                    }, "path=" .. tostring(input.path))
                end
                if last > total then
                    last = total
                end
                local out = {}
                local n = 0
                for line in (res.content .. "\n"):gmatch("(.-)\n") do
                    n = n + 1
                    if n >= first and n <= last then
                        out[#out + 1] = line
                    end
                end
                -- What cut the result, if anything. `truncated` says that it was cut;
                -- `end_line` and `total` say where and how much there is. No "next"
                -- offset: it is `end_line + 1`, and saying it twice adds nothing.
                local cut_by = nil
                local limit = tonumber(input.limit)
                if limit and limit >= 1 and #out > limit then
                    for i = #out, limit + 1, -1 do
                        out[i] = nil
                    end
                    cut_by = "limit"
                end
                local function result_of(k, cb)
                    local stop = k > 0 and (first + k - 1) or (first - 1)
                    return {
                        content = table.concat(out, "\n", 1, k),
                        version = res.version,
                        start_line = first,
                        end_line = stop,
                        total = total,
                        truncated = cb ~= nil or nil,
                        cut_by = cb,
                    }
                end
                -- Token budget: the caller's `max_tokens` and the shell's share, the
                -- smaller wins. Only measurable when a counter was handed in.
                local budget, budget_from = nil, nil
                if budget_tokens then
                    budget, budget_from = budget_tokens(), "budget"
                end
                local want = tonumber(input.max_tokens)
                if want and budget_count and (budget == nil or want < budget) then
                    budget, budget_from = want, "max_tokens"
                end
                if budget then
                    -- Measure the result as it will be returned (metadata and JSON
                    -- escaping included), which is what the shell measures too.
                    local kept = fit_lines(out, budget, budget_count, function(k)
                        return std.json.encode(result_of(k, budget_from))
                    end)
                    if kept < #out then
                        if kept == 0 then
                            return refused({
                                ok = false,
                                reason = "result_too_large",
                                budget = budget,
                                total = total,
                                error = string.format(
                                    "Line %d alone is over the %d-token budget one result may take. "
                                        .. "Nothing can be returned from here.",
                                    first,
                                    budget
                                ),
                            }, "path=" .. tostring(input.path) .. " line=" .. tostring(
                                first
                            ))
                        end
                        for i = #out, kept + 1, -1 do
                            out[i] = nil
                        end
                        cut_by = budget_from
                    end
                end
                return result_of(#out, cut_by)
            end,
        },

        edit = {
            description = "Replace one or more line ranges in a file. Addressed by line "
                .. "number, not by searching for text: give `start_line`, `end_line` and the "
                .. "`expect`ed current text of those lines. **Every line number in the call "
                .. "addresses the file as it is now, before any of these edits.** When you "
                .. "send several, do not shift the later ones to account for lines an earlier "
                .. "one adds or removes — they are all checked and applied against one "
                .. "reading, as a set. Every edit is checked before any is applied, so a "
                .. "rejected call changes nothing. On `expect_mismatch` the reply carries the "
                .. "text actually at those lines, and `failures` lists every edit that did "
                .. "not pass — correct them from that instead of re-reading the file. Several "
                .. "of them off by the same number of lines means the numbers were counted "
                .. "against the edited file; re-count against the file as you read it. Pass "
                .. "`base` from the read to be told when the file changed under you. There is "
                .. "no fuzzy matching: `expect` must be exact.",
            input_schema = {
                type = "object",
                properties = {
                    path = path_prop(),
                    base = {
                        type = "string",
                        description = "The `version` from the read this edit is based on. "
                            .. "Omit only if you have not read the file in this turn.",
                    },
                    edits = {
                        type = "array",
                        description = "Edits to apply together. Every range addresses the "
                            .. "file as it is now; none of them shifts to account for another. "
                            .. "Ranges must not overlap.",
                        items = {
                            type = "object",
                            properties = {
                                start_line = {
                                    type = "integer",
                                    description = "1-based first line to replace, in the file as it is now.",
                                },
                                end_line = {
                                    type = "integer",
                                    description = "1-based last line to replace, inclusive, in the file "
                                        .. "as it is now.",
                                },
                                expect = {
                                    type = "string",
                                    description = "Exact current text of those lines as the file stands "
                                        .. "now, newline-joined, without a trailing newline.",
                                },
                                replace = {
                                    type = "string",
                                    description = "Replacement text. Empty string deletes the lines.",
                                },
                            },
                            required = { "start_line", "end_line", "expect", "replace" },
                        },
                    },
                },
                required = { "path", "edits" },
            },
            handler = function(input)
                local denied = check_path(input.path)
                if denied then
                    return denied
                end
                return std.fs.edit(input.path, { base = input.base, edits = input.edits })
            end,
        },

        search_replace = {
            description = "Edit a file by exact text replacement. Each edit names a `search` "
                .. "snippet — the current text, copied verbatim from what you read (whitespace "
                .. "and blank lines count), a few lines long, occurring exactly once in the "
                .. "file — and the `replace` text that takes its place. No line numbers. The "
                .. "call is carried out as "
                .. prefix
                .. "edit against the file as it is now, with the same checks: every edit is "
                .. "resolved before any is applied, a rejected call changes nothing, and two "
                .. "edits may not touch the same line. `search_not_found` means the text is "
                .. "not in the file as it is now; `actual` is what the file says where your "
                .. "search stopped agreeing with it (`matched_prefix_lines` is how many of "
                .. "your lines did match). Copy from `actual` rather than re-sending what you "
                .. "believe the code says. `actual` is absent when not even your first line "
                .. "occurs; then `nearest` (when present) is the file line that shares the most "
                .. "words with your first line, with its line number — read around it, or re-read "
                .. "the region. "
                .. "`search_ambiguous` means it occurs more than once and says how many in "
                .. "`matches` and on which lines in `at_lines` — add surrounding lines until it "
                .. "is one. A successful call may carry a `note`: `shrank` when a replace has fewer "
                .. "lines than its search, `brace_shift` when it changes the `{` minus `}` count "
                .. "— an edit that appends after a block must include the block's closing brace "
                .. "in `search`, or the text that followed lands on the wrong side of it. `failures` lists every "
                .. "edit that did not resolve, so correct them together; several missing at "
                .. "once means the region moved, and one re-read of it fixes the whole call. "
                .. "Pass `base` from the read to be told when the file changed under you.",
            input_schema = {
                type = "object",
                properties = {
                    path = path_prop(),
                    base = {
                        type = "string",
                        description = "The `version` from the read this edit is based on. "
                            .. "Omit only if you have not read the file in this turn.",
                    },
                    edits = {
                        type = "array",
                        description = "Edits resolved together against the current file. Each "
                            .. "search must be unique in the file and no two may touch the same line.",
                        items = {
                            type = "object",
                            properties = {
                                search = {
                                    type = "string",
                                    description = "Verbatim current text, unique in the file.",
                                },
                                replace = {
                                    type = "string",
                                    description = "Replacement text. Empty string deletes the snippet.",
                                },
                            },
                            required = { "search", "replace" },
                        },
                    },
                },
                required = { "path", "edits" },
            },
            handler = function(input)
                local denied = check_path(input.path)
                if denied then
                    return denied
                end
                local edits = input.edits
                if type(edits) ~= "table" or #edits == 0 then
                    return refused({ ok = false, reason = "no_edits" }, "path=" .. tostring(input.path))
                end
                -- Every search is resolved against one reading of the file,
                -- then handed over as one batch: the edit primitive's own
                -- overlap check and `base` check decide together whether
                -- anything is written.
                --
                -- Every search is resolved before any refusal is returned, and
                -- the reply names all of them. This is the batch that is
                -- supposed to be a batch — a snippet identifies itself, so
                -- several in one call is coherent where several line ranges
                -- are not — and it is therefore the one where several can be
                -- wrong at once: a caller working from a stale reading has
                -- every snippet from the changed region miss together. Naming
                -- one of them sends it back to re-read for a single line; the
                -- set is what shows the region moved.
                local content = std.fs.read_versioned(input.path).content
                local translated = {}
                local failures = {}
                for i, e in ipairs(edits) do
                    local search = type(e) == "table" and e.search or nil
                    local replace = type(e) == "table" and e.replace or nil
                    if type(search) ~= "string" or search == "" or type(replace) ~= "string" then
                        table.insert(failures, { reason = "bad_edit", edit_index = i })
                    else
                        local pos = content:find(search, 1, true)
                        if not pos then
                            local at, actual = diverges_at(content, search)
                            table.insert(failures, {
                                reason = "search_not_found",
                                edit_index = i,
                                matched_prefix_lines = at,
                                actual = actual,
                                -- Only when there is no `actual`: a region
                                -- that partly matched is a better pointer
                                -- than a line that shares some words.
                                nearest = (actual == nil) and nearest_line(content, search) or nil,
                            })
                        else
                            -- Count the rest rather than stopping at the second:
                            -- "occurs 4 times" tells the caller how much context
                            -- to add, where "more than once" does not. And say
                            -- where: the line of each occurrence, so the caller
                            -- can pick the one it meant instead of guessing
                            -- [実測 2026-09-13 dmdclref beat 2: `    }\n}` が 2 箇所
                            --  (impl の末尾と mod tests の末尾)。model は数から
                            --  場所を推理して当てたが、行番号があれば推理は要らない].
                            local function line_of(p)
                                local _, n = content:sub(1, p - 1):gsub("\n", "")
                                return n + 1
                            end
                            local matches, at_lines = 1, { line_of(pos) }
                            local at = content:find(search, pos + #search, true)
                            while at do
                                matches = matches + 1
                                if #at_lines < 10 then
                                    at_lines[#at_lines + 1] = line_of(at)
                                end
                                at = content:find(search, at + #search, true)
                            end
                            if matches > 1 then
                                table.insert(failures, {
                                    reason = "search_ambiguous",
                                    edit_index = i,
                                    matches = matches,
                                    at_lines = at_lines,
                                })
                            else
                                translated[i] = line_edit_for(content, pos, search, replace)
                            end
                        end
                    end
                end
                if #failures > 0 then
                    local first = failures[1]
                    return refused({
                        ok = false,
                        reason = first.reason,
                        edit_index = first.edit_index,
                        matches = first.matches,
                        failures = failures,
                    }, string.format(
                        "path=%s edits=%d failed=%d",
                        tostring(input.path),
                        #edits,
                        #failures
                    ))
                end
                -- Say how the file's shape changed, per edit and in total.
                --
                -- A `replace` shorter than its `search` deletes the lines the
                -- model did not reproduce, and nothing says so: the answer is
                -- `applied = n` either way, and the loss surfaces one verify
                -- later as a compile error pointing somewhere else entirely
                -- [実測 2026-09-11 ST1 run 182520: 1 件の edit が search に
                --  含めた `opts` / `cursor` / `}` を replace に書き戻さず、
                --  struct の閉じ括弧ごと消えた。error は 1568 行目の
                --  "unclosed delimiter" で、消えた場所は 280 行目付近。
                --  その 1 件で run (受入 test 6 本を書き上げていた) が全損]。
                -- Counts, not a judgement: the tool does not refuse a shrink —
                -- deleting code is a legitimate edit — it only states it.
                local shrank, net, braces = {}, 0, {}
                for i, e in ipairs(edits) do
                    local function lines_of(t)
                        local _, n = tostring(t or ""):gsub("\n", "")
                        return n + 1
                    end
                    local from, to = lines_of(e.search), lines_of(e.replace)
                    net = net + (to - from)
                    if to < from then
                        shrank[#shrank + 1] = string.format("edit %d: %d->%d lines", i, from, to)
                    end
                    local shift = brace_balance(e.replace) - brace_balance(e.search)
                    if shift ~= 0 then
                        braces[#braces + 1] = string.format("edit %d: %+d", i, shift)
                    end
                end
                local res = std.fs.edit(input.path, { base = input.base, edits = translated })
                if type(res) == "table" and res.ok ~= false then
                    res.net_line_delta = net
                    local notes = {}
                    if #shrank > 0 then
                        res.shrank = table.concat(shrank, "; ")
                        notes[#notes + 1] = "Some replacements are shorter than the text they replaced ("
                            .. res.shrank
                            .. "). If that was not intended, the lines you left out of `replace` are now gone "
                            .. "— re-read the region and put them back."
                        log.debug("[fs shrink] " .. tostring(input.path) .. " " .. res.shrank .. " net=" .. net)
                    end
                    if #braces > 0 then
                        res.brace_shift = table.concat(braces, "; ")
                        notes[#notes + 1] = "Some replacements change the count of `{` minus `}` against the text they "
                            .. "replaced ("
                            .. res.brace_shift
                            .. "). If the edit was meant to add code after a block, the block's closing brace "
                            .. "belongs inside `search` — as it stands, the text that followed your search is "
                            .. "now on the other side of a brace. Re-read the region if that was not intended."
                        log.debug("[fs braces] " .. tostring(input.path) .. " " .. res.brace_shift)
                    end
                    if #notes > 0 then
                        res.note = table.concat(notes, " ")
                    end
                end
                return res
            end,
        },

        write = {
            description = "Write a file in full, replacing whatever was there. Prefer "
                .. prefix
                .. "edit unless you are creating the file or intend to discard its "
                .. "current contents entirely.",
            input_schema = {
                type = "object",
                properties = {
                    path = path_prop(),
                    content = { type = "string", description = "Full new file content." },
                },
                required = { "path", "content" },
            },
            handler = function(input)
                local denied = check_path(input.path)
                if denied then
                    return denied
                end
                std.fs.write(input.path, input.content)
                return { ok = true }
            end,
        },

        rollback = {
            description = "Restore a file to the content it had before the last successful "
                .. prefix
                .. "edit. Use to discard an edit you have decided against.",
            input_schema = {
                type = "object",
                properties = { path = path_prop() },
                required = { "path" },
            },
            handler = function(input)
                local denied = check_path(input.path)
                if denied then
                    return denied
                end
                return std.fs.rollback(input.path)
            end,
        },
    }

    local specs = {}
    for _, op in ipairs(allowed) do
        local def = defs[op]
        if def then
            table.insert(specs, {
                name = prefix .. op,
                description = def.description,
                input_schema = def.input_schema,
                handler = def.handler,
            })
        end
    end
    return specs
end

-- Register the same tools into the global tool registry, for callers that own
-- the VM and want them visible to `tool.schema()` / `tool.call`.
--
-- Returns: array of registered tool names.
std.fs.register_tools = function(opts)
    local registered = {}
    for _, spec in ipairs(std.fs.tool_specs(opts)) do
        tool.register(spec.name, { description = spec.description, input_schema = spec.input_schema }, spec.handler)
        table.insert(registered, spec.name)
    end
    return registered
end
