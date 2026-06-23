-- Embedded Lua source loaded by src/bridge/ts.rs.
-- Defines std.ts.register_tools(opts?) — LLM-facing tool registration helper.
--
-- opts (all optional):
--   allowed : array of op names   (default: {"append", "query", "last"})
--   prefix  : tool name prefix    (default: "ts_")
--
-- Returns: array of registered tool names.
--
-- Notes on sum/avg:
--   The `value` column is stored as a JSON-encoded payload.  SQLite's
--   CAST(value AS REAL) treats JSON objects as 0.0, so `sum`/`avg` produce
--   meaningful results only when the series contains numeric values.  Use
--   number-only series for aggregate operations.
--
-- Tag key restriction:
--   Tag keys must match [a-zA-Z0-9_]+ (ASCII alphanumeric and underscore).
--   This restriction guards against SQL injection via json_extract paths.

std.ts.register_tools = function(opts)
    opts = opts or {}
    local allowed = opts.allowed or { "append", "query", "last" }
    local prefix = opts.prefix or "ts_"

    local defs = {
        append = {
            description = "Append a data point to a named time-series stream backed by "
                .. "the agent's local SQLite database (persists across runs, agent-private). "
                .. "`value` can be a number (e.g. a metric) or a table (e.g. a structured "
                .. "MCP envelope). Both are stored as JSON and round-trip without loss. "
                .. "`tags` is an optional flat object used to label the point; keys must "
                .. "match [a-zA-Z0-9_]+. `at` is an optional Unix timestamp in milliseconds; "
                .. "defaults to the current wall-clock time.",
            input_schema = {
                type = "object",
                properties = {
                    series = {
                        type = "string",
                        description = "Logical stream name (e.g. \"cpu_load\", \"agent_events\").",
                    },
                    value = {
                        description = "Data point payload: a number or a table (JSON-encoded).",
                    },
                    tags = {
                        type = "object",
                        description = "Optional flat label object. Keys: [a-zA-Z0-9_]+ only.",
                        additionalProperties = { type = "string" },
                    },
                    at = {
                        type = "integer",
                        description = "Optional Unix timestamp in milliseconds. Defaults to now.",
                    },
                },
                required = { "series", "value" },
            },
            handler = function(input)
                std.ts.append(input.series, input.value, input.tags, input.at)
                return { ok = true }
            end,
        },

        query = {
            description = "Query a time-series stream from the agent's local SQLite database. "
                .. "Returns an array of rows. Raw mode (no `agg`): each row is "
                .. "{ ts, value, tags }. Single-aggregate mode (`agg` without `bucket_ms`): "
                .. "returns one row { value } with the scalar result. Time-bucketed mode "
                .. "(`agg` + `bucket_ms`): each row is { bucket_ts, value }. "
                .. "Supported agg values: \"count\", \"sum\", \"avg\", \"last\". "
                .. "sum/avg interpret `value` as a number via CAST; rows with object values "
                .. "contribute 0.0. Tag filtering uses a conjunction of json_extract checks "
                .. "(AND semantics); rows without tags never match a tag filter.",
            input_schema = {
                type = "object",
                properties = {
                    series = {
                        type = "string",
                        description = "Stream name to query.",
                    },
                    opts = {
                        type = "object",
                        description = "Query options.",
                        properties = {
                            from = {
                                type = "integer",
                                description = "Start of time range (Unix ms, inclusive). "
                                    .. "Default: beginning of time.",
                            },
                            to = {
                                type = "integer",
                                description = "End of time range (Unix ms, inclusive). "
                                    .. "Default: end of time.",
                            },
                            tags = {
                                type = "object",
                                description = "AND-filter: all key-value pairs must match "
                                    .. "(json_extract per key). Keys: [a-zA-Z0-9_]+.",
                                additionalProperties = { type = "string" },
                            },
                            agg = {
                                type = "string",
                                description = "Aggregation function: "
                                    .. "\"count\" | \"sum\" | \"avg\" | \"last\".",
                                enum = { "count", "sum", "avg", "last" },
                            },
                            bucket_ms = {
                                type = "integer",
                                description = "Bucket width in milliseconds (> 0). "
                                    .. "Requires `agg`. Enables time-bucketed aggregation.",
                            },
                            limit = {
                                type = "integer",
                                description = "Maximum number of rows to return (>= 0).",
                            },
                            offset = {
                                type = "integer",
                                description = "Number of rows to skip (>= 0).",
                            },
                        },
                    },
                },
                required = { "series" },
            },
            handler = function(input)
                local rows = std.ts.query(input.series, input.opts)
                return { rows = rows }
            end,
        },

        last = {
            description = "Return the most-recent data point in a time-series stream from "
                .. "the agent's local SQLite database. Returns nil if no matching row exists, "
                .. "or { ts, value, tags } for the latest row. `tags` applies the same "
                .. "AND-conjunction filter as std.ts.query.",
            input_schema = {
                type = "object",
                properties = {
                    series = {
                        type = "string",
                        description = "Stream name to query.",
                    },
                    tags = {
                        type = "object",
                        description = "Optional AND-filter for tag matching. "
                            .. "Keys: [a-zA-Z0-9_]+.",
                        additionalProperties = { type = "string" },
                    },
                },
                required = { "series" },
            },
            handler = function(input)
                local row = std.ts.last(input.series, input.tags)
                return { row = row }
            end,
        },
    }

    local registered = {}
    for _, op in ipairs(allowed) do
        local d = defs[op]
        if d then
            local name = prefix .. op
            tool.register(
                name,
                { description = d.description, input_schema = d.input_schema },
                d.handler
            )
            table.insert(registered, name)
        end
    end
    return registered
end
