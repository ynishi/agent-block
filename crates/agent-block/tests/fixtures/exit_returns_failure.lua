-- Returns a value that says the work did not succeed. The process finished,
-- so it exits 0: the exit code is about the process, and the answer is in
-- the value.
return std.json.encode({ ok = false, why = "nothing to drain" })
