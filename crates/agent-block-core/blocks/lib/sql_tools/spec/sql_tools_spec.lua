-- sql_tools_spec.lua — mlua-lspec unit tests for the `sql_tools` library, the
-- module the `std.sql` bridge installs as `std.sql.register_tools`.
--
-- Run via:
--   just test-lua sql_tools
--
-- The pure runner has no host bridges, so `std.sql` and the `tool` global are
-- stubbed here, each recording what it was handed.
--
-- What this proves:
--   1 `require("sql_tools")` answers a table — the module's exports and its
--     shape, not `true`, which is what the delegation idiom rests on;
--   2 an option the module does not know is refused BY NAME when the dev
--     contract is on, rather than being a no-op;
--   3 what it registers is one tool per allowed op, prefixed, each a name /
--     description / input_schema / handler, and the answer is their names;
--   4 the handlers forward the statement and its parameters to the bridge and
--     hand back the shape the tool promises (`rows` / `affected` + `last_id`).

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ── the host, as much of it as these paths reach ─────────────────────────────

--- Every `std.sql` call, in order, as { op, sql, params }.
local calls = {}

_G.std = {
    sql = {
        query = function(sql, params)
            calls[#calls + 1] = { op = "query", sql = sql, params = params }
            return { { n = 1 } }
        end,
        exec = function(sql, params)
            calls[#calls + 1] = { op = "exec", sql = sql, params = params }
            return { affected = 2, last_id = 7 }
        end,
    },
}

--- Everything handed to `tool.register`, in order.
local registered = {}
_G.tool = {
    register = function(name, meta, handler)
        registered[#registered + 1] = { name = name, meta = meta, handler = handler }
    end,
}

local sql_tools = require("sql_tools")
local check = require("lshape.check")

--- Run `fn` with the dev-mode gate pinned on, and hand back the message it
--- failed with — the contract below is dev-only.
local function dev_refusal(fn)
    local saved = check.is_dev_mode
    check.is_dev_mode = function()
        return true
    end
    local ok, err = pcall(fn)
    check.is_dev_mode = saved
    expect(ok).to.be(false)
    return tostring(err)
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("sql_tools — the module", function()
    it("answers a table of functions and its shape", function()
        expect(type(sql_tools)).to.be("table")
        expect(type(sql_tools.register_tools)).to.be("function")
        expect(type(sql_tools.shapes)).to.be("table")
        expect(type(sql_tools.shapes.register_tools_opts)).to.be("table")
    end)

    it("installs nothing on `std.sql` itself — a library is not wiring", function()
        expect(std.sql.register_tools).to.be(nil)
    end)
end)

describe("sql_tools.register_tools — the opts contract", function()
    it("names an option it does not know", function()
        local message = dev_refusal(function()
            sql_tools.register_tools({ prefx = "s_" })
        end)
        expect(message:find("$.prefx", 1, true) ~= nil).to.be(true)
        expect(message:find("std.sql.register_tools opts", 1, true) ~= nil).to.be(true)
    end)

    it("names the field a wrong-shaped option sits at", function()
        local message = dev_refusal(function()
            sql_tools.register_tools({ allowed = "query" })
        end)
        expect(message:find("$.allowed", 1, true) ~= nil).to.be(true)
    end)
end)

describe("sql_tools.register_tools — what it registers", function()
    it("is one tool per allowed op, prefixed, each a name / description / schema / handler", function()
        registered = {}
        local names = sql_tools.register_tools({ prefix = "probe_", allowed = { "query" } })

        expect(names).to.equal({ "probe_query" })
        expect(registered[1].name).to.be("probe_query")
        expect(type(registered[1].meta.description)).to.be("string")
        expect(registered[1].meta.input_schema.required).to.equal({ "sql" })
        expect(type(registered[1].handler)).to.be("function")
    end)

    it("registers both ops when the caller names none", function()
        registered = {}
        expect(sql_tools.register_tools()).to.equal({ "sql_query", "sql_exec" })
    end)
end)

describe("sql_tools.register_tools — the handlers", function()
    it("forwards the statement and its parameters, and answers `rows`", function()
        registered, calls = {}, {}
        sql_tools.register_tools({ allowed = { "query" } })
        local answer = registered[1].handler({ sql = "SELECT 1", params = { 1, 2 } })

        expect(calls[1].op).to.be("query")
        expect(calls[1].sql).to.be("SELECT 1")
        expect(calls[1].params).to.equal({ 1, 2 })
        expect(answer.rows).to.equal({ { n = 1 } })
    end)

    it("answers `affected` and `last_id` for a statement", function()
        registered, calls = {}, {}
        sql_tools.register_tools({ allowed = { "exec" } })
        local answer = registered[1].handler({ sql = "INSERT INTO t VALUES (?)", params = { "x" } })

        expect(calls[1].op).to.be("exec")
        expect(answer.affected).to.be(2)
        expect(answer.last_id).to.be(7)
    end)
end)
