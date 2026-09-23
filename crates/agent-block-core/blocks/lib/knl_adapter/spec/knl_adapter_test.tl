-- knl_adapter_test.tl — `htl test` for `knl_adapter`: the two Port
-- interfaces held to their methods, and the tool side bound without a
-- kernel. The specs beside it hold the shim, the classifiers and the
-- counting against scripted Ports.
local t = require("htl.test")
local adapter = require("knl_adapter")

t.describe("LLMPort.new", function()
    t.it("refuses an impl missing any of build / parse / classify, naming the one", function()
        t.expect(function()
            adapter.LLMPort.new({ build = function() end, parse = function() end })
        end):to_error("missing method: classify")
        t.expect(function()
            adapter.LLMPort.new("nope")
        end):to_error("impl must be a table")
    end)
end)

t.describe("ToolPort and tools", function()
    t.it("binds a flat spec into the entry a device takes, keyed by its name", function()
        local tools = adapter.tools({
            {
                name = "echo",
                description = "says it back",
                input_schema = { type = "object" },
                handler = function(args: any): any
                    return args
                end,
            },
        })
        local entry = tools.echo
        t.expect(entry):to_not_be_nil()
        t.expect(entry and entry.description):to_equal("says it back")
        local answered = entry and entry.handler({ x = 1 }) as {string:any}
        t.expect(answered and answered.x):to_equal(1)
    end)
    t.it("refuses a spec that spells the schema `schema`, and two tools of one name", function()
        -- Through an untyped handle: the checker would refuse the key
        -- first, and what is under test is the module's own refusal.
        local lua = adapter.ToolPort.lua as function(any): any
        t.expect(function()
            lua({ name = "a", handler = function() end, schema = {} })
        end):to_error("the field is `input_schema`, not `schema`")
        t.expect(function()
            adapter.tools({
                { name = "a", handler = function() end },
                { name = "a", handler = function() end },
            })
        end):to_error("duplicate tool name 'a'")
    end)
    t.it("hands back what declare() said through tool()", function()
        local port = adapter.ToolPort.new({
            declare = function(): any
                return { name = "p", description = "a port" }
            end,
            invoke = function(_: any, args: any): any
                return "ran " .. tostring(args)
            end,
        })
        local name, entry = adapter.tool(port)
        t.expect(name):to_equal("p")
        t.expect(entry.handler("x")):to_equal("ran x")
    end)
end)

t.describe("estimate", function()
    t.it("counts bytes at the safe ratio", function()
        t.expect(adapter._estimate_tokens({ system = "abcdefgh" })):to_equal(5)
    end)
end)
