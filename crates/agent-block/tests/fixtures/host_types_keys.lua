-- Prints, for every host table `host_types.d.tl` declares, the names of the
-- functions the running VM actually holds there — one `KEYS <table>=a,b,c`
-- line each. tests/e2e_host_types_drift.rs reads the declaration and holds
-- it to these lines: a function declared that the host does not register is
-- a declaration that has drifted, and a `.tl` typed against it would call
-- into nothing.
local function keys_of(label, t)
    local names = {}
    if type(t) == "table" then
        for k, v in pairs(t) do
            if type(v) == "function" then
                names[#names + 1] = k
            end
        end
    end
    table.sort(names)
    print("KEYS " .. label .. "=" .. table.concat(names, ","))
end
keys_of("std.json", std.json)
keys_of("std.kv", std.kv)
keys_of("std.sql", std.sql)
keys_of("std.ts", std.ts)
keys_of("std.fs", std.fs)
keys_of("std.time", std.time)
keys_of("std.env", std.env)
keys_of("std.task", std.task)
keys_of("tool", tool)
keys_of("log", log)
keys_of("sh", sh)
keys_of("http", http)
keys_of("mcp", mcp)
