local ns = "_e2e_kv_" .. tostring(os.time())

-- 1. set / get roundtrip
std.kv.set(ns, "count", 42)
std.kv.set(ns, "name", "agent-block")
print("get_count=" .. tostring(std.kv.get(ns, "count")))
print("get_name=" .. tostring(std.kv.get(ns, "name")))
print("get_missing=" .. tostring(std.kv.get(ns, "nope")))

-- 2. list
local keys = std.kv.list(ns)
table.sort(keys)
print("list=" .. table.concat(keys, ","))

-- 3. prefix list
std.kv.set(ns, "run-1", "a")
std.kv.set(ns, "run-2", "b")
local runs = std.kv.list(ns, "run-")
table.sort(runs)
print("prefix_list=" .. table.concat(runs, ","))

-- 4. delete
print("delete_existing=" .. tostring(std.kv.delete(ns, "count")))
print("delete_missing=" .. tostring(std.kv.delete(ns, "nope")))
print("after_delete=" .. tostring(std.kv.get(ns, "count")))
