-- Embedded Lua source loaded by src/bridge/fs.rs.
-- Defines std.fs.register_tools(opts?) — LLM-facing tool registration helper.
--
-- opts (all optional):
--   allowed   : array of op names  (default: {"read","edit"}; "write" is opt-in)
--   prefix    : tool name prefix   (default: "fs_")
--   path_lock : array of paths     (restricts every op to these files; the
--                                   model cannot reach anything else)
--
-- Returns: array of registered tool names.
--
-- `write` is not in the default set: whole-file replacement is how a model
-- silently discards code it did not think to reproduce. Callers that want it
-- must ask.

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
                .. "`expect`ed current text of those lines. Every edit is checked before any "
                .. "is applied, so a rejected call changes nothing. On `expect_mismatch` the "
                .. "reply carries the text actually at those lines — correct it from that "
                .. "instead of re-reading the file. Pass `base` from the read to be told when "
                .. "the file changed under you. There is no fuzzy matching: `expect` must be exact.",
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
                        description = "Edits to apply together. Ranges must not overlap.",
                        items = {
                            type = "object",
                            properties = {
                                start_line = { type = "integer", description = "1-based first line to replace." },
                                end_line = {
                                    type = "integer",
                                    description = "1-based last line to replace, inclusive.",
                                },
                                expect = {
                                    type = "string",
                                    description = "Exact current text of those lines, newline-joined, "
                                        .. "without a trailing newline.",
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
