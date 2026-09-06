-- Exercises the std.fs editing primitives end to end inside a real Isle.
--
-- The point of the design is that the address is a line range and the text is
-- only a check, so these cases cover: a clean batch edit, every rejection that
-- leaves the file untouched, and rollback.

local dir = std.env.get("AGENT_BLOCK_HOME")
local path = dir .. "/target.txt"
std.fs.write(path, "alpha\nbravo\ncharlie\ndelta\n")

local function check(label, cond)
    print(string.format("[FS] %-32s = %s", label, tostring(cond)))
end

-- read_versioned ----------------------------------------------------------
local r = std.fs.read_versioned(path)
check("read.lines", r.lines == 4)
check("read.version_present", type(r.version) == "string" and #r.version > 0)

-- stale base is refused ---------------------------------------------------
local stale = std.fs.edit(path, {
    base = "0000000000000000",
    edits = { { start_line = 1, end_line = 1, expect = "alpha", replace = "ALPHA" } },
})
check("stale.rejected", stale.ok == false and stale.reason == "stale_base")
check("stale.file_untouched", std.fs.read(path) == "alpha\nbravo\ncharlie\ndelta\n")

-- expect mismatch returns what is actually there ---------------------------
local mismatch = std.fs.edit(path, {
    base = r.version,
    edits = { { start_line = 2, end_line = 2, expect = "WRONG", replace = "x" } },
})
check("mismatch.rejected", mismatch.ok == false and mismatch.reason == "expect_mismatch")
check("mismatch.reports_actual", mismatch.actual == "bravo")
check("mismatch.failures_lists_the_one", #mismatch.failures == 1 and mismatch.failures[1].edit_index == 1)
check("mismatch.file_untouched", std.fs.read(path) == "alpha\nbravo\ncharlie\ndelta\n")

-- numbers counted against the edited file, which is the mistake the tool
-- description now names. Edit 1 inserts a line; edits 2 and 3 address "bravo"
-- and "delta" one line lower than they are, as they would be once edit 1 had
-- landed. Every one of them is reported, so the constant offset is visible
-- without re-reading the file.
local shifted = std.fs.edit(path, {
    base = r.version,
    edits = {
        { start_line = 1, end_line = 1, expect = "alpha", replace = "ALPHA\nAAA" },
        { start_line = 3, end_line = 3, expect = "bravo", replace = "BRAVO" },
        { start_line = 5, end_line = 5, expect = "delta", replace = "DELTA" },
    },
})
check("shifted.rejected", shifted.ok == false and shifted.reason == "expect_mismatch")
check("shifted.first_failure_on_top", shifted.edit_index == 2 and shifted.actual == "charlie")
check("shifted.reports_both", #shifted.failures == 2)
check(
    "shifted.failure_reasons",
    shifted.failures[1].reason == "expect_mismatch" and shifted.failures[2].reason == "out_of_range"
)
check("shifted.failure_indices", shifted.failures[1].edit_index == 2 and shifted.failures[2].edit_index == 3)
check("shifted.out_of_range_carries_length", shifted.failures[2].file_lines == 4)
check("shifted.file_untouched", std.fs.read(path) == "alpha\nbravo\ncharlie\ndelta\n")

-- overlapping edits are refused as a set ----------------------------------
local overlap = std.fs.edit(path, {
    base = r.version,
    edits = {
        { start_line = 1, end_line = 2, expect = "alpha\nbravo", replace = "one" },
        { start_line = 2, end_line = 3, expect = "bravo\ncharlie", replace = "two" },
    },
})
check("overlap.rejected", overlap.ok == false and overlap.reason == "overlapping_edits")
check("overlap.file_untouched", std.fs.read(path) == "alpha\nbravo\ncharlie\ndelta\n")

-- out of range ------------------------------------------------------------
local oob = std.fs.edit(path, {
    edits = { { start_line = 9, end_line = 9, expect = "", replace = "x" } },
})
check("out_of_range.rejected", oob.ok == false and oob.reason == "out_of_range")

-- a batch applies bottom-up in one write ----------------------------------
local ok_res = std.fs.edit(path, {
    base = r.version,
    edits = {
        { start_line = 1, end_line = 1, expect = "alpha", replace = "ALPHA\nAAA" },
        { start_line = 4, end_line = 4, expect = "delta", replace = "" },
    },
})
check("batch.ok", ok_res.ok == true and ok_res.applied == 2)
check("batch.content", std.fs.read(path) == "ALPHA\nAAA\nbravo\ncharlie\n")
check("batch.version_changed", ok_res.version ~= r.version)

-- the version from the failed attempt is now stale -------------------------
local after_stale = std.fs.edit(path, {
    base = r.version,
    edits = { { start_line = 1, end_line = 1, expect = "ALPHA", replace = "x" } },
})
check("reuse_old_version.rejected", after_stale.ok == false and after_stale.reason == "stale_base")

-- rollback restores the pre-edit content ----------------------------------
local rb = std.fs.rollback(path)
check("rollback.ok", rb.ok == true)
check("rollback.content", std.fs.read(path) == "alpha\nbravo\ncharlie\ndelta\n")
check("rollback.second_call_empty", std.fs.rollback(path).ok == false)

-- register_tools ----------------------------------------------------------
local names = std.fs.register_tools({ allowed = { "read", "edit" }, path_lock = { path } })
check("tools.registered", #names == 2 and names[1] == "fs_read" and names[2] == "fs_edit")

local schema = tool.schema()
local found = {}
for _, t in ipairs(schema) do
    found[t.name] = true
end
check("tools.in_schema", found["fs_read"] == true and found["fs_edit"] == true)

-- path_lock keeps the model inside the declared files
local denied = tool.call("fs_read", { path = dir .. "/other.txt" })
check("path_lock.denies_other", denied.ok == false and denied.reason == "path_not_allowed")

local allowed = tool.call("fs_read", { path = path })
check("path_lock.allows_target", allowed.lines == 4)

print("[FS] done")
