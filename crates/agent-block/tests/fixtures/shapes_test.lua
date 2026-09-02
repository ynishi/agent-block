-- shapes_test.lua — mlua-lspec unit tests for the blocks' boundary contracts.
--
-- Run via:
--   just test-lua shapes_test   # this file
--   just test-lua               # every spec fixture
--
-- The contracts themselves (agent.shapes / compile_loop.shapes) are plain data,
-- so they are checked here directly with `lshape.check.check` rather than
-- through the dev-mode assert at the call sites. That keeps the cases readable
-- and, more to the point, makes them run whether or not dev mode is on.
--
-- The first test is the exception: it pins the recipe. `assert_dev` is inert
-- unless LSHAPE_CHECK=1, so a contract that nothing enables is a comment with
-- extra steps — if `just test-lua` ever stops setting it, this is what says so.
--
-- The runtime injects std / log / tool as globals; the harness has none.

local describe, it, expect = lust.describe, lust.it, lust.expect

if not log then
    log = { warn = function() end, info = function() end, debug = function() end, error = function() end }
end
if not tool then
    tool = { register = function() end }
end
if not std then
    std = {
        env = {
            get = function(_name)
                return nil
            end,
            get_or = function(_name, default)
                return default
            end,
            agent_id = function()
                return nil
            end,
        },
        json = {
            encode = function(v)
                return tostring(v)
            end,
        },
        fs = {
            metadata = function(_path)
                return nil
            end,
            tool_specs = function(_opts)
                return {
                    {
                        name = "fs_edit",
                        description = "stub (shapes_test)",
                        input_schema = { type = "object" },
                        handler = function()
                            error("std.fs edit handler is not stubbed in this fixture", 0)
                        end,
                    },
                }
            end,
        },
    }
end

local lshape = require("lshape")
local check = lshape.check
local agent = require("agent")
local compile_loop = require("compile_loop")

describe("lshape dev mode", function()
    it("is on, so the call-site asserts are live in the specs", function()
        expect(check.is_dev_mode()).to.equal(true)
    end)
end)

describe("compile_loop.shapes.runner_result", function()
    local schema = compile_loop.shapes.runner_result

    it("accepts the full form the bundled runners return", function()
        local ok = check.check({ ok = true, stdout = "out", stderr = "", exit_code = 0 }, schema)
        expect(ok).to.equal(true)
    end)

    it("accepts ok on its own", function()
        expect(check.check({ ok = false }, schema)).to.equal(true)
    end)

    it("stays open to keys the loop does not read", function()
        expect(check.check({ ok = true, duration_ms = 12 }, schema)).to.equal(true)
    end)

    it("rejects a missing ok, which is the field the loop branches on", function()
        local ok, why = check.check({ stdout = "out" }, schema)
        expect(ok).to.equal(false)
        expect(why ~= nil).to.be.truthy()
    end)

    it("rejects a truthy non-boolean ok", function()
        expect(check.check({ ok = "yes" }, schema)).to.equal(false)
    end)

    it("rejects an exit_code that arrived as a string", function()
        expect(check.check({ ok = false, exit_code = "1" }, schema)).to.equal(false)
    end)

    it("rejects a runner that returned nothing", function()
        expect(check.check(nil, schema)).to.equal(false)
    end)
end)

describe("agent.shapes.log_meta", function()
    local schema = agent.shapes.log_meta

    it("accepts an entirely unset environment", function()
        expect(check.check({}, schema)).to.equal(true)
    end)

    it("accepts all four ids", function()
        local ok = check.check({
            trace_id = "t",
            run_id = "r",
            agent_id = "a",
            agent_name = "n",
        }, schema)
        expect(ok).to.equal(true)
    end)

    it("is closed, so a fifth key is drift rather than an extra", function()
        expect(check.check({ trace_id = "t", session_id = "s" }, schema)).to.equal(false)
    end)

    it("rejects an id that is not a string", function()
        expect(check.check({ run_id = 7 }, schema)).to.equal(false)
    end)
end)

describe("agent._log_meta", function()
    it("returns a value its own contract accepts", function()
        expect(check.check(agent._log_meta(nil), agent.shapes.log_meta)).to.equal(true)
    end)

    it("passes explicit log_meta through", function()
        local meta = agent._log_meta({ log_meta = { trace_id = "t-1", run_id = "r-1" } })
        expect(meta.trace_id).to.equal("t-1")
        expect(meta.run_id).to.equal("r-1")
    end)
end)
