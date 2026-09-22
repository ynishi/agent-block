-- coding_spec.lua — mlua-lspec tests for the `coding` consumer block's pure
-- parts: what the seed is made of, how targets are resolved, and what
-- `coding.run` refuses before it opens a session.
--
-- Run via:
--   just test-lua coding_spec
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
--     without port and conf, a non-integer iteration count, a `done` that is
--     neither mode, and `done = "plan"` with no `check_timeout`;
--   5 run, the model's facts (the tripwires): a conf naming the reply neither
--     a cap nor a reserve is refused and either one answers it, a reserve
--     that is not whole tokens is refused, a conf with no timeout is refused,
--     and a port whose profile names no window is refused before a session
--     opens;
--   6 run, strict: `strict = true` with an Exec knob left unnamed is refused
--     and the refusal names the knob; one that names them all gets past it;
--   7 decide: what ends a run — the model answering with no tool call while an
--     edit has landed and the verify is green, and, in plan mode, every check
--     it filed passing; a green verify alone never ends one;
--   8 plan_of: the plan tool's input, as an array or as a JSON string, and
--     what it refuses;
--   9 plan_report: the counts, the failing check with its exit and tail, and
--     the note a check that printed nothing gets;
--  10 system: which ending paragraph each mode states, that no mode means
--     declare, and that a mode it does not know fails;
--  11 shapes: a plan-mode result, with and without a failing check, and one
--     carrying the `config` the run was started with.

local describe, it, expect = lust.describe, lust.it, lust.expect

