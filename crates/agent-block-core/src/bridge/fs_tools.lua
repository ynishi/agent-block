-- Embedded Lua source loaded by src/bridge/fs.rs.
-- Defines std.fs.register_tools(opts?) — LLM-facing tool registration helper.
--
-- opts (all optional):
--   allowed   : array of op names  (default: {"read","edit"}; "write",
--                                   "rollback" and "search_replace" are opt-in)
--   prefix    : tool name prefix   (default: "fs_")
--   path_lock : array of paths     (restricts every op to these files; the
--                                   model cannot reach anything else)
--
-- Returns: array of registered tool names.
--
-- `write` is not in the default set: whole-file replacement is how a model
-- silently discards code it did not think to reproduce. Callers that want it
-- must ask.
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
    local allowed = opts.allowed or { "read", "edit" }
    local prefix = opts.prefix or "fs_"
    local path_lock = opts.path_lock

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
    local function check_path(path)
        if lock_set and not lock_set[path] then
            return {
                ok = false,
                reason = "path_not_allowed",
                path = path,
                allowed_paths = path_lock,
            }
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
            description = "Read a file. Returns { content, lines, version }. "
                .. "`version` identifies the exact content read — pass it back as `base` "
                .. "when editing so the edit is rejected if the file changed meanwhile. "
                .. "Line numbers in the result are 1-based and are what "
                .. prefix
                .. "edit addresses.",
            input_schema = {
                type = "object",
                properties = {
                    path = path_prop(),
                    start_line = {
                        type = "integer",
                        description = "Optional 1-based first line to return (default: whole file).",
                    },
                    end_line = {
                        type = "integer",
                        description = "Optional 1-based last line to return, inclusive.",
                    },
                },
                required = { "path" },
            },
            handler = function(input)
                local denied = check_path(input.path)
                if denied then
                    return denied
                end
                local res = std.fs.read_versioned(input.path)
                if not input.start_line then
                    return res
                end
                -- Slice without losing the version, which still refers to the
                -- whole file (that is what `base` is compared against).
                local out = {}
                local n = 0
                local first = input.start_line
                local last = input.end_line or first
                for line in (res.content .. "\n"):gmatch("(.-)\n") do
                    n = n + 1
                    if n >= first and n <= last then
                        table.insert(out, line)
                    end
                end
                return {
                    content = table.concat(out, "\n"),
                    lines = res.lines,
                    version = res.version,
                    start_line = first,
                    end_line = last,
                }
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
                .. "not in the file as it is now — re-read that region rather than guessing. "
                .. "`search_ambiguous` means it occurs more than once and says how many in "
                .. "`matches` — add surrounding lines until it is one. `failures` lists every "
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
                    return { ok = false, reason = "no_edits" }
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
                            table.insert(failures, { reason = "search_not_found", edit_index = i })
                        else
                            -- Count the rest rather than stopping at the second:
                            -- "occurs 4 times" tells the caller how much context
                            -- to add, where "more than once" does not.
                            local matches = 1
                            local at = content:find(search, pos + #search, true)
                            while at do
                                matches = matches + 1
                                at = content:find(search, at + #search, true)
                            end
                            if matches > 1 then
                                table.insert(failures, {
                                    reason = "search_ambiguous",
                                    edit_index = i,
                                    matches = matches,
                                })
                            else
                                translated[i] = line_edit_for(content, pos, search, replace)
                            end
                        end
                    end
                end
                if #failures > 0 then
                    local first = failures[1]
                    return {
                        ok = false,
                        reason = first.reason,
                        edit_index = first.edit_index,
                        matches = first.matches,
                        failures = failures,
                    }
                end
                return std.fs.edit(input.path, { base = input.base, edits = translated })
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
