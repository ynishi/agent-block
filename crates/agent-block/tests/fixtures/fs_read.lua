-- The `fs_read` tool spec: a range of lines comes back as start-end/total
-- (the Content-Range shape), and a result cut short — by `limit`, by the
-- caller's `max_tokens`, or by the shell's window share — says so with
-- `truncated` / `cut_by` and an `end_line` a caller can continue from.
-- One `[RD] <label> = <bool>` line per property; the harness fails on any
-- `= false`.

local dir = std.env.get("AGENT_BLOCK_HOME")
local path = dir .. "/read.txt"
local lines = {}
for i = 1, 40 do
    lines[i] = string.format("line %02d %s", i, string.rep("x", 20))
end
std.fs.write(path, table.concat(lines, "\n") .. "\n")

local function check(label, cond)
    print(string.format("[RD] %-44s = %s", label, tostring(cond)))
end

-- A counter the shell would hand in: ~4 bytes per token, measured on the
-- encoded result (metadata included), the same text the shell measures.
local function count(s)
    return math.ceil(#s / 4)
end

local function specs_with(limits)
    local specs = std.fs.tool_specs({ allowed = { "read" }, prefix = "fs_", limits = limits })
    for _, s in ipairs(specs) do
        if s.name == "fs_read" then
            return s
        end
    end
end

local function n_lines(content)
    if content == "" then
        return 0
    end
    local n = 0
    for _ in (content .. "\n"):gmatch("(.-)\n") do
        n = n + 1
    end
    return n
end

-- no budget: whole file / range / start_line alone runs to the end ---------
local plain = specs_with(nil)
local r = plain.handler({ path = path })
check("whole.range", r.start_line == 1 and r.end_line == 40 and r.total == 40)
check("whole.content", n_lines(r.content) == 40 and r.truncated == nil and r.cut_by == nil)
check("whole.version", type(r.version) == "string" and #r.version > 0)
check("whole.schema.no_max_tokens", plain.input_schema.properties.max_tokens == nil)

r = plain.handler({ path = path, start_line = 10, end_line = 12 })
check(
    "range.10_12",
    r.start_line == 10 and r.end_line == 12 and n_lines(r.content) == 3 and r.content:sub(1, 7) == "line 10"
)

r = plain.handler({ path = path, start_line = 38 })
check(
    "start_only.runs_to_end",
    r.start_line == 38 and r.end_line == 40 and n_lines(r.content) == 3 and r.truncated == nil
)

r = plain.handler({ path = path, start_line = 5, end_line = 999 })
check("end_past_eof.clamped", r.end_line == 40 and r.total == 40 and r.truncated == nil)

r = plain.handler({ path = path, start_line = 41 })
check("start_past_eof.refused", r.ok == false and r.reason == "range_invalid" and r.total == 40)
r = plain.handler({ path = path, start_line = 9, end_line = 3 })
check("reversed_range.refused", r.ok == false and r.reason == "range_invalid")

-- limit --------------------------------------------------------------------
r = plain.handler({ path = path, limit = 7 })
check(
    "limit.cuts",
    r.truncated == true
        and r.cut_by == "limit"
        and r.start_line == 1
        and r.end_line == 7
        and n_lines(r.content) == 7
        and r.total == 40
)
r = plain.handler({ path = path, start_line = 30, limit = 100 })
check("limit.larger_than_range.no_cut", r.truncated == nil and r.end_line == 40)

-- shell budget -------------------------------------------------------------
local budgeted = specs_with({ result_tokens = 120, count = count })
check("budget.schema.has_max_tokens", budgeted.input_schema.properties.max_tokens ~= nil)
r = budgeted.handler({ path = path })
check(
    "budget.cuts",
    r.truncated == true and r.cut_by == "budget" and r.start_line == 1 and r.end_line < 40 and r.total == 40
)
check("budget.measured_on_encoded", count(std.json.encode(r)) <= 120)
check("budget.content_matches_range", n_lines(r.content) == r.end_line - r.start_line + 1)
local one_more = budgeted.handler({ path = path, start_line = 1, end_line = r.end_line + 1 })
check("budget.is_the_largest_fit", one_more.truncated == true and one_more.end_line == r.end_line)

-- caller's max_tokens: the smaller of the two wins ------------------------
local mt = budgeted.handler({ path = path, max_tokens = 60 })
check("max_tokens.smaller_wins", mt.truncated == true and mt.cut_by == "max_tokens" and mt.end_line < r.end_line)
check("max_tokens.measured_on_encoded", count(std.json.encode(mt)) <= 60)
local big = budgeted.handler({ path = path, max_tokens = 100000 })
check("max_tokens.larger_than_budget.budget_wins", big.cut_by == "budget" and big.end_line == r.end_line)
local ignored = plain.handler({ path = path, max_tokens = 10 })
check("max_tokens.without_counter.not_applied", ignored.truncated == nil and ignored.end_line == 40)

-- not even one line fits ---------------------------------------------------
local tiny = specs_with({ result_tokens = 10, count = count })
r = tiny.handler({ path = path })
check("too_large.refused", r.ok == false and r.reason == "result_too_large" and r.total == 40 and r.budget == 10)

-- empty file: a range of nothing, not an error ------------------------------
local empty = dir .. "/empty.txt"
std.fs.write(empty, "")
local specs_e = std.fs.tool_specs({ allowed = { "read" }, prefix = "fs_", path_lock = { path, empty } })
local read_e
for _, s in ipairs(specs_e) do
    if s.name == "fs_read" then
        read_e = s.handler
    end
end
r = read_e({ path = empty })
check(
    "empty.range_of_nothing",
    r.ok ~= false and r.content == "" and r.start_line == 1 and r.end_line == 0 and r.total == 0
)
r = read_e({ path = empty, start_line = 5 })
check("empty.start_past_eof.refused", r.ok == false and r.reason == "range_invalid")

-- resume: continuing from end_line + 1 reads the file exactly once ---------
local seen, calls, at = {}, 0, 1
while at <= 40 do
    local part = budgeted.handler({ path = path, start_line = at })
    calls = calls + 1
    if part.ok == false or calls > 40 then
        break
    end
    for i = part.start_line, part.end_line do
        seen[i] = (seen[i] or 0) + 1
    end
    at = part.end_line + 1
end
local exact = true
for i = 1, 40 do
    if seen[i] ~= 1 then
        exact = false
    end
end
check("resume.covers_file_exactly_once", exact and calls > 1)

-- limit and budget together: whichever cuts first is named -----------------
r = budgeted.handler({ path = path, limit = 2 })
check("limit_then_budget.limit_named", r.cut_by == "limit" and r.end_line == 2)

print("[RD] done")