if rawget(_G, "std") == nil then
    -- The pure spec runner carries no host bridges, and `plan_of` reaches for
    -- `std.json.decode` when a model sends its steps as a string instead of an
    -- array. This stands in for the host's decoder over the one shape the plan
    -- tool takes: an array of objects whose values are strings.
    local function decode(text)
        local out = {}
        for object in text:gmatch("%b{}") do
            local item = {}
            for key, value in object:gmatch('"([^"]+)"%s*:%s*"([^"]*)"') do
                item[key] = value
            end
            out[#out + 1] = item
        end
        return out
    end
    _G.std = { json = { decode = decode } }
end

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
    -- The model's facts, which the run now refuses to start without. Every
    -- case below has to get past them to reach the thing it is about, so they
    -- sit in the shared conf rather than in each case.
    local llm = { port = {}, conf = { max_tokens = 4096, timeout = 600 } }

    --- The message `coding.run` refused the given opts with. Every run in
    --- this file fails — the pure runner carries no host bridges, so nothing
    --- here reaches a session — and what a case asserts is WHICH refusal it
    --- got, not that there was one.
    local function refusal(over)
        local opts = { spec = "x", verify = "true", targets = { "a" }, llm = llm }
        for k, v in pairs(over or {}) do
            opts[k] = v
        end
        local ok, err = pcall(coding.run, opts)
        expect(ok).to.be(false)
        return tostring(err)
    end

    --- Whether that message is the one about `text`.
    local function about(message, text)
        return message:find(text, 1, true) ~= nil
    end

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

    it('refuses a done it does not know, and done = "plan" without a check_timeout', function()
        expect(function()
            coding.run({ spec = "x", verify = "true", targets = { "a" }, llm = llm, done = "x" })
        end).to.fail()
        expect(function()
            coding.run({ spec = "x", verify = "true", targets = { "a" }, llm = llm, done = "plan" })
        end).to.fail()
        expect(function()
            coding.run({
                spec = "x",
                verify = "true",
                targets = { "a" },
                llm = llm,
                done = "plan",
                check_timeout = 0,
            })
        end).to.fail()
    end)

    it("needs the reply's room named — a cap on the wire, or a reserve held back", function()
        expect(about(refusal({ llm = { port = {}, conf = { timeout = 600 } } }), "the reply's room")).to.be(true)
        -- Either one answers it, and the run gets past that tripwire.
        expect(about(refusal({}), "the reply's room")).to.be(false)
        expect(about(refusal({ llm = { port = {}, conf = { timeout = 600 } }, reserve = 4096 }), "the reply's room")).to.be(
            false
        )
    end)

    it("takes a reserve of whole tokens and nothing else", function()
        expect(about(refusal({ reserve = 0 }), "`reserve` must be a whole number")).to.be(true)
        expect(about(refusal({ reserve = "observed_max" }), "`reserve` must be a whole number")).to.be(true)
    end)

    it("needs the reply's seconds named", function()
        expect(about(refusal({ llm = { port = {}, conf = { max_tokens = 4096 } } }), "the reply's seconds")).to.be(true)
    end)

    it("needs the model's window — declared in the conf, or asked of the server", function()
        -- A port whose profile answers no window. The run refuses on it
        -- before it opens a session, so this needs no host behind it.
        local function port_seeing(context_window)
            return {
                count = function()
                    return 1
                end,
                profile = function()
                    return { context_window = context_window }
                end,
            }
        end
        local blind = { llm = { port = port_seeing(nil), conf = { max_tokens = 4096, timeout = 600 } } }
        expect(about(refusal(blind), "the model's window is not named")).to.be(true)
        local declared = {
            llm = { port = port_seeing(32768), conf = { context_window = 32768, max_tokens = 4096, timeout = 600 } },
        }
        expect(about(refusal(declared), "the model's window is not named")).to.be(false)
    end)

    it("strict = true: every Exec knob is the caller's to state, and the refusal names them", function()
        local named = {
            strict = true,
            turns = 8,
            timeout = 360,
            result_share = 0.25,
            repeat_max = 2,
            done = "declare",
        }
        local without_iters = {}
        for k, v in pairs(named) do
            without_iters[k] = v
        end
        local said = refusal(without_iters)
        expect(about(said, "strict = true")).to.be(true)
        expect(about(said, "these are not: iters")).to.be(true)
        named.iters = 5
        expect(about(refusal(named), "strict = true")).to.be(false)
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

describe("coding.decide — what ends a run", function()
    local base = { declared = true, verify_ok = true, edits_applied = 1 }
    local function with(over)
        local f = {}
        for k, v in pairs(base) do
            f[k] = v
        end
        for k, v in pairs(over) do
            f[k] = v
        end
        return f
    end

    it("declare: the model answering without a tool call, green, with an edit landed", function()
        expect(coding.decide("declare", base)).to.be(true)
    end)

    it("declare: a green verify alone never ends the run", function()
        expect(coding.decide("declare", with({ declared = false }))).to.be(false)
    end)

    it("declare: declaring on a red verify, or with nothing edited, does not end it", function()
        expect(coding.decide("declare", with({ verify_ok = false }))).to.be(false)
        expect(coding.decide("declare", with({ edits_applied = 0 }))).to.be(false)
    end)

    it("plan: needs a filed plan with every check passing, on top of declare's facts", function()
        expect(coding.decide("plan", base)).to.be(false)
        expect(coding.decide("plan", with({ plan = { total = 3, passed = 3 } }))).to.be(true)
        expect(coding.decide("plan", with({ plan = { total = 3, passed = 2 } }))).to.be(false)
        expect(coding.decide("plan", with({ plan = { total = 0, passed = 0 } }))).to.be(false)
        expect(coding.decide("plan", with({ plan = { total = 3, passed = 3 }, declared = false }))).to.be(false)
        expect(coding.decide("plan", with({ plan = { total = 3, passed = 3 }, verify_ok = false }))).to.be(false)
    end)
end)

describe("coding.plan_of — the plan tool's input", function()
    it("accepts steps with a step and a check", function()
        local steps = coding.plan_of({ steps = { { step = "add fn", check = "grep -q 'fn double' src/lib.rs" } } })
        expect(#steps).to.be(1)
        expect(steps[1].check).to.be("grep -q 'fn double' src/lib.rs")
    end)

    it("accepts the same array sent as a JSON string", function()
        local steps = coding.plan_of({ steps = '[{"step":"add fn","check":"grep -q double src/lib.rs"}]' })
        expect(#steps).to.be(1)
        expect(steps[1].step).to.be("add fn")
        expect(steps[1].check).to.be("grep -q double src/lib.rs")
    end)

    it("refuses an empty plan, a step without a check, a blank step", function()
        local s, err = coding.plan_of({ steps = {} })
        expect(s).to.be(nil)
        expect(err:find("non%-empty array") ~= nil).to.be(true)
        s, err = coding.plan_of({ steps = { { step = "x" } } })
        expect(s).to.be(nil)
        expect(err:find("steps%[1%].check") ~= nil).to.be(true)
        s, err = coding.plan_of({ steps = { { step = "  ", check = "true" } } })
        expect(s).to.be(nil)
        expect(err:find("steps%[1%].step") ~= nil).to.be(true)
        expect(coding.plan_of(nil)).to.be(nil)
    end)
end)

describe("coding.plan_report — the checks as facts", function()
    it("counts passes and shows the failing check with its exit and tail", function()
        local text, passed = coding.plan_report({
            { step = "add fn", check = "grep -q double src/lib.rs", ok = true, exit_code = 0, tail = "" },
            { step = "add test", check = "cargo test double", ok = false, exit_code = 101, tail = "error: no test" },
        })
        expect(passed).to.be(1)
        expect(text:sub(1, 30)).to.be("<harness>plan: 1/2 checks pass")
        expect(text:find("[pass] 1. add fn", 1, true) ~= nil).to.be(true)
        expect(text:find("[fail] 2. add test", 1, true) ~= nil).to.be(true)
        expect(text:find("check: cargo test double -> exit 101", 1, true) ~= nil).to.be(true)
        expect(text:find("error: no test", 1, true) ~= nil).to.be(true)
        expect(text:sub(-10)).to.be("</harness>")
    end)

    it("says a failing check printed nothing, so its exit is all it reported", function()
        local text, passed = coding.plan_report({
            {
                step = "twelve tests",
                check = "test $(grep -c 'fn test_' src/lib.rs) -ge 12",
                ok = false,
                exit_code = 1,
                tail = "",
            },
        })
        expect(passed).to.be(0)
        expect(text:find("printed nothing", 1, true) ~= nil).to.be(true)
        expect(text:find("have it print the value", 1, true) ~= nil).to.be(true)
    end)
end)

describe("coding.system — states how the run ends", function()
    it("declare: no tool call + green + an edit; green alone is not the end", function()
        local sys = coding.system("fs_read", "fs_search_replace", "declare")
        expect(sys:find("you answer without a tool call", 1, true) ~= nil).to.be(true)
        expect(sys:find("The verify passing by itself does not end the run", 1, true) ~= nil).to.be(true)
        expect(sys:find("`plan` tool", 1, true)).to.be(nil)
    end)

    it("plan: names the plan tool and the checks", function()
        local sys = coding.system("fs_read", "fs_search_replace", "plan")
        expect(sys:find("file a plan with the `plan` tool", 1, true) ~= nil).to.be(true)
        expect(sys:find("every check passes", 1, true) ~= nil).to.be(true)
    end)

    it("no mode is declare, and a mode it does not know fails", function()
        expect(coding.system("r", "e")).to.be(coding.system("r", "e", "declare"))
        expect(function()
            coding.system("r", "e", "green")
        end).to.fail()
    end)
end)

describe("coding.shapes — done and plan on the result", function()
    local check = require("lshape").check

    it("accepts a plan-mode result with nothing left failing", function()
        expect(check.check({
            ok = true,
            iters = 2,
            summary = "PASS in 2 iters",
            done = "plan",
            plan = { total = 3, passed = 3, filed = true, failing = {} },
        }, coding.shapes.run_result)).to.be(true)
    end)

    it("accepts one carrying the config the run was started with", function()
        expect(check.check({
            ok = true,
            iters = 1,
            summary = "PASS in 1 iters",
            config = { strict = false, values = { iters = { value = 5, from = "default" } } },
        }, coding.shapes.run_result)).to.be(true)
    end)

    it("accepts one that names the check still failing", function()
        expect(check.check({
            ok = false,
            iters = 5,
            summary = "give-up: max_iters at iter 5/5",
            done = "plan",
            failure_reason = "max_iters",
            plan = {
                total = 3,
                passed = 2,
                filed = true,
                failing = {
                    {
                        step = "twelve tests",
                        check = "test $(grep -c 'fn test_' src/lib.rs) -ge 12",
                        exit_code = 1,
                        tail = "",
                    },
                },
            },
        }, coding.shapes.run_result)).to.be(true)
    end)
end)
