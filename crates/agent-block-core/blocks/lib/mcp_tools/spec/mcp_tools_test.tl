-- mcp_tools_test.tl — `htl test` for the `mcp_tools` module: the two
-- translations, against a fake `std.json` declared through `host_types`.
--
-- Run via `just test-tl` (walks `blocks/lib` for `*_test.tl`; one state per
-- file). The declaration is the module's own: `global std: host.Std` with a
-- value, which is what a test may do and a module must not.
local t = require("htl.test")
local host = require("host_types")

global std: host.Std = {
    json = {
        encode = function(v: any): string
            local blocks = v as { { string: any } }
            return "<json:" .. tostring(#blocks) .. ">"
        end,
    },
}

local mcp_tools = require("mcp_tools")

t.describe("tool_decl", function()
    t.it("prefixes the tool with the server and keeps the schema under input_schema", function()
        local decl = mcp_tools.tool_decl("srv", { name = "read", inputSchema = { type = "object" } })
        t.expect(decl.name):to_equal("srv__read")
        t.expect(decl.description):to_equal("")
        t.expect(decl.input_schema.type):to_equal("object")
    end)
    t.it("accepts the snake_case key and invents an empty object schema for none", function()
        local snake = mcp_tools.tool_decl("srv", { name = "a", input_schema = { type = "string" } })
        t.expect(snake.input_schema.type):to_equal("string")
        local none = mcp_tools.tool_decl("srv", { name = "b" })
        t.expect(none.input_schema.type):to_equal("object")
    end)
end)

t.describe("result_text", function()
    t.it("hands one text block back verbatim", function()
        t.expect(mcp_tools.result_text({ { type = "text", text = "hi" } })):to_equal("hi")
    end)
    t.it("answers the empty string for no blocks, nil included", function()
        t.expect(mcp_tools.result_text({})):to_equal("")
        t.expect(mcp_tools.result_text(nil)):to_equal("")
    end)
    t.it("encodes anything else as JSON so nothing is dropped", function()
        t.expect(mcp_tools.result_text({ { type = "image" } })):to_equal("<json:1>")
        t.expect(mcp_tools.result_text({ { type = "text", text = "a" }, { type = "text", text = "b" } })):to_equal(
            "<json:2>"
        )
    end)
end)
