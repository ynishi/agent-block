-- api_spec.lua — the machine check that policy's public interface is fully
-- declared, mirroring knl/spec/api_spec.lua.
--
-- Run via:
--   test_launch(code_file=".../policy/spec/api_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("policy") resolves
--
-- Why this file exists
--   Every public interface here is declared as an lshape and published through
--   `policy.shapes`. A rule like that decays the moment it depends on someone
--   remembering it: a factory is added, the registry is not, and the contract
--   quietly stops being the contract. So the completeness is checked rather
--   than remembered — this spec walks the module itself and fails on
--     * an export with no `policy.shapes.api` entry,
--     * an entry with no export,
--     * a declared argument carrying no shape (a hole in the dev-mode gate),
--     * a member — a function a factory hands BACK — with no argument list.
--
--   It reads the module, never a list written next to it, so there is no third
--   place to keep in step.
--
--   And the registry is not only complete, it is EXECUTED: in dev mode policy
--   installs a wrapper that holds each declared call to its entry. The two
--   halves of that gate are proven here too — a wrong-typed call fails through
--   the registry in dev, and reaches the factory's own answer in prod, where
--   the wrapper is not installed at all.
--
-- The fake bridge comes in with `support` because the shapes here are built on
-- the kernel's (`event_base`, `request`, `outcome`, `error_kinds`) and loading
-- knl is what publishes them.

local describe, it, expect = lust.describe, lust.it, lust.expect

local support = require("policy.spec.support")
local kernel = require("knl")
local policy = require("policy")
local api = policy.shapes.api
local check = require("lshape.check")

-- ─────────────────────────────────────────────────────────────────────────────
-- Helpers (the same judgements knl/spec/api_spec.lua makes)
-- ─────────────────────────────────────────────────────────────────────────────

--- A schema is plain data with a `kind` (lshape's Schema-as-Data contract).
local function is_shape(v)
    return type(v) == "table" and rawget(v, "kind") ~= nil
end

--- What a `returns` slot may hold: a shape or a description.
local function is_declaration(v)
    return is_shape(v) or type(v) == "string"
end

--- An `args` declaration under the executed form: an ordered list, one item
--- per positional argument, each carrying the shape it is held to and a word
--- for it. Empty is a declaration too — "this export takes nothing".
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

--- Load a second `policy` with the dev-mode gate pinned to `on`.
---
--- The wrapper is installed once, at load, so the two modes are two module
--- instances rather than two calls. The file-level `policy` is put straight
--- back into `package.loaded` afterwards.
local function load_policy(on)
    local saved = check.is_dev_mode
    check.is_dev_mode = function()
        return on
    end
    package.loaded["policy"] = nil
    local loaded, mod = pcall(require, "policy")
    check.is_dev_mode = saved
    package.loaded["policy"] = policy
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

-- ─────────────────────────────────────────────────────────────────────────────

describe("policy.shapes.api — every export is declared", function()
    it("declares every public export of the module", function()
        local undeclared = {}
        for _, name in ipairs(public_names(policy)) do
            if api[name] == nil then
                undeclared[#undeclared + 1] = name
            end
        end
        expect(listed(undeclared)).to.be("")
    end)

    it("declares nothing the module does not export", function()
        local stale = {}
        for _, name in ipairs(public_names(api)) do
            if policy[name] == nil then
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
        -- The gate can only hold what a shape describes, so an argument
        -- declared in prose is a hole in it. The members — the functions a
        -- factory hands back — are walked here and nothing wraps them, exactly
        -- as knl declares `device:with`.
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

    it("covers the five the design names, and the registry itself", function()
        for _, name in ipairs({ "window", "carry", "stagnation", "retry", "escalate", "shapes" }) do
            expect(policy[name]).to.exist()
            expect(api[name]).to.exist()
        end
    end)

    it("declares the function every factory hands back", function()
        -- A factory's opts are half of its interface; the value it answers is
        -- the other half, and it is the half a caller actually calls.
        for name, member in pairs({
            window = "fold",
            carry = "bind",
            stagnation = "predicate",
            retry = "predicate",
            escalate = "next",
        }) do
            expect(api[name].members).to.exist()
            expect(api[name].members[member]).to.exist()
        end
    end)
end)

describe("policy.shapes — the contracts are published as data", function()
    it("publishes a shape for every opts table and for the verdict", function()
        for _, name in ipairs({
            "window_opts",
            "carry_opts",
            "stagnation_opts",
            "retry_opts",
            "escalate_opts",
            "beat_record",
            "stop_reason",
        }) do
            expect(is_shape(policy.shapes[name])).to.be(true)
        end
    end)

    it("closes every opts shape (a policy typo must not become a no-op)", function()
        local left_open = {}
        for _, name in ipairs({
            "window_opts",
            "carry_opts",
            "stagnation_opts",
            "retry_opts",
            "escalate_opts",
        }) do
            if rawget(policy.shapes[name], "open") ~= false then
                left_open[#left_open + 1] = name
            end
        end
        expect(listed(left_open)).to.be("")
    end)

    it("names exactly the two verdicts stagnation can answer", function()
        expect(check.check("repeated", policy.shapes.stop_reason)).to.be(true)
        expect(check.check("no_progress", policy.shapes.stop_reason)).to.be(true)
        expect(check.check("stalled", policy.shapes.stop_reason)).to.be(false)
    end)

    it("describes a beat the way this module derives one", function()
        expect(check.check({ id = "b-1", events = {} }, policy.shapes.beat_record)).to.be(true)
        expect(check.check({ id = 1, events = {} }, policy.shapes.beat_record)).to.be(false)
        expect(check.check({ id = "b-1" }, policy.shapes.beat_record)).to.be(false)
        expect(check.check({ id = "b-1", events = {}, extra = 1 }, policy.shapes.beat_record)).to.be(false)
    end)

    it("reads the kernel's two failure vocabularies rather than retyping them", function()
        -- `retry`'s `kinds` closes on the UNION of `knl.shapes.error_kinds` (a
        -- kernel failure) and `knl.shapes.call_error_kinds` (a model call that
        -- did not come off). Both are read from knl, so a class added on
        -- either side is available here the moment it lands — and neither list
        -- is retyped, which is what would let the two drift apart.
        local kinds = rawget(rawget(policy.shapes.retry_opts, "fields").kinds, "inner")
        local elem = rawget(kinds, "elem")
        local values = rawget(elem, "values")

        local union = {}
        for _, list in ipairs({ kernel.shapes.error_kinds, kernel.shapes.call_error_kinds }) do
            for _, kind in ipairs(list) do
                union[#union + 1] = kind
            end
        end
        expect(listed({ table.unpack(values) })).to.be(listed(union))
    end)
end)

describe("policy.shapes.api — the registry is executed (dev mode)", function()
    local DEV = load_policy(true)

    local function called(fn, ...)
        local args = table.pack(...)
        return with_dev_mode(true, function()
            return pcall(fn, table.unpack(args, 1, args.n))
        end)
    end

    local function mentions(err, text)
        return tostring(err):find(text, 1, true) ~= nil
    end

    it("holds policy.window's opts to the registry, naming the argument", function()
        local ok, err = called(DEV.window, 42)
        expect(ok).to.be(false)
        expect(mentions(err, "policy.window arg 1")).to.be(true)
    end)

    it("holds policy.retry's opts to the registry", function()
        local ok, err = called(DEV.retry, { max = 2, kinds = { 7 } })
        expect(ok).to.be(false)
        expect(mentions(err, "policy.retry arg 1")).to.be(true)
    end)

    it("lets a well-formed call through untouched", function()
        -- The gate is a gate: what conforms is not slowed, altered or
        -- rejected. A false positive here would be worse than no gate.
        local fold = with_dev_mode(true, function()
            return DEV.window({ tail = 2 })
        end)
        expect(type(fold)).to.be("function")
    end)

    it("passes an absent optional argument to the factory's own answer", function()
        -- Which arguments are required stays the function's own: `policy.carry`
        -- takes none, and `policy.window` still raises its own bound error
        -- rather than a shape violation about an opts table nobody passed.
        expect(called(DEV.carry)).to.be(true)
        local ok, err = called(DEV.window)
        expect(ok).to.be(false)
        expect(mentions(err, "tail must be a whole number")).to.be(true)
    end)
end)

describe("policy.shapes.api — the gate does not answer what `only` owns", function()
    -- The divergence this closes [実測: 2026-09-05]. The dev-mode gate wraps
    -- the export, so whatever it judges it judges FIRST. Handed the CLOSED
    -- opts shape it became the thing that reported an unknown option — in dev
    -- only, as "shape violation at $.session: unexpected field", instead of
    -- the factory's own "unknown option 'session' (a session is an argument
    -- …)". The module then said two different things about one typo depending
    -- on `LSHAPE_CHECK`, and three refusal cases passed under a harness that
    -- leaves it unset and failed under `just test-lua`, which sets it to 1.
    --
    -- The judgement is split now (`opts_contract`): `only` owns declaredness
    -- in both modes, the registry owns the shape of the declared keys. These
    -- cases pin BOTH modes for every factory, so neither harness is the one
    -- that decides.
    local DEV = load_policy(true)
    local PROD = load_policy(false)

    --- The message an unknown option produces, through `mod` in `mode`.
    local function refusal(mod, mode, name, opts)
        return with_dev_mode(mode, function()
            local ok, err = pcall(mod[name], opts)
            expect(ok).to.be(false)
            return tostring(err)
        end)
    end

    --- One unknown option per factory, each a plausible typo, and a session —
    --- the one wrong guess the binding convention invites.
    local TYPOS = {
        { name = "window", opts = { tail = 2, tial = 3 } },
        { name = "carry", opts = { max_byte = 10 } },
        { name = "stagnation", opts = { smae = 3 } },
        { name = "retry", opts = { maximum = 3 } },
        { name = "escalate", opts = { strong = function() end, whne = function() end } },
    }

    it("names the option, in dev and in prod alike", function()
        for _, case in ipairs(TYPOS) do
            local in_dev = refusal(DEV, true, case.name, case.opts)
            local in_prod = refusal(PROD, false, case.name, case.opts)
            for _, message in ipairs({ in_dev, in_prod }) do
                expect(message:find("unknown option", 1, true) ~= nil).to.be(true)
            end
        end
    end)

    --- The options each factory cannot be built without, so a case about an
    --- UNKNOWN key is about that and not about a missing required one — the
    --- registry still judges those, and would answer first.
    local REQUIRED = {
        window = { tail = 2 },
        carry = {},
        stagnation = {},
        retry = {},
        escalate = { strong = function() end },
    }

    it("says a session is an argument, in dev and in prod alike", function()
        local session = support.session()
        for _, name in ipairs({ "window", "carry", "stagnation", "retry", "escalate" }) do
            local opts = { session = session }
            for k, v in pairs(REQUIRED[name]) do
                opts[k] = v
            end
            local in_dev = refusal(DEV, true, name, opts)
            local in_prod = refusal(PROD, false, name, opts)
            for _, message in ipairs({ in_dev, in_prod }) do
                expect(message:find("unknown option 'session'", 1, true) ~= nil).to.be(true)
                expect(message:find("an argument", 1, true) ~= nil).to.be(true)
            end
        end
    end)

    it("still holds a declared key to its shape in dev (the gate kept its half)", function()
        -- Widening the registry's arg must not have turned the gate off: a
        -- `kinds` the kernel does not publish is a DECLARED key of the wrong
        -- shape, and that is the registry's judgement to make.
        local ok, err = with_dev_mode(true, function()
            return pcall(DEV.retry, { max = 2, kinds = { 7 } })
        end)
        expect(ok).to.be(false)
        expect(tostring(err):find("policy.retry arg 1", 1, true) ~= nil).to.be(true)
    end)
end)

describe("policy.shapes.api — the registry is absent in prod", function()
    local PROD = load_policy(false)

    it("reaches the factory's own construction error, not a shape violation", function()
        local ok, err = pcall(PROD.window, 42)
        expect(ok).to.be(false)
        expect(tostring(err):find("opts must be a table", 1, true) ~= nil).to.be(true)
        expect(tostring(err):find("arg 1", 1, true) ~= nil).to.be(false)
    end)

    it("keeps every bound loud with no gate installed at all", function()
        -- The dev gate is a convenience; the checks a policy must not be built
        -- without are written beside them and are the same in both modes.
        expect(function()
            PROD.window({ tail = 0 })
        end).to.fail()
        expect(function()
            PROD.stagnation({ same = 1 })
        end).to.fail()
        expect(function()
            -- A word from neither vocabulary. `rate_limited` was this case
            -- once and is a declared kind now: the adapter classifies a call
            -- that did not come off, and a retry policy may name what it
            -- produces.
            PROD.retry({ kinds = { "throttled" } })
        end).to.fail()
        expect(function()
            PROD.escalate({})
        end).to.fail()
        expect(function()
            PROD.carry({ max_bytes = 0 })
        end).to.fail()
    end)
end)
