-- fs_tools_spec.lua — mlua-lspec unit tests for the `fs_tools` library, the
-- module the `std.fs` bridge installs as `std.fs.tool_specs` /
-- `std.fs.register_tools`.
--
-- Run via:
--   just test-lua fs_tools
--
-- The pure runner has no host bridges, so the primitives the code paths under
-- test reach — `std.fs.read_versioned`, `std.json.encode`, `log.debug`,
-- `tool.register` — are stubbed here, each doing the least the path needs and
-- recording what it was handed. The stubs stay in this file rather than a
-- shared `support.lua`: they are four lines, and no other spec wants them.
--
-- What this proves:
--   1 `require("fs_tools")` answers a table — the module's exports and its
--     shapes, not `true`. That is what the delegation idiom rests on, and it
--     is what this module did NOT do while it was a script the bridge ran;
--   2 `allowed` is refused by name when it is missing or empty, in both
--     modes, because it is a hand-written refusal and not the dev shape;
--     `limits.result_tokens` without `limits.count` likewise;
--   3 what `tool_specs` answers is an array of tool specs — `name`,
--     `description`, `input_schema`, `handler` — with the caller's prefix on
--     every name, and only the ops the caller allowed;
--   4 `path_lock` is the handler's, not the caller's: a path outside the lock
--     is refused by the handler before the bridge is touched at all;
--   5 `register_tools` puts the same specs in the registry and answers their
--     names;
--   6 `append` adds to the end of a file that is there, refuses one that is
--     not by pointing at `write`, and is path-locked like every other op.

local describe, it, expect = lust.describe, lust.it, lust.expect

-- ── the host, as much of it as these paths reach ─────────────────────────────

--- Every path `std.fs.read_versioned` was called with, in order. A handler
--- that refuses before reading leaves this empty, which is the assertion in
--- the `path_lock` case below.
local reads = {}

--- Every `std.fs.write`, in order: `{ path, content }` each. The `append` op
--- is a read and a write, and what it wrote is the whole of what it did.
local writes = {}

--- What a path holds. A path that is not in here is one the bridge cannot
--- read, and the stub raises for it exactly as the real `read_versioned`
--- does — which is how `append` tells a file that is not there from one that
--- is.
local files = { ["/work/kept.rs"] = "alpha\nbravo\n" }

