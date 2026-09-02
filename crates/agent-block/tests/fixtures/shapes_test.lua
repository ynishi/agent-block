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

describe("compile_loop.shapes.tool_output", function()
    local schema = compile_loop.shapes.tool_output

    it("accepts the single-file form", function()
        local ok = check.check({
            ok = true,
            iters = 4,
            summary = "PASS in 4 iters",
            artifact_path = "/abs/x.lua",
        }, schema)
        expect(ok).to.equal(true)
    end)

    it("accepts the multi-file form", function()
        local ok = check.check({
            ok = true,
            iters = 2,
            summary = "PASS in 2 iters",
            modified_files = { "/abs/a.lua", "/abs/b.lua" },
        }, schema)
        expect(ok).to.equal(true)
    end)

    it("accepts a failure carrying its reason", function()
        local ok = check.check({
            ok = false,
            iters = 5,
            summary = "give-up: max_iters reached (5)",
            failure_reason = "max_iters",
            last_error = "still failing",
        }, schema)
        expect(ok).to.equal(true)
    end)

    -- The Counter WF-A defence, as something that fails rather than something
    -- that is written down next to the fields it is about.
    it("rejects leaked code", function()
        local ok = check.check({
            ok = true,
            iters = 1,
            summary = "PASS in 1 iters",
            code = "print('leaked source')",
        }, schema)
        expect(ok).to.equal(false)
    end)

    it("rejects leaked history", function()
        local ok = check.check({
            ok = true,
            iters = 1,
            summary = "PASS in 1 iters",
            history = { { iter = 1 } },
        }, schema)
        expect(ok).to.equal(false)
    end)

    it("rejects modified_files holding something other than paths", function()
        local ok = check.check({
            ok = true,
            iters = 1,
            summary = "PASS in 1 iters",
            modified_files = { 1, 2 },
        }, schema)
        expect(ok).to.equal(false)
    end)
end)

describe("agent.shapes.run_result", function()
    local schema = agent.shapes.run_result
    local usage = { input_tokens = 1, output_tokens = 2, total_tokens = 3 }

    it("accepts a success carrying content", function()
        local ok = check.check({
            ok = true,
            content = "done",
            usage = usage,
            num_turns = 2,
            messages = {},
        }, schema)
        expect(ok).to.equal(true)
    end)

    it("accepts a failure carrying an error", function()
        local ok = check.check({
            ok = false,
            error = "prompt is required",
            usage = usage,
            num_turns = 0,
            messages = {},
        }, schema)
        expect(ok).to.equal(true)
    end)

    -- Neither alternative admits this, which is the reason for two shapes
    -- rather than one with both fields optional.
    it("rejects a result that says neither what happened nor what went wrong", function()
        local ok = check.check({
            ok = true,
            usage = usage,
            num_turns = 1,
            messages = {},
        }, schema)
        expect(ok).to.equal(false)
    end)

    it("rejects a success that also carries an error", function()
        local ok = check.check({
            ok = true,
            content = "done",
            error = "but also this",
            usage = usage,
            num_turns = 1,
            messages = {},
        }, schema)
        expect(ok).to.equal(false)
    end)

    it("rejects usage missing its counters", function()
        local ok = check.check({
            ok = true,
            content = "done",
            usage = { input_tokens = 1 },
            num_turns = 1,
            messages = {},
        }, schema)
        expect(ok).to.equal(false)
    end)

    it("accepts a tracker summary carrying thinking_tokens", function()
        local ok = check.check({
            ok = true,
            content = "done",
            usage = { input_tokens = 1, output_tokens = 2, total_tokens = 3, thinking_tokens = 4 },
            num_turns = 1,
            messages = {},
        }, schema)
        expect(ok).to.equal(true)
    end)
end)

-- The call site for this one is exercised by the Rust e2e tests, which run real
-- MCP servers through the bridge; these cases pin the shape itself.
describe("agent.shapes.mcp_call_result", function()
    local schema = agent.shapes.mcp_call_result

    it("accepts a tool that ran and succeeded", function()
        local ok = check.check({
            ok = true,
            content = { { type = "text", text = "result" } },
            is_error = false,
        }, schema)
        expect(ok).to.equal(true)
    end)

    it("accepts a tool that ran and reported failure", function()
        local ok = check.check({
            ok = true,
            content = { { type = "text", text = "boom" } },
            is_error = true,
        }, schema)
        expect(ok).to.equal(true)
    end)

    it("accepts a transport failure", function()
        expect(check.check({ ok = false, error = "connection refused" }, schema)).to.equal(true)
    end)

    it("accepts structured content when the server sends it", function()
        local ok = check.check({
            ok = true,
            content = {},
            is_error = false,
            structured_content = { rows = 3 },
        }, schema)
        expect(ok).to.equal(true)
    end)

    -- Closed on purpose: the bridge builds this table in Rust, and a field
    -- appearing here that this side does not know about is the drift.
    it("rejects a field the Lua side does not know about", function()
        expect(check.check({ ok = true, content = {}, cost_usd = 0.01 }, schema)).to.equal(false)
    end)

    it("rejects is_error arriving as a string", function()
        expect(check.check({ ok = true, content = {}, is_error = "true" }, schema)).to.equal(false)
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
