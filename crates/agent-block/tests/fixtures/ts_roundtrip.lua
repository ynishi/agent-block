-- E2E fixture for std.ts.* — covers all three Crux Tier 1 constraints:
-- C1 dual-type value encoding, C2 tag AND filter via JSON1, C3 agg-bucket
-- optionality interaction. Also covers limit/offset pagination.

math.randomseed(os.time())
local s = "_e2e_ts_" .. tostring(os.time()) .. "_" .. tostring(math.random(1000, 9999))

-- ── (A) dual-type append: number and table round-trip ──────────────────────
-- Crux C1: std.ts.append must accept both number and table as value,
-- round-tripping through JSON encoding in SQLite without loss.
std.ts.append(s, 42, {task = "A"}, 1000)
std.ts.append(s, {x = 1, y = "ok"}, {task = "A"}, 2000)
local raw = std.ts.query(s, {from = 0, to = 9999, tags = {task = "A"}})
print("raw_count=" .. tostring(#raw))
print("num_value=" .. tostring(raw[1].value))
print("tbl_value_x=" .. tostring(raw[2].value.x))

-- ── (B) tag AND filter: two-tag conjunction via json_extract ───────────────
-- Crux C2: all key-value pairs in opts.tags are evaluated as conjunction
-- using json_extract, never as serialised-string equality.
std.ts.append(s, 1, {task = "B", phase = "p1"}, 3000)
std.ts.append(s, 2, {task = "B", phase = "p2"}, 4000)
std.ts.append(s, 3, {task = "B", phase = "p1"}, 5000)
-- Two-tag AND: task="B" AND phase="p1" should match rows at ts 3000 and 5000 (count=2).
-- The row at ts=4000 (phase="p2") must NOT match.
local filtered = std.ts.query(s, {from = 0, to = 9999, tags = {task = "B", phase = "p1"}, agg = "count"})
print("and_filter_count=" .. tostring(filtered[1].value))

-- ── (C) agg single (no bucket): count / sum / avg / last ──────────────────
-- Crux C3 (partial): agg without bucket_ms produces a single-aggregate row.
local cnt = std.ts.query(s, {from = 0, to = 9999, tags = {task = "B"}, agg = "count"})
print("agg_count=" .. tostring(cnt[1].value))

local sum_r = std.ts.query(s, {from = 0, to = 9999, tags = {task = "B"}, agg = "sum"})
print("agg_sum=" .. tostring(sum_r[1].value))

local lst = std.ts.query(s, {from = 0, to = 9999, tags = {task = "B"}, agg = "last"})
print("agg_last=" .. tostring(lst[1].value))

-- ── (D) agg + bucket: time-bucketed aggregation ────────────────────────────
-- Crux C3: agg with bucket_ms produces time-bucketed rows (distinct from
-- single-aggregate).  Use a dedicated series with timestamps at 0, 2000, 4000
-- so each falls in a separate bucket of width 2000 ms → bucket_count=3.
local sb = s .. "_b"
std.ts.append(sb, 10, {}, 0)
std.ts.append(sb, 20, {}, 2000)
std.ts.append(sb, 30, {}, 4000)
local bucketed = std.ts.query(sb, {from = 0, to = 9999, agg = "avg", bucket_ms = 2000})
print("bucket_count=" .. tostring(#bucketed))

-- ── (E) limit / offset pagination ─────────────────────────────────────────
-- Append 5 rows into a dedicated series and assert limit/offset semantics.
local sp = s .. "_p"
std.ts.append(sp, 1, {}, 1000)
std.ts.append(sp, 2, {}, 2000)
std.ts.append(sp, 3, {}, 3000)
std.ts.append(sp, 4, {}, 4000)
std.ts.append(sp, 5, {}, 5000)

local limited = std.ts.query(sp, {from = 0, to = 9999, limit = 2})
print("limited_count=" .. tostring(#limited))

local offset_rows = std.ts.query(sp, {from = 0, to = 9999, limit = 2, offset = 1})
print("offset_count=" .. tostring(#offset_rows))
-- offset=1 skips the first row; rows 2 and 3 (ts 2000, 3000) should be returned
print("offset_first_value=" .. tostring(offset_rows[1].value))
