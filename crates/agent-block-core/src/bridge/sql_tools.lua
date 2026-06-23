-- Embedded Lua source loaded by src/bridge/sql.rs.
-- Defines std.sql.register_tools(opts?) — LLM-facing tool registration helper.
--
-- opts (all optional):
--   allowed : array of op names    (default: {"query","exec"})
--   prefix  : tool name prefix     (default: "sql_")
--
-- Returns: array of registered tool names.

std.sql.register_tools = function(opts)
    opts = opts or {}
    local allowed = opts.allowed or { "query", "exec" }
    local prefix = opts.prefix or "sql_"

    local base_schema = {
        type = "object",
        properties = {
            sql    = { type = "string" },
            params = {
                type = "array",
                description = "Positional parameter values for ? placeholders.",
            },
        },
        required = { "sql" },
    }

    local defs = {
        query = {
            description = "Run a SQL query against the agent's local SQLite database (embedded, file-backed UTF-8, persists across runs, agent-private). Use for SELECT-style reads. Returns { rows = [{ col = val, ... }, ...] }. Has a 5s default timeout (errors with 'sql.query timeout (<N>ms)' if exceeded). Type mapping: Lua booleans are stored as 0/1 integers (SQLite has no native bool); BLOB columns are unsupported; TEXT is UTF-8; NULL columns are returned as the std.sql.null sentinel (compare with `row.col == std.sql.null`).",
            input_schema = base_schema,
            handler = function(input)
                local rows = std.sql.query(input.sql, input.params)
                return { rows = rows }
            end,
        },
        exec = {
            description = "Run a SQL statement against the agent's local SQLite database (embedded, file-backed, persists across runs, agent-private). Use for INSERT / UPDATE / DELETE / CREATE TABLE / other DDL. Returns { affected = N, last_id = M }. Has a 5s default timeout (errors with 'sql.exec timeout (<N>ms)' if exceeded). Type mapping: Lua booleans are stored as 0/1 integers (SQLite has no native bool); BLOB columns are unsupported; TEXT is UTF-8.",
            input_schema = base_schema,
            handler = function(input)
                local r = std.sql.exec(input.sql, input.params)
                return { affected = r.affected, last_id = r.last_id }
            end,
        },
    }

    local registered = {}
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
