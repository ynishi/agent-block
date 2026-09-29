-- fake_bridge_spec.lua — mlua-lspec checks on the shared fake `knl` bridge
-- itself (knl/spec/fake_bridge.lua).
--
-- Run via:
--   test_launch(code_file=".../knl/spec/fake_bridge_spec.lua",
--               search_paths=[".../blocks/lib"])
--
-- Why a stand-in gets a spec: every other spec that opens a session passes or
-- fails against this fake, so what it claims to mirror is worth holding to a
-- check rather than to its header. What this proves:
--   1 a session carries every method the module's own registry declares
--     (`knl.shapes.session`), so the fake cannot fall behind the surface
--     `knl.beat` asks for;
--   2 append refuses what the kernel refuses and otherwise stores a copy with
--     `seq` stamped and every other field as written — a numeric `meta.beat`
--     included, which is the Lua layer's rule to refuse and not the kernel's;
--   3 with no options it adds nothing: no kernel rows, no clock;
--   4 reserve is the one decision point, and a refusal leaves the balance;
--   5 the kernel's own writes appear only when asked for, close records once,
--     and an allocation is one move or a refusal that opens no child;
--   6 `error` reads an attributed raise back, and only a published class.
--
-- What the real bridge does is asked where there is one
-- (crates/agent-block/tests/fixtures/knl_beat_test.lua).

local describe, it, expect = lust.describe, lust.it, lust.expect

local fake_bridge = require("knl.spec.fake_bridge")

-- Installed once so the module can load; every case below builds its own
-- fresh fake with `new`, so no case sees another's sessions.
fake_bridge.install()
local K = require("knl")

local function raised(fn)
    local ok, err = pcall(fn)
    expect(ok).to.be(false)
    return fake_bridge.read_error(err)
end

