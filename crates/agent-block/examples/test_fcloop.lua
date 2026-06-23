-- test_fcloop.lua — FCLoop with a simple tool
local fcloop = require("fcloop")

-- Register a tool
tool.register("list_files", {
    description = "List files in a directory",
    input_schema = {
        type = "object",
        properties = {
            path = { type = "string", description = "Directory path" },
        },
        required = { "path" },
    },
}, function(input)
    local files = std.fs.glob(input.path .. "/*")
    return table.concat(files, "\n")
end)

tool.register("read_file", {
    description = "Read a file's content",
    input_schema = {
        type = "object",
        properties = {
            path = { type = "string", description = "File path" },
        },
        required = { "path" },
    },
}, function(input)
    return std.fs.read(input.path)
end)

-- Run FCLoop
local messages = {
    {
        role = "user",
        content = "List the lua scripts in the scripts/ directory, then read hello.lua and summarize what it does in one sentence.",
    },
}

fcloop.run(messages, {
    system = "You are a helpful assistant. Use the available tools to answer questions. Be concise.",
    max_tokens = 1024,
})

-- Print final assistant message
local last = messages[#messages]
if last.role == "assistant" then
    for _, block in ipairs(last.content) do
        if block.type == "text" then
            log.info(block.text)
        end
    end
end
