-- compile_loop_adaptive_extra_tools_test.lua — mlua-lspec tests for
-- tool_mode="adaptive" (auto → none channel rescue) and conf.extra_tools
-- (caller-registered tool injection), driven through run_loop with the
-- _test_set_llm_call override (no HTTP, no real LLM).
--
-- Run via:
--   mcp__lua-debugger__test_launch(
--     code_file    = "crates/agent-block/tests/fixtures/compile_loop_adaptive_extra_tools_test.lua",
--     search_paths = ["crates/agent-block-core/blocks"]
--   )

local describe, it, expect = lust.describe, lust.it, lust.expect

if not log then
    log = { warn = function() end, info = function() end, debug = function() end }
end
if not std then
    std = {
        env = {
            get = function()
                return nil
            end,
            get_or = function(_n, d)
                return d
            end,
        },
        json = {
            encode = function(v)
                return tostring(v)
            end,
            decode = function(_s)
                error("decode stub not expected in these tests")
            end,
        },
    }
end

local CL = require("compile_loop")
local H = CL._test_helpers()
local run_loop = H.run_loop

local function contains(haystack, needle)
    return haystack:find(needle, 1, true) ~= nil
end

-- Create two temp target files with old content; returns paths.
local function make_targets()
    local fa, fb = os.tmpname(), os.tmpname()
    local f = assert(io.open(fa, "w"))
    f:write("old-a\n")
    f:close()
    f = assert(io.open(fb, "w"))
    f:write("old-b\n")
    f:close()
    return fa, fb
end

local function remove_targets(fa, fb)
    os.remove(fa)
    os.remove(fb)
end

-- Runner passes when every target file contains "new".
local function make_runner()
    return function(paths)
        for _, p in ipairs(paths) do
            local f = io.open(p, "r")
            if not f then
                return { ok = false, stdout = "", stderr = "cannot open " .. p, exit_code = 1 }
            end
            local c = f:read("*a") or ""
            f:close()
            if not c:find("new", 1, true) then
                return { ok = false, stdout = "", stderr = "no 'new' in " .. p, exit_code = 1 }
            end
        end
        return { ok = true, stdout = "", stderr = "", exit_code = 0 }
    end
end

local function sr_text_for(fa, fb)
    return "<<< path="
        .. fa
        .. " >>>\n<<<<<<< SEARCH\nold-a\n=======\nnew-a\n>>>>>>> REPLACE\n\n<<< path="
        .. fb
        .. " >>>\n<<<<<<< SEARCH\nold-b\n=======\nnew-b\n>>>>>>> REPLACE"
end

local function base_conf(fa, fb, extra)
    local conf = {
        runner = make_runner(),
        lang = "lua",
        target_files = { fa, fb },
        multi_file = true,
        spec = "change old to new in both files",
        edit_mode = "diff",
        max_iters = 5,
    }
    for k, v in pairs(extra or {}) do
        conf[k] = v
    end
    return conf
end

