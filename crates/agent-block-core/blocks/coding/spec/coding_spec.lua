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
--     one is left out, and the spec comes first; `mode = "names"` lists the
--     paths and reads nothing, and a mode it does not know is refused;
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
--     carrying the `config` the run was started with;
--  12 config_of: where each value came from — `caller` for one the caller
--     named, `default` for one it did not, `discovered` for a window the conf
--     left to the port, and no entry at all for a value nobody gave;
--  13 result_of: the result out of a converged state and out of one that gave
--     up, and what plan mode puts on it — the counts, the checks still
--     failing, and a plan nobody filed;
--  14 ops_of: which std.fs ops the model is handed — the default two, one
--     edit op or several, a name given twice counted once, the shapes it
--     refuses, and that the names themselves are std.fs's to judge;
--  15 cut_at_limit: which stop_reason words say a reply was cut at the
--     output limit, and that a reply which finished is not one;
--  16 _beat_outcome: the kernel's four statuses as the loop reads them — the
--     answer, `max_iters` for a stop on the grant, `llm_call` for an error or
--     a refusal.

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

    it('mode = "names": every path, no content, and the model told to read what it needs', function()
        local seed = coding.seed("Do it.", { "/r/small.rs", "/r/big.rs" }, { read = read, mode = "names" })
        expect(seed:sub(1, 6)).to.be("Do it.")
        expect(seed:find("## Target files", 1, true) ~= nil).to.be(true)
        expect(seed:find("read the parts you need first", 1, true) ~= nil).to.be(true)
        expect(seed:find("/r/small.rs", 1, true) ~= nil).to.be(true)
        expect(seed:find("/r/big.rs", 1, true) ~= nil).to.be(true)
        -- Not one line of either file, and not one line number.
        expect(seed:find("pub fn a() {}", 1, true)).to.be(nil)
        expect(seed:find("pub fn big() {}", 1, true)).to.be(nil)
        expect(seed:find("## Current content of", 1, true)).to.be(nil)
    end)

    it("names a target the full mode would leave out — nothing is read to build it", function()
        -- `read` is never called, so a target that does not exist yet is in
        -- the list all the same: its path is what the model is being given.
        local seed = coding.seed("Do it.", { "/r/none.rs" }, {
            read = function()
                error("mode = names must not read a target")
            end,
            mode = "names",
        })
        expect(seed:find("/r/none.rs", 1, true) ~= nil).to.be(true)
    end)

    it("refuses a mode it does not know", function()
        expect(function()
            coding.seed("Do it.", { "/r/small.rs" }, { read = read, mode = "map" })
        end).to.fail()
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

    it('refuses a seed shape that is neither "full" nor "names"', function()
        expect(about(refusal({ seed = "map" }), '`seed` must be "full" or "names"')).to.be(true)
        expect(about(refusal({ seed = "names" }), "`seed`")).to.be(false)
    end)

    it("refuses a tool set it cannot read, with the other opts", function()
        expect(about(refusal({ ops = "search_replace" }), "`ops` must be a table")).to.be(true)
        expect(about(refusal({ ops = { edits = { "write" } } }), "`ops` has no option 'edits'")).to.be(true)
        -- A tool set it can read gets past this and on to the next refusal.
        expect(about(refusal({ ops = { edit = { "write", "append" } } }), "`ops`")).to.be(false)
    end)

    it("takes a reserve of whole tokens and nothing else", function()
        expect(about(refusal({ reserve = 0 }), "`reserve` must be a whole number")).to.be(true)
        expect(about(refusal({ reserve = "observed_max" }), "`reserve` must be a whole number")).to.be(true)
    end)

    it("takes a call_reserve of whole tokens and nothing else", function()
        expect(about(refusal({ call_reserve = -1 }), "`call_reserve` must be a whole number")).to.be(true)
        expect(about(refusal({ call_reserve = 1.5 }), "`call_reserve` must be a whole number")).to.be(true)
        -- Zero is a number a caller may mean: keep nothing back for the call.
        expect(about(refusal({ call_reserve = 0 }), "`call_reserve`")).to.be(false)
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
            ops = { read = "read", edit = { "search_replace" } },
            seed = "full",
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

describe("coding.ops_of — which std.fs ops the model is handed", function()
    it("is read + search_replace when the caller names none", function()
        local ops = coding.ops_of(nil)
        expect(ops.read).to.be("read")
        expect(ops.edit).to.equal({ "search_replace" })
    end)

    it("takes one edit op as a string and several as an array", function()
        expect(coding.ops_of({ edit = "write" }).edit).to.equal({ "write" })
        local several = coding.ops_of({ read = "read", edit = { "search_replace", "write", "append" } })
        expect(several.edit).to.equal({ "search_replace", "write", "append" })
        expect(several.read).to.be("read")
    end)

    it("names an op once however often the caller does — one tool, one entry", function()
        expect(coding.ops_of({ edit = { "write", "write", "append" } }).edit).to.equal({ "write", "append" })
    end)

    it("refuses a shape that is not a tool set, and a key it does not know", function()
        expect(function()
            coding.ops_of("search_replace")
        end).to.fail()
        expect(function()
            coding.ops_of({ edit = {} })
        end).to.fail()
        expect(function()
            coding.ops_of({ edit = { 7 } })
        end).to.fail()
        expect(function()
            coding.ops_of({ read = true })
        end).to.fail()
        expect(function()
            coding.ops_of({ edits = { "write" } })
        end).to.fail()
    end)

    it("does not check the names against a list of its own — that is std.fs's to answer", function()
        -- An op that does not exist gets this far; it is refused where the
        -- tool is built, by the module that knows which ops there are.
        expect(coding.ops_of({ edit = { "teleport" } }).edit).to.equal({ "teleport" })
    end)
end)

describe("coding.cut_at_limit — an answer that ran out of room", function()
    it("knows the word each dialect uses for a reply cut at the output limit", function()
        -- Anthropic says `max_tokens` itself; the OpenAI dialect's
        -- `map_finish_reason` turns `length` into the same word, and `length`
        -- is read as well for a port that hands the provider's own through.
        expect(coding.cut_at_limit("max_tokens")).to.be(true)
        expect(coding.cut_at_limit("length")).to.be(true)
    end)

    it("is false for a reply that finished, and for one with no word at all", function()
        expect(coding.cut_at_limit("end_turn")).to.be(false)
        expect(coding.cut_at_limit("tool_use")).to.be(false)
        expect(coding.cut_at_limit(nil)).to.be(false)
        expect(coding.cut_at_limit(4096)).to.be(false)
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

describe("coding.config_of — what the run was configured with", function()
    local conf = { model = "m", timeout = 600, max_tokens = 4096, context_window = 32768 }
    local resolved = {
        iters = 5,
        turns = 8,
        done = "declare",
        ops = { read = "read", edit = { "search_replace", "append" } },
        seed = "names",
        repo = "/r",
        system = "SYS",
        targets = { "/r/a.rs" },
        window = 32768,
    }
    local function config(over)
        local opts = { llm = { port = {}, conf = conf }, verify = "true" }
        for k, v in pairs(over or {}) do
            opts[k] = v
        end
        return coding.config_of(opts, resolved)
    end

    it("says caller for a knob the caller named and default for one it did not", function()
        local c = config({ iters = 5 })
        expect(c.values.iters.from).to.be("caller")
        expect(c.values.iters.value).to.be(5)
        expect(c.values.turns.from).to.be("default")
        expect(c.values.turns.value).to.be(8)
        expect(c.strict).to.be(false)
    end)

    it("says discovered for a window the conf did not declare, caller for one it did", function()
        local blind = config({ llm = { port = {}, conf = { model = "m", timeout = 600, max_tokens = 4096 } } })
        expect(blind.values.context_window.from).to.be("discovered")
        expect(blind.values.context_window.value).to.be(32768)
        expect(config({}).values.context_window.from).to.be("caller")
    end)

    it("names the values the run resolved, not the strings it was given", function()
        local c = config({ strict = true, repo = "/r/" })
        expect(c.strict).to.be(true)
        expect(c.values.repo.value).to.be("/r")
        expect(c.values.repo.from).to.be("caller")
        expect(c.values.targets.value[1]).to.be("/r/a.rs")
        expect(c.values.system.value).to.be("SYS")
        expect(c.values.verify.value).to.be("true")
        expect(c.values.llm_timeout.value).to.be(600)
    end)

    it("carries the ops the run resolved, and where they came from", function()
        local taken = config({})
        expect(taken.values.ops.from).to.be("default")
        expect(taken.values.ops.value.read).to.be("read")
        expect(taken.values.ops.value.edit).to.equal({ "search_replace", "append" })
        expect(config({ ops = { edit = { "write" } } }).values.ops.from).to.be("caller")
    end)

    it("carries the seed shape the run used", function()
        expect(config({}).values.seed.from).to.be("default")
        expect(config({}).values.seed.value).to.be("names")
        expect(config({ seed = "names" }).values.seed.from).to.be("caller")
    end)

    it("names a call_reserve the caller gave, and claims no value for one nobody did", function()
        local none = config({})
        expect(none.values.call_reserve.value).to.be(nil)
        expect(none.values.call_reserve.from).to.be("default")
        local given = config({ call_reserve = 3072 })
        expect(given.values.call_reserve.value).to.be(3072)
        expect(given.values.call_reserve.from).to.be("caller")
    end)

    it("claims no source for a value nobody gave", function()
        local c = config({ llm = { port = {}, conf = { timeout = 600 } } })
        expect(c.values.model).to.be(nil)
        expect(c.values.max_tokens).to.be(nil)
        expect(c.values.reserve).to.be(nil)
        -- Outside plan mode `check_timeout` has no value to name, so it reads
        -- as a `from` alone rather than as an absent knob.
        expect(c.values.check_timeout.value).to.be(nil)
        expect(c.values.check_timeout.from).to.be("default")
    end)
end)

describe("coding.result_of — the result out of the state the loop left", function()
    local check = require("lshape").check

    --- The state a finished run leaves behind, with a case's own fields over it.
    local function state(over)
        local st = {
            done = "declare",
            config = { strict = false, values = { iters = { value = 5, from = "default" } } },
            iters = 0,
            converged = false,
            failure_reason = nil,
            last_error = nil,
            session_id = "s-1",
            zero_edits = 0,
            baseline_ok = nil,
            edits_applied = 0,
            plan_steps = nil,
            last_plan = nil,
            last_checks = nil,
        }
        for k, v in pairs(over or {}) do
            st[k] = v
        end
        return st
    end

    it("a converged run: ok, the pass line, and no failure on it", function()
        local r = coding.result_of(state({ converged = true, iters = 2, baseline_ok = false }), 5)
        expect(r.ok).to.be(true)
        expect(r.iters).to.be(2)
        expect(r.summary).to.be("PASS in 2 iters")
        expect(r.failure_reason).to.be(nil)
        expect(r.last_error).to.be(nil)
        expect(r.baseline_ok).to.be(false)
        expect(r.session).to.be("s-1")
        expect(r.plan).to.be(nil)
        expect(check.check(r, coding.shapes.run_result)).to.be(true)
    end)

    it("a run that gave up: the reason, the last error, and where it stopped", function()
        local r = coding.result_of(state({ iters = 3, failure_reason = "no_edits", last_error = "error: nope" }), 5)
        expect(r.ok).to.be(false)
        expect(r.summary).to.be("give-up: no_edits at iter 3/5")
        expect(r.failure_reason).to.be("no_edits")
        expect(r.last_error).to.be("error: nope")
        expect(check.check(r, coding.shapes.run_result)).to.be(true)
    end)

    it("plan mode: the counts and the checks still failing, or a plan nobody filed", function()
        local checks = {
            { step = "a", check = "true", ok = true, exit_code = 0, tail = "" },
            { step = "b", check = "false", ok = false, exit_code = 1, tail = "" },
        }
        local r = coding.result_of(
            state({
                done = "plan",
                iters = 5,
                failure_reason = "max_iters",
                last_plan = { total = 2, passed = 1 },
                last_checks = checks,
            }),
            5
        )
        expect(r.done).to.be("plan")
        expect(r.plan.total).to.be(2)
        expect(r.plan.passed).to.be(1)
        expect(r.plan.filed).to.be(true)
        expect(#r.plan.failing).to.be(1)
        expect(r.plan.failing[1].step).to.be("b")
        expect(check.check(r, coding.shapes.run_result)).to.be(true)

        local none = coding.result_of(state({ done = "plan", iters = 1, failure_reason = "max_iters" }), 1)
        expect(none.plan.filed).to.be(false)
        expect(none.plan.total).to.be(0)
        expect(#none.plan.failing).to.be(0)
    end)
end)

describe("coding._beat_outcome — a beat's Outcome, as the loop reads it", function()
    local Outcome = require("knl").Outcome

    it("ok: the answer, and nothing to stop for", function()
        local answer, reason, err = coding._beat_outcome(Outcome.ok({ beat = "b-1" }))
        expect(answer.beat).to.be("b-1")
        expect(reason).to.be(nil)
        expect(err).to.be(nil)
    end)

    it("stopped on the grant is max_iters; any other stop keeps its own word", function()
        local answer, reason, err = coding._beat_outcome(Outcome.stopped("budget", "beats"))
        expect(answer).to.be(nil)
        expect(reason).to.be("max_iters")
        expect(err).to.be("budget")
        local none, other = coding._beat_outcome(Outcome.stopped("closed"))
        expect(none).to.be(nil)
        expect(other).to.be("stopped")
    end)

    it("an error and a refusal are both llm_call, carrying what was said", function()
        local a, reason, err = coding._beat_outcome(Outcome.err("call", { kind = "timeout", message = "no answer" }))
        expect(a).to.be(nil)
        expect(reason).to.be("llm_call")
        expect(err).to.be("timeout: no answer")
        local b, r2, e2 = coding._beat_outcome(Outcome.err("state", "closed"))
        expect(b).to.be(nil)
        expect(r2).to.be("llm_call")
        expect(e2).to.be("state: closed")
        local c, r3, e3 = coding._beat_outcome(Outcome.refused("model", { kind = "refusal", message = "declined" }))
        expect(c).to.be(nil)
        expect(r3).to.be("llm_call")
        expect(e3).to.be("refusal: declined")
    end)
end)
