--- sql_tools — the library behind `std.sql.register_tools`.
---
--- `src/bridge/sql.rs` registers the Rust half of the `std.sql` bridge and
--- then `require`s this module by name, installing every function it exports
--- onto `std.sql` (`bridge::load_tools_module`). This module writes nothing
--- onto `std` itself: it is a library, and the wiring is the bridge's.
---
--- The tier order every other module follows applies here too: a project's
--- `.agent-block/lib/sql_tools/` wins, this source is the fallback.
--- `agent-block vendor sql_tools` therefore changes the tool surface a model
--- is handed with no install in between, and a vendored copy delegates the
--- way any other module does — `local base = require("embedded.sql_tools")`,
--- wrap what it answers, `return M`.
---
--- Exports:
---   register_tools(opts?) -> array of registered tool names
---   shapes.register_tools_opts — the opts contract, as data
---
--- opts (all optional):
---   allowed : array of op names    (default: {"query","exec"})
---   prefix  : tool name prefix     (default: "sql_")
---
--- Each tool this registers is one `knl.shapes.tool_spec` — `name`,
--- `description`, `input_schema`, `handler` — but the kernel is deliberately
--- NOT required here: a tool module has to load on a VM that has none (the
--- bridge's fallback path runs this source directly on a bare `Lua::new()`).
--- The field set is named by doc, and checked where both halves exist:
--- `crates/agent-block/tests/e2e_embedded_blocks.rs`.
---
--- Written in Teal and embedded through `include_tl!` (`host.rs` and
--- `bridge/sql.rs`); the Lua the host holds is what Teal generates from this
--- file, without these comments. The globals it reads are declared through
--- `host_types`, and `lshape` through `lshape.d.tl` beside the vendored copy.
local host = require("host_types")
global std: host.Std
global tool: host.Tool

--- `lshape`, the tolerant way, for the one VM that has no `require` at all.
--- See `kv_tools/init.lua` for the case this covers: the bridge's fallback
--- runs this source on a bare `Lua::new()`, where nothing resolves, and that
--- VM only needs `std.sql` to hold its functions.
local shape_ok, lshape = pcall(require, "lshape")
local T = shape_ok and lshape.t or nil
local shape = shape_ok and lshape.check or nil

local record M
    --- What `register_tools` takes.
    record Opts
        allowed: {string}
        prefix: string
    end
    --- What both tools take: the statement, and its positional parameters.
    record Input
        sql: string
        params: {any}
    end
    --- One tool, before it is named. Built whole in `defs`, one per op:
    --- every field is set there, and the check holds each literal to it.
    record Def ---@struct
        description: string
        input_schema: {string:any}
        handler: function(Input): {string:any}
    end
    --- The opts contract, as data. Built whole, once.
    record Shapes ---@struct
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
        allowed = T.array_of(T.string):describe('the ops to register: "query", "exec"; default both'):is_optional(),
        prefix = T.string:describe('prepended to every tool name; default "sql_"'):is_optional(),
    }, { open = false })
end

M.shapes = { register_tools_opts = REGISTER_TOOLS_OPTS }

--- Register the LLM-facing SQL tools into the global tool registry.
---
--- Returns: array of registered tool names.
function M.register_tools(opts: M.Opts): {string}
    opts = opts or {}
    if shape ~= nil then
        shape.assert_dev(opts, REGISTER_TOOLS_OPTS, "std.sql.register_tools opts")
    end
    local allowed = opts.allowed or { "query", "exec" }
    local prefix = opts.prefix or "sql_"

    local base_schema: {string:any} = {
        type = "object",
        properties = {
            sql = { type = "string" },
            params = {
                type = "array",
                description = "Positional parameter values for ? placeholders.",
            },
        },
        required = { "sql" },
    }

    local defs: {string:M.Def} = {
        query = {
            description = "Run a SQL query against the agent's local SQLite database (embedded, file-backed UTF-8, persists across runs, agent-private). Use for SELECT-style reads. Returns { rows = [{ col = val, ... }, ...] }. Has a 5s default timeout (errors with 'sql.query timeout (<N>ms)' if exceeded). Type mapping: Lua booleans are stored as 0/1 integers (SQLite has no native bool); BLOB columns are unsupported; TEXT is UTF-8; NULL columns are returned as the std.sql.null sentinel (compare with `row.col == std.sql.null`).",
            input_schema = base_schema,
            handler = function(input: M.Input): {string:any}
                local rows = std.sql.query(input.sql, input.params)
                return { rows = rows }
            end,
        },
        exec = {
            description = "Run a SQL statement against the agent's local SQLite database (embedded, file-backed, persists across runs, agent-private). Use for INSERT / UPDATE / DELETE / CREATE TABLE / other DDL. Returns { affected = N, last_id = M }. Has a 5s default timeout (errors with 'sql.exec timeout (<N>ms)' if exceeded). Type mapping: Lua booleans are stored as 0/1 integers (SQLite has no native bool); BLOB columns are unsupported; TEXT is UTF-8.",
            input_schema = base_schema,
            handler = function(input: M.Input): {string:any}
                local r = std.sql.exec(input.sql, input.params)
                return { affected = r.affected, last_id = r.last_id }
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
