-- tool_spec.lua — mlua-lspec unit tests for the ToolPort / tool binding
-- (tool-port-design.md ST1: lua binding + Port contract as the mcp seam).
--
-- Run via:
--   test_launch(code_file=".../knl_adapter/spec/tool_spec.lua",
--               search_paths=[".../blocks/lib"])
--
-- What this proves:
--   1 ToolPort.new validates the 2-method contract (declare / invoke).
--   2 ToolPort.lua wraps a flat spec (the std.fs.tool_specs shape) as a
--     pass-through Port; the schema field is `input_schema` and a spec that
--     spells it `schema` is a construction error.
--   3 knl_adapter.tools binds a flat-spec array into knl's tools map;
--     duplicate names are a loud error; entries carry no source literal.
--   4 end-to-end over the kernel: a beat whose model answers with tool_use
--     runs the bound handler and closes the pair (ok=true / raise=>ok=false).
--
-- A fake `knl` bridge stands in for the Rust syscall layer (same fake as
-- knl/spec/device_spec.lua), installed BEFORE require("knl").

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ─────────────────────────────────────────────────────────────────────────────
-- Fake `knl` bridge
-- ─────────────────────────────────────────────────────────────────────────────

local minted = 0
local opened = 0

local function fake_session(opts)
    opts = opts or {}
    opened = opened + 1
    local id = string.format("sess-%06d", opened)
    local s = { _events = {}, _seq = 0, _owner = opts.owner or "anon" }
    -- Identity: the three readings the kernel answers. They are here because
    -- `knl.beat` asks a value for the whole session surface before it treats
    -- it as a session — a fake that answered less would not be one.
    function s:id()
        return id
    end
    function s:scope_id()
        return "scope-" .. id
    end
    function s:owner()
        return self._owner
    end
    -- An append records: the kernel stamps seq and stores every other field
    -- as written, `beat` (the shell's declared id) included.
    function s:append(ev)
        self._seq = self._seq + 1
        ev.seq = self._seq
        self._events[#self._events + 1] = ev
        return ev.seq
    end
    function s:events()
        return self._events
    end
    -- No grant here, so every reservation is allowed and nothing is
    -- recorded — the kernel's "no budget, no ledger" answer.
    function s:reserve(_n)
        return true
    end
    -- The write IS the result: spend answers nothing (the kernel's surface).
    function s:spend(_n) end
    function s:remaining()
        return nil
    end
    function s:exhausted()
        return false
    end
    function s:close() end
    return s
end

knl = {
    open = function(o)
        return fake_session(o)
    end,
    new_beat_id = function()
        minted = minted + 1
        return string.format("beat-%06d", minted)
    end,
}

local kernel = require("knl")
local adapter = require("knl_adapter")
local ToolPort = adapter.ToolPort
local Outcome = kernel.Outcome

-- A flat spec factory shaped like std.fs.tool_specs output.
local function flat_spec(name, fn)
    return {
        name = name,
        description = "the " .. name .. " tool",
        input_schema = { type = "object", properties = {} },
        handler = fn or function(args)
            return name .. ":" .. tostring(args.x)
        end,
    }
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("ToolPort.new — the 2-method contract", function()
    it("rejects an impl missing declare or invoke", function()
        expect(function()
            ToolPort.new({ declare = function() end })
        end).to.fail()
        expect(function()
            ToolPort.new({ invoke = function() end })
        end).to.fail()
    end)

    it("accepts a full impl", function()
        local p = ToolPort.new({
            declare = function()
                return { name = "t" }
            end,
            invoke = function()
                return "r"
            end,
        })
        expect(p:declare().name).to.be("t")
    end)
end)

describe("ToolPort.lua — flat spec pass-through", function()
    it("declares the spec triple verbatim and invokes the closure", function()
        local p = ToolPort.lua(flat_spec("echo"))
        local d = p:declare()
        expect(d.name).to.be("echo")
        expect(d.description).to.be("the echo tool")
        expect(p:invoke({ x = 1 })).to.be("echo:1")
    end)

    it("rejects a spec that spells the schema field `schema`", function()
        -- Not a second accepted spelling: reading one name and declaring
        -- the other is how a tool reaches a provider with no schema at all.
        expect(function()
            ToolPort.lua({
                name = "old",
                schema = { type = "object" },
                handler = function()
                    return "r"
                end,
            })
        end).to.fail()
    end)

    it("rejects a spec without name or handler", function()
        expect(function()
            ToolPort.lua({ handler = function() end })
        end).to.fail()
        expect(function()
            ToolPort.lua({ name = "nohandler" })
        end).to.fail()
    end)
end)

describe("knl_adapter.tools — binding into the knl map", function()
    it("binds a flat-spec array (the fs.tool_specs shape) as-is", function()
        local tools = adapter.tools({ flat_spec("fs_read"), flat_spec("fs_edit") })
        expect(tools.fs_read).to.exist()
        expect(tools.fs_edit).to.exist()
        expect(tools.fs_read.description).to.be("the fs_read tool")
        expect(type(tools.fs_read.handler)).to.be("function")
        expect(tools.fs_read.handler({ x = 7 })).to.be("fs_read:7")
    end)

    it("mixes ToolPort instances and flat specs in one list", function()
        local p = ToolPort.new({
            declare = function()
                return { name = "ported" }
            end,
            invoke = function(_, args)
                return "p:" .. tostring(args.x)
            end,
        })
        local tools = adapter.tools({ p, flat_spec("flat") })
        expect(tools.ported.handler({ x = 2 })).to.be("p:2")
        expect(tools.flat).to.exist()
    end)

    it("a duplicate name is a loud error", function()
        expect(function()
            adapter.tools({ flat_spec("dup"), flat_spec("dup") })
        end).to.fail()
    end)

    it("a declare() without a name is a loud error even in prod", function()
        local p = ToolPort.new({
            declare = function()
                return { description = "nameless" }
            end,
            invoke = function()
                return "r"
            end,
        })
        expect(function()
            adapter.tool(p)
        end).to.fail()
    end)
end)

describe("ToolPort.mcp — the second source (fake mcp bridge)", function()
    local function install_fake_mcp(tools, call_results)
        mcp = {
            list_tools = function(_server)
                return { ok = true, tools = tools }
            end,
            call = function(server, name, args)
                local r = call_results[name]
                if type(r) == "function" then
                    return r(server, name, args)
                end
                return r
            end,
        }
    end

    it("declares with the <server>__<tool> namespace and inputSchema conversion", function()
        install_fake_mcp({}, {})
        local p = ToolPort.mcp("srv", {
            name = "search",
            description = "find things",
            inputSchema = { type = "object", properties = { q = { type = "string" } } },
        })
        local d = p:declare()
        expect(d.name).to.be("srv__search")
        expect(d.description).to.be("find things")
        expect(d.input_schema.properties.q.type).to.be("string")
    end)

    it("invoke extracts a single text block verbatim", function()
        install_fake_mcp({}, {
            search = { ok = true, content = { { type = "text", text = "found it" } } },
        })
        local p = ToolPort.mcp("srv", { name = "search" })
        expect(p:invoke({ q = "x" })).to.be("found it")
    end)

    it("invoke raises on transport failure (ok=false) and on is_error", function()
        install_fake_mcp({}, {
            down = { ok = false, error = "connection refused" },
            bad = { ok = true, is_error = true, content = { { type = "text", text = "tool blew up" } } },
        })
        expect(function()
            ToolPort.mcp("srv", { name = "down" }):invoke({})
        end).to.fail()
        expect(function()
            ToolPort.mcp("srv", { name = "bad" }):invoke({})
        end).to.fail()
    end)

    it("mcp_tools lists a server into ports, honouring allow", function()
        install_fake_mcp({
            { name = "a", description = "A" },
            { name = "b", description = "B" },
            { name = "c", description = "C" },
        }, {
            b = { ok = true, content = { { type = "text", text = "B ran" } } },
        })
        local ports = adapter.mcp_tools("srv", { allow = { "b" } })
        expect(#ports).to.be(1)
        local tools = adapter.tools(ports)
        expect(tools.srv__b).to.exist()
        expect(tools.srv__b.handler({})).to.be("B ran")
    end)

    it("a raising mcp invoke closes the pair ok=false through a beat", function()
        install_fake_mcp({ { name = "bad" } }, {
            bad = { ok = true, is_error = true, content = { { type = "text", text = "boom" } } },
        })
        local session = kernel.open({})
        local device = kernel.device({
            llm = (function()
                local sent = false
                return function(_req)
                    if sent then
                        return {
                            status = "ok",
                            content = { { type = "text", text = "done" } },
                            usage = {},
                            stop_reason = "end_turn",
                        }
                    end
                    sent = true
                    return {
                        status = "ok",
                        content = { { type = "tool_use", id = "c1", name = "srv__bad", input = {} } },
                        usage = {},
                        stop_reason = "tool_use",
                    }
                end
            end)(),
            tools = adapter.tools(adapter.mcp_tools("srv")),
        })
        session:append({ kind = "msg_user", content = "q" })
        local o = kernel.beat(session, device)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(o.out.tools[1].ok).to.be(false)
    end)
end)

describe("bound tools drive a beat (kernel contract end-to-end)", function()
    local function llm_with_tool(name)
        local sent = false
        return function(_req)
            if sent then
                return {
                    status = "ok",
                    content = { { type = "text", text = "done" } },
                    usage = {},
                    stop_reason = "end_turn",
                }
            end
            sent = true
            return {
                status = "ok",
                content = {
                    { type = "tool_use", id = "c1", name = name, input = { x = 9 } },
                },
                usage = {},
                stop_reason = "tool_use",
            }
        end
    end

    it("a tool_use runs the bound handler and closes the pair ok=true", function()
        local session = kernel.open({})
        local device = kernel.device({
            llm = llm_with_tool("fs_read"),
            tools = adapter.tools({ flat_spec("fs_read") }),
        })
        session:append({ kind = "msg_user", content = "q" })
        local o = kernel.beat(session, device)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(o.out.tools[1].name).to.be("fs_read")
        expect(o.out.tools[1].ok).to.be(true)
        local last = session:events()[#session:events()]
        expect(last.kind).to.be("tool_result")
        expect(last.result).to.be("fs_read:9")
        -- both halves of the pair carry the beat's declared id
        expect(last.beat).to.be(o.out.beat)
        expect(type(o.out.beat)).to.be("string")
    end)

    it("a raising invoke closes the pair ok=false (source failure vocabulary stays inside)", function()
        local boom = flat_spec("boom", function()
            error("device exploded")
        end)
        local session = kernel.open({})
        local device = kernel.device({
            llm = llm_with_tool("boom"),
            tools = adapter.tools({ boom }),
        })
        session:append({ kind = "msg_user", content = "q" })
        local o = kernel.beat(session, device)
        expect(Outcome.is_ok(o)).to.be(true)
        expect(o.out.tools[1].ok).to.be(false)
        local last = session:events()[#session:events()]
        expect(last.kind).to.be("tool_result")
        expect(last.ok).to.be(false)
    end)
end)
