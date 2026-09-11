-- Requires `session` and reports whether the copy that answered is a vendored
-- one.
--
-- Driven by tests/e2e_vendor.rs, which vendors `session` into a temp project
-- and marks the written file. The embedded module carries no `vendored` field,
-- so `nil` here means the project copy was not the one that resolved.
local session = require("session")
print("SESSION_VENDORED=" .. tostring(session.vendored))
