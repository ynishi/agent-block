-- kv_tools_spec.lua — mlua-lspec unit tests for the `kv_tools` library, the
-- module the `std.kv` bridge installs as `std.kv.register_tools`.
--
-- Run via:
--   just test-lua kv_tools
--
-- The pure runner has no host bridges, so `std.kv` and the `tool` global are
-- stubbed here, each recording what it was handed.
--
-- What this proves:
--   1 `require("kv_tools")` answers a table — the module's exports and its
--     shape, not `true`, which is what the delegation idiom rests on;
--   2 an option the module does not know is refused BY NAME when the dev
--     contract is on, rather than being a no-op that reads like a caller who
--     locked the namespace and did not;
--   3 what it registers is one tool per allowed op, prefixed, each a name /
--     description / input_schema / handler, and the answer is their names;
--   4 `ns_lock` is the handler's: the namespace the model is not asked for is
--     the one the bridge is called with.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ── the host, as much of it as these paths reach ─────────────────────────────

--- Every `std.kv` call, in order, as { op, ns, key }.
local calls = {}

_G.std = {
    kv = {
        get = function(ns, key)
            calls[#calls + 1] = { op = "get", ns = ns, key = key }
            return "stored"
        end,
        set = function(ns, key, value)
            calls[#calls + 1] = { op = "set", ns = ns, key = key, value = value }
        end,
        delete = function(ns, key)
            calls[#calls + 1] = { op = "delete", ns = ns, key = key }
            return true
        end,
        list = function(ns, prefix)
            calls[#calls + 1] = { op = "list", ns = ns, prefix = prefix }
            return { "a", "b" }
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

local kv_tools = require("kv_tools")
local check = require("lshape.check")

--- Run `fn` with the dev-mode gate pinned on, and hand back the message it
--- failed with. The contract below is dev-only, so the case pins the mode
--- rather than trusting the environment it happens to run in.
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

describe("kv_tools — the module", function()
    it("answers a table of functions and its shape", function()
        expect(type(kv_tools)).to.be("table")
        expect(type(kv_tools.register_tools)).to.be("function")
        expect(type(kv_tools.shapes)).to.be("table")
        expect(type(kv_tools.shapes.register_tools_opts)).to.be("table")
    end)

    it("installs nothing on `std.kv` itself — a library is not wiring", function()
        expect(std.kv.register_tools).to.be(nil)
    end)
end)

describe("kv_tools.register_tools — the opts contract", function()
    it("names an option it does not know", function()
        local message = dev_refusal(function()
            kv_tools.register_tools({ ns_lok = "locked" })
        end)
        expect(message:find("$.ns_lok", 1, true) ~= nil).to.be(true)
        expect(message:find("std.kv.register_tools opts", 1, true) ~= nil).to.be(true)
    end)

    it("names the field a wrong-shaped option sits at", function()
        local message = dev_refusal(function()
            kv_tools.register_tools({ prefix = 17 })
        end)
        expect(message:find("$.prefix", 1, true) ~= nil).to.be(true)
    end)
end)

describe("kv_tools.register_tools — what it registers", function()
    it("is one tool per allowed op, prefixed, each a name / description / schema / handler", function()
        registered = {}
        local names = kv_tools.register_tools({ prefix = "probe_", allowed = { "get", "list" } })

        expect(names).to.equal({ "probe_get", "probe_list" })
        expect(#registered).to.be(2)
        for _, entry in ipairs(registered) do
            expect(type(entry.name)).to.be("string")
            expect(type(entry.meta.description)).to.be("string")
            expect(type(entry.meta.input_schema)).to.be("table")
            expect(type(entry.handler)).to.be("function")
        end
    end)

    it("registers all four ops when the caller names none", function()
        registered = {}
        local names = kv_tools.register_tools()
        expect(names).to.equal({ "kv_get", "kv_set", "kv_delete", "kv_list" })
    end)
end)

describe("kv_tools.register_tools — ns_lock", function()
    it("asks the model for `ns` when there is no lock, and calls with what it said", function()
        registered, calls = {}, {}
        kv_tools.register_tools({ allowed = { "get" } })
        expect(registered[1].meta.input_schema.required).to.equal({ "ns", "key" })

        local answer = registered[1].handler({ ns = "chosen", key = "k" })
        expect(answer.value).to.be("stored")
        expect(calls[1].ns).to.be("chosen")
    end)

    it("does not ask for `ns` when locked, and calls with the lock whatever the model said", function()
        registered, calls = {}, {}
        kv_tools.register_tools({ allowed = { "get" }, ns_lock = "locked-ns" })
        expect(registered[1].meta.input_schema.required).to.equal({ "key" })
        expect(registered[1].meta.input_schema.properties.ns).to.be(nil)

        registered[1].handler({ ns = "somewhere-else", key = "k" })
        expect(calls[1].ns).to.be("locked-ns")
    end)
end)