-- ─────────────────────────────────────────────────────────────────────────────
-- tool_mode = "adaptive"
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop tool_mode=adaptive", function()
    it("switches to the no-tools channel after repeated zero-edit iters", function()
        local fa, fb = make_targets()
        local calls = {}
        CL._test_set_llm_call(function(opts, messages)
            table.insert(calls, { tools = opts.tools, messages = messages })
            if #calls <= 2 then
                -- Iters 1-2: no SR text, no tool calls — zero-edit iters.
                return { choices = { { message = { content = "thinking about it", tool_use_blocks = {} } } } }
            end
            -- Iter 3 (post-switch): plain SR text.
            return { choices = { { message = { content = sr_text_for(fa, fb), tool_use_blocks = {} } } } }
        end)

        local res = run_loop(base_conf(fa, fb, { tool_mode = "adaptive" }))
        CL._test_reset_llm_call()

        expect(res.ok).to.equal(true)
        expect(#calls).to.equal(3)
        -- Calls 1-2 declare tools (adaptive starts as "auto").
        expect(type(calls[1].tools)).to.equal("table")
        expect(type(calls[2].tools)).to.equal("table")
        -- Call 3 is after the switch: no tools declared.
        expect(calls[3].tools).to.equal(nil)
        -- Post-switch user content embeds fresh file contents + no-tools directive.
        local user3 = calls[3].messages[2].content
        expect(contains(user3, "Current file content (path=")).to.equal(true)
        expect(contains(user3, "old-a")).to.equal(true)
        expect(contains(user3, "Do NOT call tools")).to.equal(true)
        remove_targets(fa, fb)
    end)

    it("rescues a tool-call-cap blowout by switching channels instead of failing", function()
        local fa, fb = make_targets()
        local calls = {}
        CL._test_set_llm_call(function(opts, _messages)
            table.insert(calls, { tools = opts.tools })
            if #calls == 1 then
                -- Iter 1: a read-loop blowout — more tool calls than the cap in one go.
                local blocks = {}
                for i = 1, 17 do
                    table.insert(blocks, { id = "tu" .. i, name = "read_file", input = { path = fa } })
                end
                return { choices = { { message = { content = "", tool_use_blocks = blocks } } } }
            end
            -- Iter 2 (post-switch): plain SR text.
            return { choices = { { message = { content = sr_text_for(fa, fb), tool_use_blocks = {} } } } }
        end)

        local res = run_loop(base_conf(fa, fb, { tool_mode = "adaptive" }))
        CL._test_reset_llm_call()

        expect(res.ok).to.equal(true)
        expect(res.iters).to.equal(2)
        expect(#calls).to.equal(2)
        expect(calls[2].tools).to.equal(nil)
        remove_targets(fa, fb)
    end)

    it("still fails with tool_loop when not adaptive and the cap is exceeded", function()
        local fa, fb = make_targets()
        CL._test_set_llm_call(function(_opts, _messages)
            local blocks = {}
            for i = 1, 17 do
                table.insert(blocks, { id = "tu" .. i, name = "read_file", input = { path = fa } })
            end
            return { choices = { { message = { content = "", tool_use_blocks = blocks } } } }
        end)

        local res = run_loop(base_conf(fa, fb, { tool_mode = "auto" }))
        CL._test_reset_llm_call()

        expect(res.ok).to.equal(false)
        expect(res.failure_reason).to.equal("tool_loop")
        remove_targets(fa, fb)
    end)
end)

-- ─────────────────────────────────────────────────────────────────────────────
-- conf.extra_tools
-- ─────────────────────────────────────────────────────────────────────────────

describe("compile_loop conf.extra_tools", function()
    it("declares, dispatches and round-trips a caller-registered tool", function()
        local fa, fb = make_targets()
        local hint_input = nil
        local calls = {}
        CL._test_set_llm_call(function(opts, messages)
            table.insert(calls, { tools = opts.tools, messages = messages })
            if #calls == 1 then
                return {
                    choices = {
                        {
                            message = {
                                content = "",
                                tool_use_blocks = { { id = "tu_hint", name = "get_hint", input = { topic = "x" } } },
                            },
                        },
                    },
                }
            end
            -- Second call (same iter, after tool_result): plain SR text.
            return { choices = { { message = { content = sr_text_for(fa, fb), tool_use_blocks = {} } } } }
        end)

        local res = run_loop(base_conf(fa, fb, {
            tool_mode = "auto",
            extra_tools = {
                {
                    name = "get_hint",
                    schema = {
                        description = "Return a build hint.",
                        input_schema = { type = "object", properties = {} },
                    },
                    handler = function(input)
                        hint_input = input
                        return "HINT-42"
                    end,
                },
            },
        }))
        CL._test_reset_llm_call()

        expect(res.ok).to.equal(true)
        expect(res.iters).to.equal(1)
        expect(hint_input.topic).to.equal("x")
        -- Declared alongside the 3 built-ins.
        expect(#calls[1].tools).to.equal(4)
        local declared = {}
        for _, t in ipairs(calls[1].tools) do
            declared[t.name] = true
        end
        expect(declared["get_hint"]).to.equal(true)
        -- The tool_result carrying the handler output reached the second call.
        local last_msg = calls[2].messages[#calls[2].messages]
        expect(last_msg.role).to.equal("user")
        expect(last_msg.content[1].type).to.equal("tool_result")
        expect(last_msg.content[1].tool_use_id).to.equal("tu_hint")
        expect(last_msg.content[1].content).to.equal("HINT-42")
        remove_targets(fa, fb)
    end)

    it("propagates a handler error as recoverable tool_result text", function()
        local fa, fb = make_targets()
        local calls = {}
        CL._test_set_llm_call(function(opts, messages)
            table.insert(calls, { tools = opts.tools, messages = messages })
            if #calls == 1 then
                return {
                    choices = {
                        {
                            message = {
                                content = "",
                                tool_use_blocks = { { id = "tu_boom", name = "boom", input = {} } },
                            },
                        },
                    },
                }
            end
            return { choices = { { message = { content = sr_text_for(fa, fb), tool_use_blocks = {} } } } }
        end)

        local res = run_loop(base_conf(fa, fb, {
            tool_mode = "auto",
            extra_tools = {
                {
                    name = "boom",
                    schema = { description = "always fails", input_schema = { type = "object", properties = {} } },
                    handler = function()
                        error("boom failed")
                    end,
                },
            },
        }))
        CL._test_reset_llm_call()

        expect(res.ok).to.equal(true)
        local last_msg = calls[2].messages[#calls[2].messages]
        expect(contains(last_msg.content[1].content, "ERROR:")).to.equal(true)
        expect(contains(last_msg.content[1].content, "boom failed")).to.equal(true)
        remove_targets(fa, fb)
    end)

    it("make() rejects reserved built-in names", function()
        local ok, err = pcall(CL.make, {
            runner = function() end,
            extra_tools = { { name = "read_file", handler = function() end } },
        })
        expect(ok).to.equal(false)
        expect(contains(tostring(err), "reserved")).to.equal(true)
    end)

    it("make() rejects an extra tool without a handler function", function()
        local ok, err = pcall(CL.make, {
            runner = function() end,
            extra_tools = { { name = "no_handler" } },
        })
        expect(ok).to.equal(false)
        expect(contains(tostring(err), "handler must be a function")).to.equal(true)
    end)
end)
