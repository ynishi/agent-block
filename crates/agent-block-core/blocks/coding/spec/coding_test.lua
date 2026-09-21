-- coding_test.lua — mlua-lspec tests for the `coding` consumer block's pure
-- parts: what the seed is made of, how targets are resolved, and what
-- `coding.run` refuses before it opens a session.
--
-- Run via:
--   just test-lua coding_test
--
-- The loop itself is exercised end to end by running
-- `examples/coding_loop.lua` against a crate; nothing here calls a model.
--
-- What this proves:
--   1 targets: an array or a comma / newline string, trimmed, made absolute
--     under the repo, an absolute path left alone, an empty list refused;
--   2 numbered: 1-based numbers, a trailing newline does not add a line;
--   3 seed: every file goes in whole and numbered, however large, a missing
--     one is left out, and the spec comes first;
--   4 run: the opts it refuses — no spec, no verify, no targets, an llm
--     without port and conf, a non-integer iteration count.

local describe, it, expect = lust.describe, lust.it, lust.expect

local coding = require("coding")

describe("coding.targets", function()
    it("resolves an array under the repo and leaves an absolute path alone", function()
        local t = coding.targets({ "src/lib.rs", "/abs/x.rs" }, "/repo/")
        expect(t[1]).to.be("/repo/src/lib.rs")
        expect(t[2]).to.be("/abs/x.rs")
    end)

    it("splits a string on commas and newlines and trims", function()
        local t = coding.targets(" a.rs, b.rs\nc.rs ", "/r")
        expect(#t).to.be(3)
        expect(t[1]).to.be("/r/a.rs")
        expect(t[3]).to.be("/r/c.rs")
    end)

    it("refuses an empty list and a wrong type", function()
        expect(function()
            coding.targets(" , ", "/r")
        end).to.fail()
        expect(function()
            coding.targets(42, "/r")
        end).to.fail()
    end)
end)

describe("coding.numbered", function()
    it("numbers from 1 and does not count a trailing newline", function()
        expect(coding.numbered("a\nb\n")).to.be("1\ta\n2\tb")
        expect(coding.numbered("a\nb")).to.be("1\ta\n2\tb")
        expect(coding.numbered("")).to.be("")
    end)
end)

describe("coding.seed", function()
    local files = {
        ["/r/small.rs"] = "pub fn a() {}\n",
        ["/r/big.rs"] = string.rep("x\n", 100) .. "pub fn big() {}\n",
    }
    local function read(path)
        return files[path]
    end

    it("puts the spec first, a small file whole and numbered, a missing one nowhere", function()
        local seed = coding.seed("Do it.", { "/r/small.rs", "/r/none.rs" }, { read = read })
        expect(seed:sub(1, 6)).to.be("Do it.")
        expect(seed:find("## Current content of /r/small.rs", 1, true) ~= nil).to.be(true)
        expect(seed:find("1\tpub fn a() {}", 1, true) ~= nil).to.be(true)
        expect(seed:find("none.rs", 1, true)).to.be(nil)
    end)

    it("puts a large file in whole as well — nothing stands in for it", function()
        local seed = coding.seed("Do it.", { "/r/big.rs" }, { read = read })
        expect(seed:find("## Current content of /r/big.rs", 1, true) ~= nil).to.be(true)
        expect(seed:find("1\tx", 1, true) ~= nil).to.be(true)
        expect(seed:find("101\tpub fn big() {}", 1, true) ~= nil).to.be(true)
        expect(seed:find("Structural map", 1, true)).to.be(nil)
    end)
end)

describe("coding.run — what it refuses", function()
    local llm = { port = {}, conf = {} }

    it("needs a spec, a verify, targets, and an llm with port and conf", function()
        expect(function()
            coding.run({ verify = "true", targets = { "a" }, llm = llm })
        end).to.fail()
        expect(function()
            coding.run({ spec = "x", targets = { "a" }, llm = llm })
        end).to.fail()
        expect(function()
            coding.run({ spec = "x", verify = "true", llm = llm })
        end).to.fail()
        expect(function()
            coding.run({ spec = "x", verify = "true", targets = { "a" }, llm = { port = {} } })
        end).to.fail()
        expect(function()
            coding.run({ spec = "x", verify = "true", targets = { "a" }, llm = llm, iters = 0.5 })
        end).to.fail()
        expect(function()
            coding.run("not a table")
        end).to.fail()
    end)

    it("declares its opts and result shapes, baseline and no_edits included", function()
        expect(type(coding.shapes.run_opts)).to.be("table")
        expect(type(coding.shapes.run_result)).to.be("table")
        local check = require("lshape").check
        expect(
            check.check(
                { ok = true, iters = 1, summary = "PASS in 1 iters", baseline_ok = false },
                coding.shapes.run_result
            )
        ).to.be(true)
        expect(
            check.check(
                { ok = false, iters = 0, summary = "give-up", failure_reason = "seed_overflow" },
                coding.shapes.run_result
            )
        ).to.be(true)
    end)
end)
