-- Register a tool, call it, print the result.
tool.register("echo", {
    description = "Echo input back",
    input_schema = {
        type = "object",
        properties = {
            message = { type = "string" },
        },
    },
}, function(input)
    return "echoed: " .. input.message
end)

local result = tool.call("echo", { message = "ping" })
print(result)
