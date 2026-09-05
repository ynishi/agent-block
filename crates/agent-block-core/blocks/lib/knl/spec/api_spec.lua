-- api_spec.lua — the machine check that knl's public interface is fully
-- declared (the Lua half; the bridge holds the other).
--
-- Run via:
--   test_launch(code_file=".../knl/spec/api_spec.lua",
--               search_paths=[".../blocks/lib"])   -- so require("knl") resolves
--
-- Why this file exists
--   Every public interface of knl is defined as a shape and published
--   through `knl.shapes`. A rule like that decays the moment it depends on
--   someone remembering it: an export is added, the registry is not, and the
--   contract quietly stops being the contract. So the completeness is
--   checked rather than remembered — this spec walks the module itself and
--   fails on
--     * an export with no `knl.shapes.api` entry (the registry fell behind),
--     * an entry with no export (the registry kept a name that is gone),
--     * an `Outcome` function missing from / stale in that entry's members,
--     * a device field `knl.shapes.device_config` does not describe.
--
--   It reads the module, never a list written next to it, so there is no
--   third place to keep in step.
--
--   And the registry is not only complete, it is EXECUTED: every `args`
--   entry is an ordered list of `{ shape, desc }`, and in dev mode knl
--   installs a wrapper that holds each declared call to its entry. So this
--   file also proves the two halves of that gate — a wrong-typed call fails
--   through the registry in dev, and reaches the function's own answer in
--   prod, where the wrapper is not installed at all.
--
-- TODO (the other half — a later pass): the same check across the
-- bridge. `knl.api()` answers the bridge's own SESSION_API / MODULE_API
-- tables, and a test that has the bridge in its VM will hold every name it
-- declares against `knl.shapes.session` / `knl.shapes.module`, so adding a
-- syscall on one side and not the other goes red. It cannot live here (this
-- file runs in a VM with no bridge on purpose), and nothing here depends on
-- it landing.
--
-- No fake `knl` bridge is installed on purpose: none of this touches the
-- syscall layer, and a module that cannot be loaded and read without one
-- would be a finding of its own.

local describe, it, expect = lust.describe, lust.it, lust.expect

local K = require("knl")
local api = K.shapes.api
local check = require("lshape.check")

-- ─────────────────────────────────────────────────────────────────────────────
-- Helpers
-- ─────────────────────────────────────────────────────────────────────────────

