-- Smoke example for std.ts.* — manual run:
--   agent-block -s examples/test_ts.lua
--
-- No external services required. Uses AGENT_BLOCK_HOME (default: ~/.agent-block)
-- or AGENT_BLOCK_TS_PATH to store ts.sqlite.
--
-- Expected output:
--   smoke_last_type=number (or table)
--   smoke_count=2
--   smoke_raw_count=2

local series = "smoke"

-- Append a numeric metric.
std.ts.append(series, 1)

-- Append a structured payload (table value).
std.ts.append(series, {ok = true, msg = "hello"})

-- last() returns the most-recent data point.
local last_row = std.ts.last(series)
print("smoke_last_type=" .. type(last_row.value))

-- query() with agg="count" aggregates all rows.
local count_result = std.ts.query(series, {agg = "count"})
print("smoke_count=" .. tostring(count_result[1].value))

-- query() in raw mode returns all rows ordered by ts.
local raw = std.ts.query(series, {})
print("smoke_raw_count=" .. tostring(#raw))
