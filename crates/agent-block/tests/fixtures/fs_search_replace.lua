-- Exercises the `search_replace` tool spec end to end inside a real Isle: a
-- snippet named by its text is carried out as the line-addressed edit, with
-- that edit's checks intact.
--
-- The cases are the boundaries of the translation — a snippet inside a line,
-- one spanning lines and ending with a newline, the first line, the last line
-- of a file without a trailing newline, an empty replacement — and every
-- refusal that leaves the file untouched: not found, ambiguous, two edits on
-- one line, a stale base. The refusals also carry what a caller needs to
-- recover in one turn rather than by re-reading: how many times an ambiguous
-- snippet occurs, and every edit that failed rather than the first.

local dir = std.env.get("AGENT_BLOCK_HOME")
local path = dir .. "/target.txt"

local function check(label, cond)
    print(string.format("[SR] %-36s = %s", label, tostring(cond)))
end

local specs = std.fs.tool_specs({ allowed = { "read", "search_replace" }, prefix = "fs_" })
local read, sr
for _, s in ipairs(specs) do
    if s.name == "fs_read" then
        read = s.handler
    elseif s.name == "fs_search_replace" then
        sr = s.handler
    end
end
check("spec.registered", read ~= nil and sr ~= nil)

local function reset(content)
    std.fs.write(path, content)
    return read({ path = path })
end

-- inside a line ----------------------------------------------------------
local r = reset("alpha\nbravo\ncharlie\ndelta\n")
local res = sr({ path = path, base = r.version, edits = { { search = "rav", replace = "RAV" } } })
check("mid.ok", res.ok == true and res.applied == 1)
check("mid.content", std.fs.read(path) == "alpha\nbRAVo\ncharlie\ndelta\n")
check("mid.version_moves", type(res.version) == "string" and res.version ~= r.version)

-- spanning lines, ending with a newline -----------------------------------
r = reset("alpha\nbravo\ncharlie\ndelta\n")
res = sr({ path = path, base = r.version, edits = { { search = "bravo\ncharlie\n", replace = "B\nC\n" } } })
check("span.ok", res.ok == true)
check("span.content", std.fs.read(path) == "alpha\nB\nC\ndelta\n")

-- the first line ----------------------------------------------------------
r = reset("alpha\nbravo\n")
res = sr({ path = path, base = r.version, edits = { { search = "alpha", replace = "ALPHA" } } })
check("first.content", res.ok == true and std.fs.read(path) == "ALPHA\nbravo\n")

-- the last line of a file with no trailing newline -------------------------
r = reset("one\ntwo")
res = sr({ path = path, base = r.version, edits = { { search = "two", replace = "TWO" } } })
check("last.content", res.ok == true and std.fs.read(path) == "one\nTWO")

-- an empty replacement deletes the snippet (a whole line, here) ------------
r = reset("alpha\nbravo\ncharlie\n")
res = sr({ path = path, base = r.version, edits = { { search = "bravo\n", replace = "" } } })
check("delete.content", res.ok == true and std.fs.read(path) == "alpha\ncharlie\n")

-- two edits on different lines land together ------------------------------
r = reset("alpha\nbravo\ncharlie\n")
res = sr({
    path = path,
    base = r.version,
    edits = { { search = "alpha", replace = "A" }, { search = "charlie", replace = "C" } },
})
check("two.applied", res.ok == true and res.applied == 2)
check("two.content", std.fs.read(path) == "A\nbravo\nC\n")

-- not found: nothing changes ----------------------------------------------
r = reset("alpha\nbravo\n")
res = sr({ path = path, base = r.version, edits = { { search = "gamma", replace = "x" } } })
check("missing.rejected", res.ok == false and res.reason == "search_not_found" and res.edit_index == 1)
check("missing.file_untouched", std.fs.read(path) == "alpha\nbravo\n")

-- ambiguous: nothing changes, and the count says how far off unique it is --
r = reset("x\nx\nx\n")
res = sr({ path = path, base = r.version, edits = { { search = "x", replace = "y" } } })
check("ambiguous.rejected", res.ok == false and res.reason == "search_ambiguous")
check("ambiguous.counts_matches", res.matches == 3)
check("ambiguous.file_untouched", std.fs.read(path) == "x\nx\nx\n")

-- every edit that did not resolve is named, not just the first. A caller
-- working from a reading the file has moved past misses several snippets at
-- once, and the set is what says the region moved rather than one line.
r = reset("alpha\nbravo\ncharlie\n")
res = sr({
    path = path,
    base = r.version,
    edits = {
        { search = "alpha", replace = "A" },
        { search = "gamma", replace = "G" },
        { search = "delta", replace = "D" },
    },
})
check("all_failures.rejected", res.ok == false and res.reason == "search_not_found")
check("all_failures.first_on_top", res.edit_index == 2)
check("all_failures.reports_both", #res.failures == 2)
check("all_failures.indices", res.failures[1].edit_index == 2 and res.failures[2].edit_index == 3)
check("all_failures.file_untouched", std.fs.read(path) == "alpha\nbravo\ncharlie\n")

-- two edits on one line are the edit primitive's overlap -------------------
r = reset("alpha bravo\n")
res = sr({
    path = path,
    base = r.version,
    edits = { { search = "alpha", replace = "A" }, { search = "bravo", replace = "B" } },
})
check("overlap.rejected", res.ok == false and res.reason == "overlapping_edits")
check("overlap.file_untouched", std.fs.read(path) == "alpha bravo\n")

-- a stale base is the edit primitive's refusal too --------------------------
reset("alpha\n")
res = sr({ path = path, base = "0000000000000000", edits = { { search = "alpha", replace = "A" } } })
check("stale.rejected", res.ok == false and res.reason == "stale_base")
check("stale.file_untouched", std.fs.read(path) == "alpha\n")

-- malformed edits are refused before the file is read ----------------------
res = sr({ path = path, edits = {} })
check("empty.rejected", res.ok == false and res.reason == "no_edits")
res = sr({ path = path, edits = { { search = "", replace = "x" } } })
check("blank_search.rejected", res.ok == false and res.reason == "bad_edit")

print("[SR] done")
