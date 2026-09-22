-- ts_tools_spec.lua — mlua-lspec unit tests for the `ts_tools` library, the
-- module the `std.ts` bridge installs as `std.ts.register_tools`.
--
-- Run via:
--   just test-lua ts_tools
--
-- The pure runner has no host bridges, so `std.ts` and the `tool` global are
-- stubbed here, each recording what it was handed.
--
-- What this proves:
--   1 `require("ts_tools")` answers a table — the module's exports and its
--     shape, not `true`, which is what the delegation idiom rests on;
--   2 an option the module does not know is refused BY NAME when the dev
--     contract is on, rather than being a no-op;
--   3 what it registers is one tool per allowed op, prefixed, each a name /
--     description / input_schema / handler, and the answer is their names;
--   4 the handlers forward the series and its options to the bridge and hand
--     back the shape the tool promises.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ── the host, as much of it as these paths reach ─────────────────────────────

--- Every `std.ts` call, in order.
local calls = {}

_G.std = {
    ts = {
        append = function(series, value, tags, at)
            calls[#calls + 1] = { op = "append", series = series, value = value, tags = tags, at = at }
        end,
        query = function(series, opts)
            calls[#calls + 1] = { op = "query", series = series, opts = opts }
            return { { ts = 1, value = 2 } }
        end,
        last = function(series, tags)
            calls[#calls + 1] = { op = "last", series = series, tags = tags }
            return { ts = 9, value = 3 }
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

local ts_tools = require("ts_tools")
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

describe("ts_tools — the module", function()
    it("answers a table of functions and its shape", function()
        expect(type(ts_tools)).to.be("table")
        expect(type(ts_tools.register_tools)).to.be("function")
        expect(type(ts_tools.shapes)).to.be("table")
        expect(type(ts_tools.shapes.register_tools_opts)).to.be("table")
    end)

    it("installs nothing on `std.ts` itself — a library is not wiring", function()
        expect(std.ts.register_tools).to.be(nil)
    end)
end)

describe("ts_tools.register_tools — the opts contract", function()
    it("names an option it does not know", function()
        local message = dev_refusal(function()
            ts_tools.register_tools({ preifx = "t_" })
        end)
        expect(message:find("$.preifx", 1, true) ~= nil).to.be(true)
        expect(message:find("std.ts.register_tools opts", 1, true) ~= nil).to.be(true)
    end)

    it("names the field a wrong-shaped option sits at", function()
        local message = dev_refusal(function()
            ts_tools.register_tools({ allowed = { 1, 2 } })
        end)
        expect(message:find("$.allowed", 1, true) ~= nil).to.be(true)
    end)
end)

describe("ts_tools.register_tools — what it registers", function()
    it("is one tool per allowed op, prefixed, each a name / description / schema / handler", function()
        registered = {}
        local names = ts_tools.register_tools({ prefix = "probe_", allowed = { "append", "last" } })

        expect(names).to.equal({ "probe_append", "probe_last" })
        expect(#registered).to.be(2)
        for _, entry in ipairs(registered) do
            expect(type(entry.name)).to.be("string")
            expect(type(entry.meta.description)).to.be("string")
            expect(type(entry.meta.input_schema)).to.be("table")
            expect(type(entry.handler)).to.be("function")
        end
        expect(registered[1].meta.input_schema.required).to.equal({ "series", "value" })
    end)

    it("registers all three ops when the caller names none", function()
        registered = {}
        expect(ts_tools.register_tools()).to.equal({ "ts_append", "ts_query", "ts_last" })
    end)
end)

describe("ts_tools.register_tools — the handlers", function()
    it("forwards a point whole, timestamp and tags included", function()
        registered, calls = {}, {}
        ts_tools.register_tools({ allowed = { "append" } })
        local answer = registered[1].handler({
            series = "cpu_load",
            value = 0.5,
            tags = { host = "a" },
            at = 1234,
        })

        expect(answer.ok).to.be(true)
        expect(calls[1].series).to.be("cpu_load")
        expect(calls[1].value).to.be(0.5)
        expect(calls[1].tags).to.equal({ host = "a" })
        expect(calls[1].at).to.be(1234)
    end)

    it("forwards the query options untouched and answers `rows`", function()
        registered, calls = {}, {}
        ts_tools.register_tools({ allowed = { "query" } })
        local opts = { agg = "sum", bucket_ms = 60000 }
        local answer = registered[1].handler({ series = "cpu_load", opts = opts })

        expect(calls[1].opts).to.be(opts)
        expect(answer.rows).to.equal({ { ts = 1, value = 2 } })
    end)

    it("answers the latest point under `row`", function()
        registered, calls = {}, {}
        ts_tools.register_tools({ allowed = { "last" } })
        local answer = registered[1].handler({ series = "cpu_load", tags = { host = "a" } })

        expect(calls[1].tags).to.equal({ host = "a" })
        expect(answer.row).to.equal({ ts = 9, value = 3 })
    end)
end)