_G.std = {
    fs = {
        read_versioned = function(path)
            reads[#reads + 1] = path
            local content = files[path]
            if content == nil then
                error("fs.edit: cannot read " .. tostring(path) .. ": No such file or directory")
            end
            local _, newlines = content:gsub("\n", "")
            return { content = content, lines = newlines, version = "v1" }
        end,
        write = function(path, content)
            writes[#writes + 1] = { path = path, content = content }
            files[path] = content
        end,
    },
    json = {
        encode = function(value)
            return tostring(value)
        end,
    },
}

_G.log = { debug = function() end }

--- Everything handed to `tool.register`, in order.
local registered = {}
_G.tool = {
    register = function(name, meta, handler)
        registered[#registered + 1] = { name = name, meta = meta, handler = handler }
    end,
}

local fs_tools = require("fs_tools")

--- The message `fn` failed with. `expect(...).to.fail()` says that it failed;
--- these cases are about WHICH field the refusal names.
local function refusal(fn)
    local ok, err = pcall(fn)
    expect(ok).to.be(false)
    return tostring(err)
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("fs_tools — the module", function()
    it("answers a table of functions and its shapes", function()
        expect(type(fs_tools)).to.be("table")
        expect(type(fs_tools.tool_specs)).to.be("function")
        expect(type(fs_tools.register_tools)).to.be("function")
        expect(type(fs_tools.shapes)).to.be("table")
        expect(type(fs_tools.shapes.tool_specs_opts)).to.be("table")
        -- `register_tools` forwards its opts untouched, so the contract is
        -- the same one under the second name.
        expect(fs_tools.shapes.register_tools_opts).to.be(fs_tools.shapes.tool_specs_opts)
    end)

    it("installs nothing on `std.fs` itself — a library is not wiring", function()
        expect(std.fs.tool_specs).to.be(nil)
        expect(std.fs.register_tools).to.be(nil)
    end)
end)

describe("fs_tools.tool_specs — the refusals that name a field", function()
    it("names `allowed` when it is missing or empty", function()
        expect(refusal(function()
            fs_tools.tool_specs()
        end):find("`allowed` is required", 1, true) ~= nil).to.be(true)
        expect(refusal(function()
            fs_tools.tool_specs({ allowed = {} })
        end):find("`allowed` is required", 1, true) ~= nil).to.be(true)
    end)

    it("names the field an `allowed` of the wrong shape sits at, when the dev contract is on", function()
        -- The other half of the split: the function owns "is `allowed` there
        -- at all", the shape owns "is what is there the right shape". This one
        -- is dev-only, so the case pins the mode rather than the environment.
        local check = require("lshape.check")
        local saved = check.is_dev_mode
        check.is_dev_mode = function()
            return true
        end
        local message = refusal(function()
            fs_tools.tool_specs({ allowed = "read" })
        end)
        check.is_dev_mode = saved

        expect(message:find("$.allowed", 1, true) ~= nil).to.be(true)
        expect(message:find("std.fs.tool_specs opts", 1, true) ~= nil).to.be(true)
    end)

    it("names an option it does not know, rather than letting the typo be a no-op", function()
        local check = require("lshape.check")
        local saved = check.is_dev_mode
        check.is_dev_mode = function()
            return true
        end
        local message = refusal(function()
            fs_tools.tool_specs({ allowed = { "read" }, path_lok = { "/work/kept.rs" } })
        end)
        check.is_dev_mode = saved

        expect(message:find("$.path_lok", 1, true) ~= nil).to.be(true)
    end)

    it("names `limits.count` when there is a budget and nothing to measure it with", function()
        local message = refusal(function()
            fs_tools.tool_specs({ allowed = { "read" }, limits = { result_tokens = 100 } })
        end)
        expect(message:find("limits.count", 1, true) ~= nil).to.be(true)
    end)
end)

describe("fs_tools.tool_specs — what it answers", function()
    it("is one spec per allowed op, prefixed, each a name / description / schema / handler", function()
        local specs = fs_tools.tool_specs({ allowed = { "read", "search_replace" }, prefix = "probe_" })
        expect(#specs).to.be(2)
        expect(specs[1].name).to.be("probe_read")
        expect(specs[2].name).to.be("probe_search_replace")
        for _, spec in ipairs(specs) do
            expect(type(spec.name)).to.be("string")
            expect(type(spec.description)).to.be("string")
            expect(type(spec.input_schema)).to.be("table")
            expect(type(spec.handler)).to.be("function")
        end
    end)

    it("hands out only the ops the caller allowed", function()
        local specs = fs_tools.tool_specs({ allowed = { "read" } })
        expect(#specs).to.be(1)
        expect(specs[1].name).to.be("fs_read")
    end)
end)

describe("fs_tools.tool_specs — path_lock", function()
    it("refuses a path outside the lock before the bridge is touched", function()
        reads = {}
        local specs = fs_tools.tool_specs({ allowed = { "read" }, path_lock = { "/work/kept.rs" } })
        local answer = specs[1].handler({ path = "/etc/passwd" })

        expect(answer.ok).to.be(false)
        expect(answer.reason).to.be("path_not_allowed")
        expect(answer.error:find("/work/kept.rs", 1, true) ~= nil).to.be(true)
        -- The refusal is the handler's own: nothing reached `std.fs`.
        expect(#reads).to.be(0)
    end)

    it("says `path_missing` rather than `path_not_allowed` when there is no path at all", function()
        reads = {}
        local specs = fs_tools.tool_specs({ allowed = { "read" }, path_lock = { "/work/kept.rs" } })
        local answer = specs[1].handler({ start_line = 1 })

        expect(answer.reason).to.be("path_missing")
        expect(#reads).to.be(0)
    end)

    it("lets a locked path through to the bridge", function()
        reads = {}
        local specs = fs_tools.tool_specs({ allowed = { "read" }, path_lock = { "/work/kept.rs" } })
        local answer = specs[1].handler({ path = "/work/kept.rs" })

        expect(answer.ok).to.be(nil)
        expect(answer.total).to.be(2)
        expect(reads[1]).to.be("/work/kept.rs")
    end)
end)

describe("fs_tools.tool_specs — append", function()
    --- The `append` spec, locked to the one file the tests write.
    local function append_spec()
        return fs_tools.tool_specs({ allowed = { "append" }, path_lock = { "/work/kept.rs" } })[1]
    end

    it("adds to the end, keeping what was there, in one write", function()
        files["/work/kept.rs"] = "alpha\nbravo\n"
        writes = {}
        local answer = append_spec().handler({ path = "/work/kept.rs", content = "charlie\n" })

        expect(answer.ok).to.be(true)
        expect(#writes).to.be(1)
        expect(writes[1].path).to.be("/work/kept.rs")
        expect(writes[1].content).to.be("alpha\nbravo\ncharlie\n")
    end)

    it("refuses a file that is not there, and says to create it with write first", function()
        files["/work/kept.rs"] = nil
        writes = {}
        local answer = append_spec().handler({ path = "/work/kept.rs", content = "x" })

        expect(answer.ok).to.be(false)
        expect(answer.reason).to.be("path_missing")
        expect(answer.path).to.be("/work/kept.rs")
        expect(answer.error:find("fs_write", 1, true) ~= nil).to.be(true)
        -- Nothing was written: the op that creates a file is `write`, and a
        -- refusal that quietly created one would be that op under this name.
        expect(#writes).to.be(0)
        files["/work/kept.rs"] = "alpha\nbravo\n"
    end)

    it("holds to the path lock, before the bridge is touched at all", function()
        reads, writes = {}, {}
        local answer = append_spec().handler({ path = "/etc/passwd", content = "x" })

        expect(answer.ok).to.be(false)
        expect(answer.reason).to.be("path_not_allowed")
        expect(#reads).to.be(0)
        expect(#writes).to.be(0)
    end)

    it("declares both its arguments required, so a call cut short is caught", function()
        local schema = append_spec().input_schema
        expect(schema.required).to.equal({ "path", "content" })
    end)
end)

describe("fs_tools.register_tools", function()
    it("registers the same specs and answers their names", function()
        registered = {}
        local names = fs_tools.register_tools({ allowed = { "read", "rollback" }, prefix = "reg_" })

        expect(names).to.equal({ "reg_read", "reg_rollback" })
        expect(#registered).to.be(2)
        expect(registered[1].name).to.be("reg_read")
        expect(type(registered[1].meta.description)).to.be("string")
        expect(type(registered[1].meta.input_schema)).to.be("table")
        expect(type(registered[1].handler)).to.be("function")
    end)

    it("refuses the same missing `allowed`, since it is the same contract", function()
        expect(refusal(function()
            fs_tools.register_tools({})
        end):find("`allowed` is required", 1, true) ~= nil).to.be(true)
    end)
end)
