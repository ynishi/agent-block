-- api_spec.lua — the machine check that supervisor's public interface is fully
-- declared, mirroring policy/spec/api_spec.lua and knl/spec/api_spec.lua.
--
-- Run via:
--   test_launch(code_file=".../supervisor/spec/api_spec.lua",
--               search_paths=[".../blocks/lib"])  -- so require("supervisor") resolves
--
-- Why this file exists
--   Every public interface here is declared as an lshape and published through
--   `supervisor.shapes`. A rule like that decays the moment it depends on
--   someone remembering it: an export is added, the registry is not, and the
--   contract quietly stops being the contract. So the completeness is checked
--   rather than remembered — this spec walks the module itself and fails on
--     * an export with no `supervisor.shapes.api` entry,
--     * an entry with no export,
--     * a declared argument carrying no shape (a hole in the dev-mode gate),
--     * a member — a function a caller supplies or an export hands back — with
--       no argument list.
--
--   And the registry is not only complete, it is EXECUTED: in dev mode the
--   module installs a wrapper that holds each declared call to its entry. Both
--   halves of that gate are proven here — a wrong-typed call fails through the
--   registry in dev and reaches the call's own answer in prod, where no
--   wrapper is installed at all — and so is the line between the gate and
--   `only`, which policy learned the hard way [実測: 2026-09-05].
--
-- The fake bridge comes in with `support`, because the shapes here are built on
-- the kernel's and loading knl is what publishes them.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("supervisor.spec.support")
local supervisor = require("supervisor")
local api = supervisor.shapes.api
local check = require("lshape.check")

-- ─────────────────────────────────────────────────────────────────────────────
-- Helpers (the same judgements policy/spec/api_spec.lua makes)
-- ─────────────────────────────────────────────────────────────────────────────

