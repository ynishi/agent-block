-- Embedded Lua source loaded by src/bridge/sql.rs.
-- Defines std.sql.register_tools(opts?) — LLM-facing tool registration helper.
--
-- opts (all optional):
--   allowed : array of op names    (default: {"query","exec"})
--   prefix  : tool name prefix     (default: "sql_")
--
-- Handlers apply a first-keyword allowlist:
--   query : SELECT / WITH
--   exec  : INSERT / UPDATE / DELETE
-- Anything else returns { error = "..." } without touching the DB.
-- NOTE: this is a thin guardrail, not a full permission gate. Production
-- will enforce via manifest-driven perms (see issue §Permission 分離).
--
-- Returns: array of registered tool names.

std.sql.register_tools = function(opts)
    opts = opts or {}
    local allowed = opts.allowed or { "query", "exec" }
    local prefix = opts.prefix or "sql_"

    local function first_keyword(sql)
        if type(sql) ~= "string" then return nil end
        return (sql:match("^%s*(%a+)") or ""):upper()
    end

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
            description = "Run a read-only SQL query (SELECT / WITH). Returns { rows = [{ col = val, ... }, ...] }.",
            input_schema = base_schema,
            handler = function(input)
                local kw = first_keyword(input.sql)
                if kw ~= "SELECT" and kw ~= "WITH" then
                    return { error = "query allows SELECT or WITH only (got " .. tostring(kw) .. ")" }
                end
                local rows = std.sql.query(input.sql, input.params)
                return { rows = rows }
            end,
        },
        exec = {
            description = "Run a write SQL statement (INSERT / UPDATE / DELETE). Returns { affected = N, last_id = M }.",
            input_schema = base_schema,
            handler = function(input)
                local kw = first_keyword(input.sql)
                local allowed_kw = { INSERT = true, UPDATE = true, DELETE = true }
                if not allowed_kw[kw] then
                    return { error = "exec allows INSERT / UPDATE / DELETE only (got " .. tostring(kw) .. ")" }
                end
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
