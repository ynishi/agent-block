--- ts_tools — the library behind `std.ts.register_tools`.
---
--- `src/bridge/ts.rs` registers the Rust half of the `std.ts` bridge and then
--- `require`s this module by name, installing every function it exports onto
--- `std.ts` (`bridge::load_tools_module`). This module writes nothing onto
--- `std` itself: it is a library, and the wiring is the bridge's.
---
--- The tier order every other module follows applies here too: a project's
--- `.agent-block/lib/ts_tools/` wins, this source is the fallback.
--- `agent-block vendor ts_tools` therefore changes the tool surface a model
--- is handed with no install in between, and a vendored copy delegates the
--- way any other module does — `local base = require("embedded.ts_tools")`,
--- wrap what it answers, `return M`.
---
--- Exports:
---   register_tools(opts?) -> array of registered tool names
---   shapes.register_tools_opts — the opts contract, as data
---
--- opts (all optional):
---   allowed : array of op names   (default: {"append", "query", "last"})
---   prefix  : tool name prefix    (default: "ts_")
---
--- Each tool this registers is one `knl.shapes.tool_spec` — `name`,
--- `description`, `input_schema`, `handler` — but the kernel is deliberately
--- NOT required here: a tool module has to load on a VM that has none (the
--- bridge's fallback path runs this source directly on a bare `Lua::new()`).
--- The field set is named by doc, and checked where both halves exist:
--- `crates/agent-block/tests/e2e_embedded_blocks.rs`.
---
--- Written in Teal and embedded through `include_tl!` (`host.rs` and
--- `bridge/ts.rs`); the Lua the host holds is what Teal generates from this
--- file, without these comments. The globals it reads are declared through
--- `host_types`, and `lshape` through `lshape.d.tl` beside the vendored copy.
---
--- Notes on sum/avg:
---   The `value` column is stored as a JSON-encoded payload.  SQLite's
---   CAST(value AS REAL) treats JSON objects as 0.0, so `sum`/`avg` produce
---   meaningful results only when the series contains numeric values.  Use
---   number-only series for aggregate operations.
---
--- Tag key restriction:
---   Tag keys must match [a-zA-Z0-9_]+ (ASCII alphanumeric and underscore).
---   This restriction guards against SQL injection via json_extract paths.

--- `lshape`, the tolerant way, for the one VM that has no `require` at all.
--- See `kv_tools/init.lua` for the case this covers: the bridge's fallback
--- runs this source on a bare `Lua::new()`, where nothing resolves, and that
--- VM only needs `std.ts` to hold its functions.
local host = require("host_types")
global std: host.Std
global tool: host.Tool

local shape_ok, lshape = pcall(require, "lshape")
local T = shape_ok and lshape.t or nil
local shape = shape_ok and lshape.check or nil

local record M
    --- What `register_tools` takes.
    record Opts
        allowed: {string}
        prefix: string
    end
    --- What the tools take, all three in one shape: `series` always,
    --- `value` / `tags` / `at` for append, `opts` for query, `tags` for last.
    record Input
        series: string
        value: any
        tags: {string:string}
        at: integer
        opts: {string:any}
    end
    --- One tool, before it is named.
    record Def
        description: string
        input_schema: {string:any}
        handler: function(Input): {string:any}
    end
    --- The opts contract, as data.
    record Shapes
        register_tools_opts: lshape.Schema
    end
    shapes: Shapes
end

--- What `register_tools` is configured with. CLOSED on purpose: an option
--- this module does not know is a typo, and a typo that quietly became a
--- no-op is a tool surface nobody chose.
local REGISTER_TOOLS_OPTS: lshape.Schema
if shape_ok then
    REGISTER_TOOLS_OPTS = T.shape({
        allowed = T.array_of(T.string)
        :describe('the ops to register: "append", "query", "last"; default all three')
        :is_optional(),
        prefix = T.string:describe('prepended to every tool name; default "ts_"'):is_optional(),
    }, { open = false })
end

M.shapes = { register_tools_opts = REGISTER_TOOLS_OPTS }

--- Register the LLM-facing time-series tools into the global tool registry.
---
--- Returns: array of registered tool names.
function M.register_tools(opts: M.Opts): {string}
    opts = opts or {}
    if shape ~= nil then
        shape.assert_dev(opts, REGISTER_TOOLS_OPTS, "std.ts.register_tools opts")
    end
    local allowed = opts.allowed or { "append", "query", "last" }
    local prefix = opts.prefix or "ts_"

    local defs: {string:M.Def} = {
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
                        description = 'Logical stream name (e.g. "cpu_load", "agent_events").',
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
            handler = function(input: M.Input): {string:any}
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
                .. 'Supported agg values: "count", "sum", "avg", "last". '
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
                                description = "End of time range (Unix ms, inclusive). " .. "Default: end of time.",
                            },
                            tags = {
                                type = "object",
                                description = "AND-filter: all key-value pairs must match "
                                    .. "(json_extract per key). Keys: [a-zA-Z0-9_]+.",
                                additionalProperties = { type = "string" },
                            },
                            agg = {
                                type = "string",
                                description = "Aggregation function: " .. '"count" | "sum" | "avg" | "last".',
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
            handler = function(input: M.Input): {string:any}
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
                        description = "Optional AND-filter for tag matching. " .. "Keys: [a-zA-Z0-9_]+.",
                        additionalProperties = { type = "string" },
                    },
                },
                required = { "series" },
            },
            handler = function(input: M.Input): {string:any}
                local row = std.ts.last(input.series, input.tags)
                return { row = row }
            end,
        },
    }

    local registered: {string} = {}
    for _, op in ipairs(allowed) do
        local d = defs[op]
        if d then
            local name = prefix .. op
            tool.register(name, { description = d.description, input_schema = d.input_schema }, d.handler)
            table.insert(registered, name)
        end
    end
    return registered
end

return M