--- A schema is plain data with a `kind` (lshape's Schema-as-Data contract).
local function is_shape(v)
    return type(v) == "table" and rawget(v, "kind") ~= nil
end

--- What a `returns` slot may hold: a shape or a description.
local function is_declaration(v)
    return is_shape(v) or type(v) == "string"
end

--- An `args` declaration under the executed form: an ordered list, one item per
--- positional argument, each carrying the shape it is held to and a word for
--- it. Empty is a declaration too — "this export takes nothing".
local function is_arg_list(v)
    if type(v) ~= "table" or is_shape(v) then
        return false
    end
    for _, item in ipairs(v) do
        if type(item) ~= "table" or not is_shape(item.shape) or type(item.desc) ~= "string" then
            return false
        end
    end
    return true
end

--- The public names of a table: everything not marked internal with `_`.
local function public_names(t)
    local names = {}
    for name in pairs(t) do
        if type(name) == "string" and name:sub(1, 1) ~= "_" then
            names[#names + 1] = name
        end
    end
    table.sort(names)
    return names
end

--- Report as a sorted, comma-joined string, so a failure names what is missing
--- instead of only saying that something is.
local function listed(names)
    table.sort(names)
    return table.concat(names, ",")
end

--- Load a second `supervisor` with the dev-mode gate pinned to `on`.
---
--- The wrapper is installed once, at load, so the two modes are two module
--- instances rather than two calls. The file-level module is put straight back
--- into `package.loaded` afterwards.
local function load_supervisor(on)
    local saved = check.is_dev_mode
    check.is_dev_mode = function()
        return on
    end
    package.loaded["supervisor"] = nil
    local loaded, mod = pcall(require, "supervisor")
    check.is_dev_mode = saved
    package.loaded["supervisor"] = supervisor
    if not loaded then
        error(mod, 0)
    end
    return mod
end

--- Run `fn` with the dev-mode gate pinned, and hand back what it returned.
local function with_dev_mode(on, fn)
    local saved = check.is_dev_mode
    check.is_dev_mode = function()
        return on
    end
    local results = table.pack(pcall(fn))
    check.is_dev_mode = saved
    if not results[1] then
        error(results[2], 0)
    end
    return table.unpack(results, 2, results.n)
end

local function parent()
    return support.session({ budget = { amount = 10, tag = "beats" } })
end

-- ─────────────────────────────────────────────────────────────────────────────

describe("supervisor.shapes.api — every export is declared", function()
    it("declares every public export of the module", function()
        local undeclared = {}
        for _, name in ipairs(public_names(supervisor)) do
            if api[name] == nil then
                undeclared[#undeclared + 1] = name
            end
        end
        expect(listed(undeclared)).to.be("")
    end)

    it("declares nothing the module does not export", function()
        local stale = {}
        for _, name in ipairs(public_names(api)) do
            if supervisor[name] == nil then
                stale[#stale + 1] = name
            end
        end
        expect(listed(stale)).to.be("")
    end)

    it("gives every entry an executable args list and a returns declaration", function()
        local bad = {}
        for _, name in ipairs(public_names(api)) do
            local entry = api[name]
            if type(entry) ~= "table" or not is_arg_list(entry.args) or not is_declaration(entry.returns) then
                bad[#bad + 1] = name
            end
        end
        expect(listed(bad)).to.be("")
    end)

    it("carries a shape on every declared argument, members included", function()
        local bad = {}
        for _, name in ipairs(public_names(api)) do
            local entry = api[name]
            for i, item in ipairs(entry.args or {}) do
                if type(item) ~= "table" or not is_shape(item.shape) then
                    bad[#bad + 1] = name .. "[" .. i .. "]"
                end
            end
            for member, member_entry in pairs(entry.members or {}) do
                if not is_arg_list(member_entry.args) or not is_declaration(member_entry.returns) then
                    bad[#bad + 1] = name .. "." .. tostring(member)
                end
            end
        end
        expect(listed(bad)).to.be("")
    end)

    it("covers the three the design names, and the registry itself", function()
        for _, name in ipairs({ "child", "parallel", "merge", "shapes" }) do
            expect(supervisor[name]).to.exist()
            expect(api[name]).to.exist()
        end
    end)

    it("declares the function each export deals in", function()
        -- Half of `child`'s and `parallel`'s interface is the body the caller
        -- supplies; half of `merge`'s is the fold it hands back. Neither is an
        -- export, and both are called.
        for name, member in pairs({ child = "fn", parallel = "fn", merge = "fold" }) do
            expect(api[name].members).to.exist()
            expect(api[name].members[member]).to.exist()
        end
    end)
end)

describe("supervisor.shapes — the contracts are published as data", function()
    it("publishes a shape for every opts table and for the answer", function()
        for _, name in ipairs({
            "child_opts",
            "child_budget",
            "child_entry",
            "parallel_opts",
            "merge_opts",
            "joiner",
            "result_slot",
            "results",
        }) do
            expect(is_shape(supervisor.shapes[name])).to.be(true)
        end
    end)

    it("closes every opts shape (a typo must not become a no-op)", function()
        local left_open = {}
        for _, name in ipairs({ "child_opts", "child_budget", "child_entry", "parallel_opts", "merge_opts" }) do
            if rawget(supervisor.shapes[name], "open") ~= false then
                left_open[#left_open + 1] = name
            end
        end
        expect(listed(left_open)).to.be("")
    end)

    it("describes an allocation the way the kernel moves one", function()
        expect(check.check({ amount = 4 }, supervisor.shapes.child_budget)).to.be(true)
        expect(check.check({ amount = 4, tag = "steps" }, supervisor.shapes.child_budget)).to.be(true)
        expect(check.check({ amount = 4.5 }, supervisor.shapes.child_budget)).to.be(false)
        expect(check.check({ amount = 0 }, supervisor.shapes.child_budget)).to.be(false)
        -- A grant's vocabulary is not an allocation's: `desc` names why an
        -- owner allowed, and a move between two ledgers has no why to name.
        expect(check.check({ amount = 4, desc = "why" }, supervisor.shapes.child_budget)).to.be(false)
        -- And the kernel's own word for the same field is not this one's: the
        -- translation happens at the syscall, in one place.
        expect(check.check({ from_parent = 4 }, supervisor.shapes.child_budget)).to.be(false)
    end)
end)

describe("supervisor.shapes.api — the registry is executed (dev mode)", function()
    local DEV = load_supervisor(true)

    local function called(fn, ...)
        local args = table.pack(...)
        return with_dev_mode(true, function()
            return pcall(fn, table.unpack(args, 1, args.n))
        end)
    end

    local function mentions(err, text)
        return tostring(err):find(text, 1, true) ~= nil
    end

    it("holds supervisor.child's opts to the registry, naming the argument", function()
        local ok, err = called(DEV.child, parent(), 42, function() end)
        expect(ok).to.be(false)
        expect(mentions(err, "supervisor.child arg 2")).to.be(true)
    end)

    it("holds supervisor.merge's session list to the registry", function()
        local ok, err = called(DEV.merge, parent(), { 42 })
        expect(ok).to.be(false)
        expect(mentions(err, "supervisor.merge arg 2")).to.be(true)
    end)

    it("holds supervisor.parallel's entries to the registry", function()
        local ok, err =
            called(DEV.parallel, parent(), { { opts = { budget = { amount = "four" } }, fn = function() end } })
        expect(ok).to.be(false)
        expect(mentions(err, "supervisor.parallel arg 2")).to.be(true)
    end)

    it("lets a well-formed call through untouched", function()
        -- The gate is a gate: what conforms is not slowed, altered or rejected.
        local p = parent()
        local value = with_dev_mode(true, function()
            return DEV.child(p, { budget = { amount = 2 } }, function(child)
                return child:remaining()
            end)
        end)
        expect(value).to.be(2)
    end)

    it("passes an absent optional argument to the call's own answer", function()
        -- Which arguments are required stays the function's own: `parallel`
        -- takes no opts and must reach its own defaulting, which in this VM
        -- means the missing nursery rather than a shape violation.
        local ok, err = called(DEV.parallel, parent(), { { opts = { budget = { amount = 1 } }, fn = function() end } })
        expect(ok).to.be(false)
        expect(mentions(err, "std.task")).to.be(true)
    end)
end)

describe("supervisor.shapes.api — the gate does not answer what `only` owns", function()
    -- The same split policy makes: `only` owns "is this key declared at all",
    -- in both modes and in the message a caller reads; the registry owns "do
    -- the declared keys have the right shape", and is welcome to be dev-only.
    -- Handed the CLOSED opts shape the gate would report an unknown option
    -- itself, in dev only, in different words — which is a module with two
    -- behaviours for one typo, split by an environment variable.
    local DEV = load_supervisor(true)
    local PROD = load_supervisor(false)

    local function refusal(mod, mode, fn)
        return with_dev_mode(mode, function()
            local ok, err = pcall(fn, mod)
            expect(ok).to.be(false)
            return tostring(err)
        end)
    end

    --- One unknown option per export, each a plausible typo — including the
    --- nested one, which is the level a contract most easily loses.
    local TYPOS = {
        function(mod)
            mod.child(parent(), { budget = { amount = 1 }, owner = "someone" }, function() end)
        end,
        function(mod)
            mod.child(parent(), { budget = { amount = 1, desc = "why" } }, function() end)
        end,
        function(mod)
            mod.parallel(
                parent(),
                { { opts = { budget = { amount = 1 } }, fn = function() end } },
                { joinr = "isolate" }
            )
        end,
        function(mod)
            mod.merge(parent(), { "sess-000001" }, { limt = 10 })
        end,
    }

    it("names the option, in dev and in prod alike", function()
        for _, case in ipairs(TYPOS) do
            for _, message in ipairs({ refusal(DEV, true, case), refusal(PROD, false, case) }) do
                expect(message:find("unknown option", 1, true) ~= nil).to.be(true)
            end
        end
    end)

    it("still holds a declared key to its shape in dev (the gate kept its half)", function()
        local ok, err = with_dev_mode(true, function()
            return pcall(DEV.child, parent(), { budget = { amount = "four" } }, function() end)
        end)
        expect(ok).to.be(false)
        expect(tostring(err):find("supervisor.child arg 2", 1, true) ~= nil).to.be(true)
    end)
end)

describe("supervisor.shapes.api — the registry is absent in prod", function()
    local PROD = load_supervisor(false)

    it("reaches the call's own construction error, not a shape violation", function()
        local ok, err = pcall(PROD.child, parent(), 42, function() end)
        expect(ok).to.be(false)
        expect(tostring(err):find("opts must be a table", 1, true) ~= nil).to.be(true)
        expect(tostring(err):find("arg 2", 1, true) ~= nil).to.be(false)
    end)

    it("keeps every bound loud with no gate installed at all", function()
        local p = parent()
        expect(function()
            PROD.child(p, { budget = { amount = 0 } }, function() end)
        end).to.fail()
        expect(function()
            PROD.parallel(p, { { opts = { budget = { amount = 1 } }, fn = function() end } }, { joiner = "fail_fast" })
        end).to.fail()
        expect(function()
            PROD.merge(p, {})
        end).to.fail()
        expect(function()
            PROD.merge(p, { p:id() }, { limit = 0 })
        end).to.fail()
    end)
end)