--- A schema is plain data with a `kind` (lshape's Schema-as-Data contract).
local function is_shape(v)
    return type(v) == "table" and rawget(v, "kind") ~= nil
end

--- What an `args` / `returns` slot may hold: a shape, a description, or an
--- ordered list of either (for the arguments a shape cannot express — a
--- session handle, a callback).
local function is_declaration(v)
    if is_shape(v) or type(v) == "string" then
        return true
    end
    if type(v) ~= "table" then
        return false
    end
    if #v == 0 then
        return false
    end
    for _, item in ipairs(v) do
        if not (is_shape(item) or type(item) == "string") then
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

--- An `args` declaration under the executed form: an ordered list, one item
--- per positional argument, each carrying the shape the argument is held to
--- and a word for it. Empty is a declaration too — "this export takes
--- nothing", or "this export is not a function".
---
--- This is the half a machine can run, which is why it is stricter than
--- `is_declaration`: a free-text argument would be a hole in the gate.
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

--- Report as a sorted, comma-joined string: a failure then names what is
--- missing instead of only saying that something is.
local function listed(names)
    table.sort(names)
    return table.concat(names, ",")
end

--- Load a second `knl` with the dev-mode gate pinned to `on`.
---
--- The wrapper is installed once, at load, so the two modes are two module
--- instances rather than two calls. The file-level `K` is put straight back
--- into `package.loaded` afterwards: the copy is a local subject, and
--- nothing else in this file (or any file after it) sees it.
local function load_knl(on)
    local saved = check.is_dev_mode
    check.is_dev_mode = function()
        return on
    end
    package.loaded["knl"] = nil
    local loaded, mod = pcall(require, "knl")
    check.is_dev_mode = saved
    package.loaded["knl"] = K
    if not loaded then
        error(mod, 0)
    end
    return mod
end

--- Run `fn` with the dev-mode gate pinned, and hand back what it returned.
---
--- The wrapper installed at load still asks `assert_dev` at call time, so a
--- dev-loaded module judges nothing unless the mode is on when it is called.
--- Pinning both is what makes a case about the gate rather than about the
--- environment this file happens to run in (bare test_launch: off; the
--- lua-spec runner: LSHAPE_CHECK=1).
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

describe("knl.shapes.api — every export is declared", function()
    it("declares every public export of the module", function()
        local undeclared = {}
        for _, name in ipairs(public_names(K)) do
            if api[name] == nil then
                undeclared[#undeclared + 1] = name
            end
        end
        expect(listed(undeclared)).to.be("")
    end)

    it("declares nothing the module does not export", function()
        local stale = {}
        for _, name in ipairs(public_names(api)) do
            if K[name] == nil then
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
        -- declared in prose is a hole in it. This is the walk that says
        -- there are none — across the top-level entries and the members
        -- (`Outcome.*`, `device:with`) alike.
        local bad = {}
        for _, name in ipairs(public_names(api)) do
            local entry = api[name]
            for i, item in ipairs(entry.args or {}) do
                if type(item) ~= "table" or not is_shape(item.shape) then
                    bad[#bad + 1] = name .. "[" .. i .. "]"
                end
            end
            for member, member_entry in pairs(entry.members or {}) do
                if not is_arg_list(member_entry.args) then
                    bad[#bad + 1] = name .. "." .. tostring(member)
                end
            end
        end
        expect(listed(bad)).to.be("")
    end)

    it("covers the exports the design names one by one", function()
        -- The walk above would still pass if the module lost an export and
        -- the registry lost it too. These are the names the module doc
        -- declares, plus the query views it ships.
        for _, name in ipairs({
            "open",
            "resume",
            "session",
            "device",
            "beat",
            "fold",
            "new_beat_id",
            "Outcome",
            "views",
            "shapes",
        }) do
            expect(K[name]).to.exist()
            expect(api[name]).to.exist()
        end
    end)
end)

describe("knl.shapes.api.Outcome — the namespace's own members", function()
    local members = api.Outcome.members

    it("declares every function Outcome exports", function()
        local undeclared = {}
        for _, name in ipairs(public_names(K.Outcome)) do
            if members[name] == nil then
                undeclared[#undeclared + 1] = name
            end
        end
        expect(listed(undeclared)).to.be("")
    end)

    it("declares nothing Outcome does not export", function()
        local stale = {}
        for _, name in ipairs(public_names(members)) do
            if K.Outcome[name] == nil then
                stale[#stale + 1] = name
            end
        end
        expect(listed(stale)).to.be("")
    end)

    it("gives every member an executable args list and a returns declaration", function()
        local bad = {}
        for _, name in ipairs(public_names(members)) do
            local entry = members[name]
            if type(entry) ~= "table" or not is_arg_list(entry.args) or not is_declaration(entry.returns) then
                bad[#bad + 1] = name
            end
        end
        expect(listed(bad)).to.be("")
    end)

    it("covers the four constructors, the four predicates and match", function()
        for _, name in ipairs({
            "ok",
            "refused",
            "err",
            "stopped",
            "is_ok",
            "is_refused",
            "is_error",
            "is_stopped",
            "match",
        }) do
            expect(type(K.Outcome[name])).to.be("function")
            expect(members[name]).to.exist()
        end
    end)
end)

describe("knl.shapes.api — the registry is executed (dev mode)", function()
    local DEV = load_knl(true)

    --- Call `fn(...)` through the dev-loaded module with the gate on, and
    --- hand back (ok, err) — the two things every case here is about.
    local function called(fn, ...)
        local args = table.pack(...)
        return with_dev_mode(true, function()
            return pcall(fn, table.unpack(args, 1, args.n))
        end)
    end

    local function mentions(err, text)
        return tostring(err):find(text, 1, true) ~= nil
    end

    it("holds knl.device's config to the registry, naming the argument", function()
        local ok, err = called(DEV.device, 42)
        expect(ok).to.be(false)
        -- the registry, not the function's own type check
        expect(mentions(err, "knl.device arg 1")).to.be(true)
    end)

    it("holds knl.beat's session to the registry, naming the argument", function()
        local d = DEV.device({ llm = function() end })
        local ok, err = called(DEV.beat, "nope", d)
        expect(ok).to.be(false)
        expect(mentions(err, "knl.beat arg 2")).to.be(false)
        expect(mentions(err, "knl.beat arg 1")).to.be(true)
    end)

    it("holds knl.session's callback to the registry, naming the position", function()
        local ok, err = called(DEV.session, {}, "not a function")
        expect(ok).to.be(false)
        expect(mentions(err, "knl.session arg 2")).to.be(true)
    end)

    it("holds an Outcome member too (the namespace's functions are exports)", function()
        local ok, err = called(DEV.Outcome.err, "not_a_stage", "boom")
        expect(ok).to.be(false)
        expect(mentions(err, "knl.Outcome.err arg 1")).to.be(true)
    end)

    it("lets a well-formed call through untouched", function()
        -- The gate is a gate: what conforms is not slowed, altered or
        -- rejected. A false positive here would be worse than no gate.
        local d = with_dev_mode(true, function()
            return DEV.device({ llm = function() end, system = "s" })
        end)
        expect(d.system).to.be("s")
        local req = with_dev_mode(true, function()
            return DEV.fold({ { kind = "msg_user", content = "hi" } }, {})
        end)
        expect(#req.messages).to.be(1)
    end)

    it("passes an absent optional argument (an argument nobody gave)", function()
        -- `tag` is optional on `stopped`, and `device` takes no config at
        -- all: neither is a violation of a declaration about what WAS
        -- passed. Which arguments are required stays the function's own.
        expect(called(DEV.Outcome.stopped, "budget")).to.be(true)
        expect(called(DEV.device)).to.be(true)
        -- and a nil where a table is declared is still the function's
        -- question: beat answers `err("conf")` rather than raising.
        expect(called(DEV.beat, {}, nil)).to.be(true)
    end)
end)

describe("knl.shapes.api — the registry is absent in prod", function()
    local PROD = load_knl(false)

    it("reaches knl.device's own construction error, not a shape violation", function()
        local ok, err = pcall(PROD.device, 42)
        expect(ok).to.be(false)
        expect(tostring(err):find("config must be a table", 1, true) ~= nil).to.be(true)
        expect(tostring(err):find("arg 1", 1, true) ~= nil).to.be(false)
    end)

    it("reaches knl.beat's own gate — an Outcome, not a raise", function()
        local d = PROD.device({ llm = function() end })
        local o = PROD.beat("nope", d)
        expect(o.status).to.be("error")
        expect(o.kind).to.be("conf")
    end)

    it("reaches knl.session's own callback check", function()
        local ok, err = pcall(PROD.session, {}, "not a function")
        expect(ok).to.be(false)
        expect(tostring(err):find("must be a function", 1, true) ~= nil).to.be(true)
        expect(tostring(err):find("arg 2", 1, true) ~= nil).to.be(false)
    end)
end)

describe("knl.shapes.error — the failure vocabulary is one list", function()
    --- Peel the doc-only / optional wrappers off a schema (both carry
    --- `inner`) to reach the one that holds the literals.
    local function literals_of(schema)
        while type(schema) == "table" and rawget(schema, "inner") ~= nil do
            schema = rawget(schema, "inner")
        end
        return rawget(schema, "values")
    end

    it("publishes the shape a raised kernel failure reads back as", function()
        expect(is_shape(K.shapes.error)).to.be(true)
        expect(rawget(K.shapes.error, "kind")).to.be("shape")
    end)

    it("closes `kind` on exactly knl.shapes.error_kinds", function()
        -- One constant, two readers: the shape and anything that
        -- enumerates the classes. A list retyped beside the shape is the
        -- second source of truth the registry exists to rule out — and the third
        -- (the kernel's own `KnlError::KINDS`) is held against this one
        -- where a bridge exists (knl_beat_test.lua, inv10).
        local declared = K.shapes.error_kinds
        expect(type(declared)).to.be("table")
        expect(#declared > 0).to.be(true)

        local in_shape = literals_of(rawget(K.shapes.error, "fields").kind)
        expect(listed({ table.unpack(in_shape) })).to.be(listed({ table.unpack(declared) }))
    end)

    it("names the class the kernel calls retryable", function()
        -- Not a style point: a loop reads `detail.retryable` to decide on
        -- asking again, and the word behind it has to be in the list.
        local declared = {}
        for _, kind in ipairs(K.shapes.error_kinds) do
            declared[kind] = true
        end
        expect(declared.busy).to.be(true)
    end)

    it("names the class a read that ran too long comes back as", function()
        -- The read side has a failure of its own: a query past its
        -- deadline is interrupted, and the
        -- class it raises is in the same closed list as every other.
        local declared = {}
        for _, kind in ipairs(K.shapes.error_kinds) do
            declared[kind] = true
        end
        expect(declared.timeout).to.be(true)
        -- and it validates as a reading, like any other class
        expect(check.check({ kind = "timeout", method = "query", retryable = false, message = "…" }, K.shapes.error)).to.be(
            true
        )
    end)

    it("names the class an allocation the parent cannot pay for comes back as", function()
        -- The one class that reports a decision rather than a fault: a child
        -- asked for more than its parent's balance covered, the refusal is in
        -- the log, and asking again against the same balance answers the
        -- same — so it is not retryable either.
        local declared = {}
        for _, kind in ipairs(K.shapes.error_kinds) do
            declared[kind] = true
        end
        expect(declared.refused).to.be(true)
        expect(check.check({ kind = "refused", method = "open", retryable = false, message = "…" }, K.shapes.error)).to.be(
            true
        )
    end)

    it("declares knl.error in the bridge registry, like every other syscall", function()
        local entry = K.shapes.module.error
        expect(entry).to.exist()
        expect(is_declaration(entry.args)).to.be(true)
        expect(is_declaration(entry.returns)).to.be(true)
        expect(entry.returns).to.be(K.shapes.error)
    end)

    it("gives every declared syscall an args and a returns", function()
        -- The same completeness rule as `shapes.api` above, on the half of
        -- the registry that describes the bridge.
        local bad = {}
        for _, registry in ipairs({ K.shapes.session, K.shapes.module }) do
            -- Every key, not `public_names`: `__close` is a declared part
            -- of the session surface, and the leading underscore there is
            -- Lua's metamethod spelling, not a "private" marker.
            for name, entry in pairs(registry) do
                if type(entry) ~= "table" or not is_declaration(entry.args) or not is_declaration(entry.returns) then
                    bad[#bad + 1] = tostring(name)
                end
            end
        end
        expect(listed(bad)).to.be("")
    end)
end)

describe("knl.views — every predefined query view is declared", function()
    -- The same rule as the module registry, on the read side: a view is a
    -- named function that runs one SELECT, and the name has to be declared
    -- or the contract is only what someone remembered to write down.
    local views = K.shapes.views

    it("declares every view the module exports", function()
        local undeclared = {}
        for _, name in ipairs(public_names(K.views)) do
            if views[name] == nil then
                undeclared[#undeclared + 1] = name
            end
        end
        expect(listed(undeclared)).to.be("")
    end)

    it("declares nothing the module does not export", function()
        local stale = {}
        for _, name in ipairs(public_names(views)) do
            if type(K.views[name]) ~= "function" then
                stale[#stale + 1] = name
            end
        end
        expect(listed(stale)).to.be("")
    end)

    it("gives every view an executable args list and a returns declaration", function()
        local bad = {}
        for _, name in ipairs(public_names(views)) do
            local entry = views[name]
            if type(entry) ~= "table" or not is_arg_list(entry.args) or not is_declaration(entry.returns) then
                bad[#bad + 1] = name
            end
        end
        expect(listed(bad)).to.be("")
    end)

    it("is the same table the api registry walks (one declaration, two names)", function()
        -- `knl.shapes.views` and `knl.shapes.api.views.members` are one
        -- table. Two would be two places to add a view to, and one of them
        -- would eventually be missed.
        expect(K.shapes.api.views.members).to.be(views)
    end)

    it("covers the five the design names", function()
        -- `usage` is one of them: the token counts are a question put to the
        -- log like any other, not a reading the kernel serves itself. So is
        -- `tree`: the shape of a session tree is read back out of the log,
        -- not held anywhere.
        for _, name in ipairs({ "beats", "tool_pairs", "ledger", "usage", "tree" }) do
            expect(type(K.views[name])).to.be("function")
            expect(views[name]).to.exist()
        end
    end)
end)

describe("knl.shapes.schema — the read schema is published as data", function()
    -- The columns a caller writes SQL against. It is checked for being
    -- well formed here, and held against the
    -- kernel's own declaration where a bridge exists (knl_beat_test.lua,
    -- inv11) — the same two-sided arrangement as the syscall registries.
    local schema = K.shapes.schema

    it("names the table and lists its columns", function()
        expect(type(schema)).to.be("table")
        expect(schema.table).to.be("events")
        expect(type(schema.columns)).to.be("table")
        expect(#schema.columns > 0).to.be(true)
    end)

    it("gives every column a name, a type and a pk flag", function()
        local bad = {}
        for i, column in ipairs(schema.columns) do
            if
                type(column) ~= "table"
                or type(column.name) ~= "string"
                or type(column.type) ~= "string"
                or type(column.pk) ~= "boolean"
            then
                bad[#bad + 1] = "column[" .. i .. "]"
            end
        end
        expect(listed(bad)).to.be("")
    end)

    it("declares the primary key the store's rows are ordered by", function()
        local pk = {}
        for _, column in ipairs(schema.columns) do
            if column.pk then
                pk[#pk + 1] = column.name
            end
        end
        expect(listed(pk)).to.be("seq,stream")
    end)

    it("carries the envelope as columns and the structure in one of them", function()
        local names = {}
        for _, column in ipairs(schema.columns) do
            names[column.name] = true
        end
        -- The envelope is the row: `kind` is the
        -- indexed column a kind-filtered view uses, `beat` is the
        -- correlation key `knl.views.beats` groups on without a JSON path,
        -- `meta` holds the shallow labels — and `data` is the one column a
        -- view has to reach into, which is what ties such a view to the
        -- shape of the kind it reads.
        for _, name in ipairs({ "kind", "beat", "meta", "data" }) do
            expect(names[name]).to.be(true)
        end
        -- and the whole-object column they replaced is gone
        expect(names.payload).to.be(nil)
    end)
end)

describe("knl.shapes.events — the `data` shape of every kind this layer writes or reads", function()
    -- The kernel validates the envelope and the `data` of its OWN kinds
    -- (session_* / budget_*) and nothing else, so these are the only
    -- declaration there is of what a beat writes — plus the two boundary
    -- kinds a supervisor reads the session tree out of. Same completeness
    -- rule as the registries above: checked, not remembered.
    local events = K.shapes.events

    -- The vocabulary a beat writes, plus the seed form a caller writes
    -- (`msg_user`).
    local WRITTEN = {
        "msg_user",
        "llm_request",
        "llm_response",
        "llm_call_failed",
        "tool_call",
        "tool_result",
    }

    -- The two kernel kinds declared here anyway, because a supervisor READS
    -- them: they carry the session tree (`parent` on an opening,
    -- `open_children` on a close) and `knl.views.tree` reads those paths.
    -- Nothing in Lua may write either — the kernel refuses a hand-written
    -- boundary — so this is a reader's declaration, not a writer's.
    local READ_ONLY = {
        "session_opened",
        "session_closed",
    }

    it("declares each kind a beat writes, plus the seed a caller writes", function()
        local missing = {}
        for _, kind in ipairs(WRITTEN) do
            if not is_shape(events[kind]) then
                missing[#missing + 1] = kind
            end
        end
        expect(listed(missing)).to.be("")
    end)

    it("declares the two kernel boundary kinds a supervisor reads", function()
        local missing = {}
        for _, kind in ipairs(READ_ONLY) do
            if not is_shape(events[kind]) then
                missing[#missing + 1] = kind
            end
        end
        expect(listed(missing)).to.be("")
    end)

    it("carries the session tree on those two and nowhere else", function()
        -- The two fields a tree is made of, each declared on the kind that
        -- records it. `knl.views.tree` reads exactly these paths, so a
        -- rename here is a view that has to move with it.
        local opened = rawget(events.session_opened, "fields")
        local closed = rawget(events.session_closed, "fields")
        expect(opened.parent ~= nil).to.be(true)
        expect(closed.open_children ~= nil).to.be(true)
        -- Both optional: a root has no parent, and a close with no children
        -- still running says nothing about them.
        expect(check.check({ scope_id = "s", owner = "u" }, events.session_opened)).to.be(true)
        expect(check.check({ reason = "done" }, events.session_closed)).to.be(true)
        expect(check.check({ scope_id = "s", owner = "u", parent = "p" }, events.session_opened)).to.be(true)
        expect(check.check({ reason = "done", open_children = { "a", "b" } }, events.session_closed)).to.be(true)
        expect(check.check({ reason = "done", open_children = "a" }, events.session_closed)).to.be(false)
    end)

    it("declares nothing else (the vocabulary here is what this layer writes or reads)", function()
        local known = {}
        for _, kind in ipairs(WRITTEN) do
            known[kind] = true
        end
        for _, kind in ipairs(READ_ONLY) do
            known[kind] = true
        end
        local extra = {}
        for name in pairs(events) do
            if not known[name] then
                extra[#extra + 1] = tostring(name)
            end
        end
        expect(listed(extra)).to.be("")
    end)

    it("closes every one of them", function()
        -- A key that arrived by accident is a `data` path somebody will
        -- eventually select; a closed shape is what makes it a failure at
        -- the append instead of a column in a view.
        local left_open = {}
        for name, declared in pairs(events) do
            if rawget(declared, "open") ~= false then
                left_open[#left_open + 1] = tostring(name)
            end
        end
        expect(listed(left_open)).to.be("")
    end)

    it("holds the envelope's own rules apart from them", function()
        -- `event_base` is the envelope (kind / beat / meta / data) and
        -- `event_meta` is the shallow-label rule inside it. Neither is a
        -- per-kind contract, and neither moves when a kind's shape does.
        expect(is_shape(K.shapes.event_base)).to.be(true)
        expect(is_shape(K.shapes.event_meta)).to.be(true)
        expect(check.check({ label = "seed", n = 1, on = true }, K.shapes.event_meta)).to.be(true)
        expect(check.check({ label = { deep = 1 } }, K.shapes.event_meta)).to.be(false)
    end)
end)

describe("knl.shapes.open_opts — a session on its own, or one opened from another", function()
    local opts = K.shapes.open_opts

    it("takes an owner's grant", function()
        expect(check.check({ owner = "u", budget = { amount = 100, tag = "tokens" } }, opts)).to.be(true)
        expect(check.check({}, opts)).to.be(true)
    end)

    it("takes an allocation out of a parent's balance", function()
        -- The parent is a session handle, which is userdata over the bridge
        -- and a table in a VM that has none — the same widening every other
        -- session argument is declared with.
        expect(check.check({ owner = "w", parent = {}, budget = { from_parent = 25 } }, opts)).to.be(true)
        expect(check.check({ parent = {}, budget = { from_parent = 25, tag = "turns" } }, opts)).to.be(true)
    end)

    it("closes the allocation on its two fields", function()
        -- `desc` is a grant's; an allocation records the parent it came from,
        -- which is the whole of what the kernel knows about why it happened.
        -- A closed shape is what makes that a failure rather than a field
        -- that quietly does nothing.
        expect(rawget(K.shapes.budget_allocation, "open")).to.be(false)
        expect(check.check({ from_parent = 5, desc = "for the worker" }, K.shapes.budget_allocation)).to.be(false)
        expect(check.check({ amount = 5 }, K.shapes.budget_allocation)).to.be(false)
        expect(check.check({ from_parent = 5 }, K.shapes.budget_allocation)).to.be(true)
    end)

    it("publishes the allocation beside the grant", function()
        -- Two shapes because they are two claims about where a balance came
        -- from: an owner allowing, and a parent handing over. Which of them
        -- a call may make is the kernel's answer, not this shape's.
        expect(is_shape(K.shapes.budget_grant)).to.be(true)
        expect(is_shape(K.shapes.budget_allocation)).to.be(true)
    end)
end)

describe("knl.shapes.query_opts — what a read may ask for beyond the SQL", function()
    it("is a closed shape (an unknown option must not quietly do nothing)", function()
        expect(is_shape(K.shapes.query_opts)).to.be(true)
        expect(rawget(K.shapes.query_opts, "open")).to.be(false)
    end)

    it("accepts a set of sessions, a timeout and a limit, all optional", function()
        expect(check.check({}, K.shapes.query_opts)).to.be(true)
        expect(check.check({ sessions = { "a", "b" }, timeout_ms = 250, limit = 10 }, K.shapes.query_opts)).to.be(true)
        expect(check.check({ sessions = { 1 } }, K.shapes.query_opts)).to.be(false)
        expect(check.check({ nonsense = true }, K.shapes.query_opts)).to.be(false)
    end)

    it("is the shape session:query's third argument is declared with", function()
        expect(K.shapes.session.query).to.exist()
        expect(K.shapes.session.query.args[3]).to.be(K.shapes.query_opts)
    end)
end)

describe("knl.shapes.device_config — every field of a device is described", function()
    -- A device carries exactly what it was built from, resolved. Building
    -- one with every key set is what makes the walk below see them all: a
    -- field left nil is not a field.
    local function full_device()
        return K.device({
            llm = function() end,
            tools = {
                echo = {
                    description = "echo",
                    input_schema = { type = "object" },
                    handler = function(args)
                        return args
                    end,
                },
            },
            tool_policy = function()
                return "run"
            end,
            fold = function()
                return { messages = {} }
            end,
            filters = {
                function(req)
                    return req
                end,
            },
            system = "be terse",
            cost = function()
                return 1
            end,
        })
    end

    it("describes every field a constructed device carries", function()
        local fields = rawget(K.shapes.device_config, "fields")
        local undescribed = {}
        for name in pairs(full_device()) do
            if fields[name] == nil then
                undescribed[#undescribed + 1] = tostring(name)
            end
        end
        expect(listed(undescribed)).to.be("")
    end)

    it("describes nothing a device cannot carry", function()
        local d = full_device()
        local unreachable = {}
        for name in pairs(rawget(K.shapes.device_config, "fields")) do
            if d[name] == nil then
                unreachable[#unreachable + 1] = name
            end
        end
        expect(listed(unreachable)).to.be("")
    end)

    it("is closed, so an undeclared key cannot ride in on a config", function()
        expect(rawget(K.shapes.device_config, "open")).to.be(false)
    end)
end)
