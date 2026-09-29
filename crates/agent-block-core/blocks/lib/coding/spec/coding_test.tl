-- coding_test.tl — `htl test` for the `coding` block: the contracts it
-- publishes as data. The Lua spec beside it holds the pure helpers and what
-- `coding.run` refuses; the loop itself runs end to end in `e2e_coding.rs`.
--
-- Run via `just test-tl` (walks `blocks/lib` for `*_test.tl`; one state per
-- file).
local t = require("htl.test")
local coding = require("coding")

t.describe("shapes", function()
    t.it("publishes its two contracts", function()
        t.expect(coding.shapes.run_opts):to_not_be_nil()
        t.expect(coding.shapes.run_result):to_not_be_nil()
    end)
end)