local function kinds(session)
    local names = {}
    for _, event in ipairs(session:events()) do
        names[#names + 1] = event.kind
    end
    return table.concat(names, ",")
end

describe("fake_bridge — the surface", function()
    it("answers every session method the module's registry declares", function()
        local s = fake_bridge.new().bridge.open({})
        local missing = {}
        for name in pairs(K.shapes.session) do
            if name ~= "__close" and type(s[name]) ~= "function" then
                missing[#missing + 1] = name
            end
        end
        table.sort(missing)
        expect(table.concat(missing, ",")).to.be("")
    end)

    it("carries the module's syscalls and no declaration of its own", function()
        local bridge = fake_bridge.new().bridge
        for _, name in ipairs({ "open", "resume", "new_beat_id", "error" }) do
            expect(type(bridge[name])).to.be("function")
        end
        expect(bridge.api).to_not.exist()
    end)

    it("mints a fresh beat id per call", function()
        local bridge = fake_bridge.new().bridge
        local a, b = bridge.new_beat_id(), bridge.new_beat_id()
        expect(type(a)).to.be("string")
        expect(a ~= b).to.be(true)
    end)
end)

describe("fake_bridge — append", function()
    it("stores a copy: seq stamped, the rest as written, numeric meta.beat included", function()
        local s = fake_bridge.new().bridge.open({})
        local event = { kind = "note", meta = { beat = 7 }, data = { x = 1 }, epoch_ms = 42 }
        expect(s:append(event)).to.be(1)
        expect(event.seq).to_not.exist()
        local rows, truncated = s:events()
        expect(truncated).to.be(false)
        expect(rows[1].seq).to.be(1)
        expect(rows[1].meta.beat).to.be(7)
        expect(rows[1].epoch_ms).to.be(42)
        rows[1].data.x = 2
        expect(s:events()[1].data.x).to.be(1)
    end)

    it("reads from `from` on", function()
        local s = fake_bridge.new().bridge.open({})
        s:append({ kind = "a" })
        s:append({ kind = "b" })
        local rows = s:events(2)
        expect(#rows).to.be(1)
        expect(rows[1].kind).to.be("b")
    end)

    it("refuses what the kernel refuses, as validation", function()
        local s = fake_bridge.new().bridge.open({})
        for _, event in ipairs({
            {},
            { kind = "note", beat = "b-1" },
            { kind = "note", meta = "b-1" },
            { kind = "note", meta = { nested = {} } },
            { kind = "session_closed", data = { reason = "x" } },
            { kind = "budget_spent", data = { amount = 1 } },
        }) do
            local e = raised(function()
                s:append(event)
            end)
            expect(e.method).to.be("append")
            expect(e.kind).to.be("validation")
        end
        expect(s:len()).to.be(0)
    end)

    it("refuses a closed session as closed", function()
        local s = fake_bridge.new().bridge.open({})
        s:close()
        local e = raised(function()
            s:append({ kind = "note" })
        end)
        expect(e.kind).to.be("closed")
    end)
end)

describe("fake_bridge — options", function()
    it("adds nothing unasked: no kernel rows, no clock", function()
        local s = fake_bridge.new().bridge.open({ budget = { amount = 5, tag = "t" } })
        s:reserve(1)
        s:spend(1)
        s:append({ kind = "note" })
        s:close("done")
        expect(kinds(s)).to.be("note")
        expect(s._events[1].epoch_ms).to_not.exist()
        expect(s.close_reason).to.be("done")
    end)

    it("writes the lifecycle and the ledger when asked, and stamps the clock", function()
        local ms = 0
        local s = fake_bridge
            .new({
                writes = { lifecycle = true, ledger = true },
                clock = function()
                    ms = ms + 1
                    return ms
                end,
            }).bridge
            .open({ budget = { amount = 2, tag = "t" } })
        s:reserve(1)
        s:reserve(5)
        s:spend(1)
        s:close("done")
        s:close("again")
        expect(kinds(s)).to.be(
            "session_opened,budget_granted,budget_reserved,budget_refused,budget_spent,session_closed"
        )
        local rows = s:events()
        expect(rows[#rows].data.reason).to.be("done")
        expect(rows[#rows].epoch_ms).to.be(6)
    end)

    it("answers a query from the option, and records it either way", function()
        local s = fake_bridge
            .new({
                query = function(_session, sql)
                    return { { sql = sql } }, true
                end,
            }).bridge
            .open({})
        local rows, truncated = s:query("SELECT 1", nil, { limit = 1 })
        expect(rows[1].sql).to.be("SELECT 1")
        expect(truncated).to.be(true)
        expect(s._queries[1].opts.limit).to.be(1)

        local plain = fake_bridge.new().bridge.open({})
        plain._query_rows = { { n = 1 } }
        expect(plain:query("SELECT n")[1].n).to.be(1)
    end)
end)

describe("fake_bridge — the budget", function()
    it("deducts on reserve, or refuses with the tag and leaves the balance", function()
        local s = fake_bridge.new().bridge.open({ budget = { amount = 3, tag = "beats" } })
        expect(s:reserve(2)).to.be(true)
        local ok, tag = s:reserve(2)
        expect(ok).to.be(false)
        expect(tag).to.be("beats")
        expect(s:remaining()).to.be(1)
        s:spend(5)
        expect(s:remaining()).to.be(0)
        expect(s:exhausted()).to.be(true)
    end)

    it("allows every reservation without a budget", function()
        local s = fake_bridge.new().bridge.open({})
        expect(s:reserve(1000)).to.be(true)
        expect(s:remaining()).to_not.exist()
        expect(s:exhausted()).to.be(false)
    end)

    it("moves an allocation in one move, and a close names the children still open", function()
        local fake = fake_bridge.new({ writes = { lifecycle = true, ledger = true } })
        local parent = fake.bridge.open({ owner = "p", budget = { amount = 10, tag = "beats" } })
        local child = fake.bridge.open({ parent = parent, budget = { from_parent = 4 } })
        expect(parent:remaining()).to.be(6)
        expect(child:remaining()).to.be(4)
        expect(child:owner()).to.be("p")
        expect(fake.sessions[child:id()]).to.be(child)
        expect(kinds(child)).to.be("session_opened,budget_granted")

        parent:close("done")
        local rows = parent:events()
        expect(rows[#rows].data.open_children[1]).to.be(child:id())
    end)

    it("refuses a short allocation and opens no child", function()
        local fake = fake_bridge.new({ writes = { ledger = true } })
        local parent = fake.bridge.open({ budget = { amount = 1, tag = "beats" } })
        local e = raised(function()
            fake.bridge.open({ parent = parent, budget = { from_parent = 2 } })
        end)
        expect(e.method).to.be("open")
        expect(e.kind).to.be("refused")
        expect(parent:remaining()).to.be(1)
        expect(kinds(parent)).to.be("budget_granted,budget_refused")
        expect(#parent._children).to.be(0)
    end)
end)

describe("fake_bridge — error", function()
    it("reads a published class back, and busy alone as retryable", function()
        local e = fake_bridge.read_error("knl: query: busy: database is locked: try again")
        expect(e.method).to.be("query")
        expect(e.kind).to.be("busy")
        expect(e.message).to.be("database is locked: try again")
        expect(e.retryable).to.be(true)
        expect(fake_bridge.read_error("knl: open: refused: short").retryable).to.be(false)
    end)

    it("leaves anything else whole and unclassified", function()
        local e = fake_bridge.read_error("knl: append: nonsense: x")
        expect(e.kind).to_not.exist()
        expect(e.message).to.be("knl: append: nonsense: x")
        expect(tostring(e)).to.be("knl: append: nonsense: x")
    end)
end)
