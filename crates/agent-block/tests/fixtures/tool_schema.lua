-- Register tools and print schema as JSON.
tool.register("greet", {
    description = "Greet a user",
    input_schema = {
        type = "object",
        properties = {
            name = { type = "string" },
        },
    },
}, function(input)
    return "Hello, " .. input.name
end)

local names = tool.list()
for _, name in ipairs(names) do
    print("tool: " .. name)
end
