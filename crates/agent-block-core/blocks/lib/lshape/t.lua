--- lshape.t — DSL combinators and schema internal structure.
---
--- Provenance: extracted from algocline-bundled-packages/alc_shapes.
--- Pure Lua, zero dependencies.
---
--- Schema-as-Data contract (after Malli's data-driven schemas):
---   * Every schema is a plain Lua table whose state is held in
---     `rawget`-readable fields: `kind`, `prim`, `inner`, `elem`,
---     `values`, `fields`, `open`, `doc`, `name`, `key`, `val`,
---     `variants`, `tag`, `items`, `min`, `max`, `min_len`, `max_len`.
---   * Metatables carry combinator sugar only (`:is_optional()`,
---     `:describe(doc)`). Stripping the metatable must not change
---     validation behaviour — this is what makes schemas persistable.
---   * Combinators return new tables; schemas are never mutated
---     in place.
---
--- See README.md §Core concept.

local M = {}

local combinators = {}
local schema_mt = { __index = combinators }

local function is_schema(v)
    return type(v) == "table" and rawget(v, "kind") ~= nil
end

function combinators:is_optional()
    return setmetatable({ kind = "optional", inner = self }, schema_mt)
end

function combinators:describe(doc)
    if type(doc) ~= "string" then
        error("lshape.t: describe expects string doc", 2)
    end
    return setmetatable({ kind = "described", inner = self, doc = doc }, schema_mt)
end

--- Bounds combinators — `:min(n)` / `:max(n)` (numeric value bounds,
--- inclusive) and `:min_len(n)` / `:max_len(n)` (length bounds via `#`,
--- for strings and dense arrays). All four fold into a single
--- `kind="bounded"` wrapper: chaining merges bounds into one node
--- (returning a fresh table — schemas are never mutated in place), so
--- `T.number:min(0):max(150)` persists as
--- `{ kind = "bounded", inner = <number>, min = 0, max = 150 }`.
--- Bound applicability is enforced at check time: min/max on a
--- non-number value and min_len/max_len on a non-string/table value are
--- violations, not silent no-ops.
local BOUND_KEYS = { "min", "max", "min_len", "max_len" }

local function with_bound(self, key, n)
    if type(n) ~= "number" then
        error("lshape.t: " .. key .. " expects number, got " .. type(n), 3)
    end
    if (key == "min_len" or key == "max_len") and (n < 0 or n % 1 ~= 0) then
        error("lshape.t: " .. key .. " expects a non-negative integer", 3)
    end
    local out
    if rawget(self, "kind") == "bounded" then
        out = { kind = "bounded", inner = rawget(self, "inner") }
        for i = 1, #BOUND_KEYS do
            out[BOUND_KEYS[i]] = rawget(self, BOUND_KEYS[i])
        end
    else
        out = { kind = "bounded", inner = self }
    end
    out[key] = n
    if out.min ~= nil and out.max ~= nil and out.min > out.max then
        error(string.format("lshape.t: min (%s) must be <= max (%s)", tostring(out.min), tostring(out.max)), 3)
    end
    if out.min_len ~= nil and out.max_len ~= nil and out.min_len > out.max_len then
        error(string.format("lshape.t: min_len (%d) must be <= max_len (%d)", out.min_len, out.max_len), 3)
    end
    return setmetatable(out, schema_mt)
end

function combinators:min(n)
    return with_bound(self, "min", n)
end

function combinators:max(n)
    return with_bound(self, "max", n)
end

function combinators:min_len(n)
    return with_bound(self, "min_len", n)
end

function combinators:max_len(n)
    return with_bound(self, "max_len", n)
end

M.string = setmetatable({ kind = "prim", prim = "string" }, schema_mt)
M.number = setmetatable({ kind = "prim", prim = "number" }, schema_mt)
M.boolean = setmetatable({ kind = "prim", prim = "boolean" }, schema_mt)
M.table = setmetatable({ kind = "prim", prim = "table" }, schema_mt)
M.fn = setmetatable({ kind = "prim", prim = "function" }, schema_mt)
M.any = setmetatable({ kind = "any" }, schema_mt)

--- T.integer — number with no fractional part. Portable across Lua
--- 5.1-5.4 / LuaJIT: the check is `v % 1 == 0` (not `math.type`), so
--- `2.0` passes while `2.5`, NaN and ±inf fail (`inf % 1` and `nan % 1`
--- are both NaN).
M.integer = setmetatable({ kind = "integer" }, schema_mt)

--- T.shape(fields, opts) — named key set.
---
--- Layer contract (generic, applies beyond ALC):
---   Runtime (pass-through) shapes:   open = true (default)
---   Entity (boundary contract) shapes: open = false (explicit)
---
--- The default is deliberately open to preserve pass-through culture:
--- consumers chain outputs through a shared envelope and may attach
--- trace / metrics / debug keys without invalidating downstream
--- consumers. Entity-layer shapes opt into strict mode because their
--- fields are the boundary contract itself, and extra keys signal
--- drift.
---
--- (ALC-context example: Runtime = `ctx.result` in bundled strategy
--- pkgs; Entity = shapes declared in `tools/docs/entity_schemas.lua`.
--- Neither is part of the lshape core contract; both are instances of
--- the generic Runtime-vs-Entity distinction above.)
function M.shape(fields, opts)
    if type(fields) ~= "table" then
        error("lshape.t: shape expects fields table as first argument", 2)
    end
    -- C3: shallow-copy the fields table so a caller who later mutates
    -- the passed table does not silently mutate this schema. Schema-as-Data
    -- doctrine treats schemas as immutable plain data; capturing the caller's
    -- live reference contradicts that invariant. `one_of.values` already
    -- does this (see M.one_of below); shape/discriminated did not.
    local copy = {}
    for name, sub in pairs(fields) do
        if type(name) ~= "string" then
            error("lshape.t: shape field name must be string, got " .. type(name), 2)
        end
        if not is_schema(sub) then
            error(string.format("lshape.t: shape field '%s' must be a schema (table with kind)", name), 2)
        end
        copy[name] = sub
    end
    local open
    if opts == nil then
        open = true
    else
        if type(opts) ~= "table" then
            error("lshape.t: shape expects opts table as second argument", 2)
        end
        if opts.open == nil then
            open = true
        else
            open = opts.open and true or false
        end
    end
    return setmetatable({ kind = "shape", fields = copy, open = open }, schema_mt)
end

--- T.partial(fields, opts) — shape with every field made optional.
---
--- Sugar: expands to `T.shape(...)` where each field is wrapped in
--- `:is_optional()` at construction time. No new `kind` — downstream
--- plumbing (check / reflect / luacats / persist) is unchanged.
---
--- D3: idempotent on already-optional fields — if a field is already
--- `kind="optional"`, it is passed through verbatim (no double-wrap).
--- D4: `opts` default mirrors `T.shape` (open=true).
function M.partial(fields, opts)
    if type(fields) ~= "table" then
        error("lshape.t: partial expects fields table as first argument", 2)
    end
    local wrapped = {}
    for name, sub in pairs(fields) do
        if type(name) ~= "string" then
            error("lshape.t: partial field name must be string, got " .. type(name), 2)
        end
        if not is_schema(sub) then
            error(string.format("lshape.t: partial field '%s' must be a schema (table with kind)", name), 2)
        end
        if rawget(sub, "kind") == "optional" then
            wrapped[name] = sub
        else
            wrapped[name] = sub:is_optional()
        end
    end
    return M.shape(wrapped, opts)
end

function M.array_of(elem)
    if not is_schema(elem) then
        error("lshape.t: array_of expects a schema as argument", 2)
    end
    -- C1 guard: Lua's `#` operator is unspecified on arrays with holes,
    -- so a runtime validator cannot reliably distinguish `{1, nil, 2}`
    -- from `{1}` when iterating `1..#value`. Admitting `nil` at the
    -- element position would make `check` silently under-validate such
    -- arrays while LuaCATS generates `(T|nil)[]` — a doc/runtime gap.
    -- `described` wrappers are transparent (doc-only), so peel them
    -- before the check: `array_of(T:describe(...):is_optional())` and
    -- `array_of(T:is_optional():describe(...))` are both rejected.
    local probe = elem
    while rawget(probe, "kind") == "described" do
        probe = rawget(probe, "inner")
    end
    if rawget(probe, "kind") == "optional" then
        error(
            "lshape.t: array_of(optional(T)) is not allowed — "
                .. "Lua's `#` cannot reliably validate arrays with nil holes. "
                .. "Use array_of(T) (require dense) or model the nil-admission "
                .. "at the enclosing field (e.g. T.array_of(T):is_optional()).",
            2
        )
    end
    return setmetatable({ kind = "array_of", elem = elem }, schema_mt)
end

function M.one_of(values)
    if type(values) ~= "table" then
        error("lshape.t: one_of expects a values table as argument", 2)
    end
    local n = 0
    for _ in pairs(values) do
        n = n + 1
    end
    if n == 0 then
        error("lshape.t: one_of expects at least one value", 2)
    end
    for i = 1, n do
        local v = values[i]
        if v == nil then
            error("lshape.t: one_of expects a 1-based dense array of values", 2)
        end
        local t = type(v)
        if t ~= "string" and t ~= "number" and t ~= "boolean" then
            error(string.format("lshape.t: one_of values must be string/number/boolean, got %s at index %d", t, i), 2)
        end
    end
    -- C5: reject duplicate literals. `T.one_of({"a", "a"})` is almost
    -- certainly a typo (copy-paste / merge glitch), and duplicate values
    -- produce a redundant expected-list in error messages. Lua has no
    -- native set; use a small hash keyed by `type..value` so string "1"
    -- and number 1 are distinguished.
    local seen = {}
    local copy = {}
    for i = 1, n do
        local v = values[i]
        local key = type(v) .. ":" .. tostring(v)
        if seen[key] then
            error(
                string.format(
                    "lshape.t: one_of has duplicate value %s at index %d",
                    (type(v) == "string") and string.format("%q", v) or tostring(v),
                    i
                ),
                2
            )
        end
        seen[key] = true
        copy[i] = v
    end
    return setmetatable({ kind = "one_of", values = copy }, schema_mt)
end

--- T.literal(v) — single-value alias over T.one_of({v}).
--- Schema is identical to `T.one_of({v})` so all downstream consumers
--- (check / reflect / luacats / persist) work unchanged.
function M.literal(value)
    local t = type(value)
    if t ~= "string" and t ~= "number" and t ~= "boolean" then
        error(string.format("lshape.t: literal expects string/number/boolean, got %s", t), 2)
    end
    return M.one_of({ value })
end

function M.discriminated(tag, variants)
    if type(tag) ~= "string" or tag == "" then
        error("lshape.t: discriminated expects non-empty string tag", 2)
    end
    if type(variants) ~= "table" then
        error("lshape.t: discriminated expects variants table", 2)
    end
    -- C3: shallow-copy for the same reason as M.shape (immutability of
    -- constructed schemas).
    -- C4: enforce that each variant declares the discriminant tag as
    -- one of its own fields. The validator (handlers.discriminated)
    -- dispatches by variant key but then re-validates the variant shape
    -- itself, which only catches the tag value mismatch if the variant
    -- shape constrains it. In practice every production variant uses
    -- `name = T.one_of({"X"})` as belt-and-suspenders; DSL-formalize that
    -- convention so typos ("forgot to add the tag field") fail loud at
    -- construction time rather than silently pass through.
    local copy = {}
    local count = 0
    for k, v in pairs(variants) do
        if type(k) ~= "string" then
            error("lshape.t: discriminated variant key must be string, got " .. type(k), 2)
        end
        if not is_schema(v) or rawget(v, "kind") ~= "shape" then
            error(string.format("lshape.t: discriminated variant '%s' must be a shape schema", k), 2)
        end
        if rawget(rawget(v, "fields"), tag) == nil then
            error(string.format("lshape.t: discriminated variant '%s' must declare the tag field '%s'", k, tag), 2)
        end
        copy[k] = v
        count = count + 1
    end
    if count == 0 then
        error("lshape.t: discriminated expects at least one variant", 2)
    end
    return setmetatable({ kind = "discriminated", tag = tag, variants = copy }, schema_mt)
end

--- T.pattern(pat) — Lua-pattern-constrained string.
--- Leaf schema. D1: reject empty pattern at construction (loud-fail).
function M.pattern(pat)
    if type(pat) ~= "string" then
        error("lshape.t: pattern expects string, got " .. type(pat), 2)
    end
    if pat == "" then
        error("lshape.t: pattern must not be empty (use T.string for any string)", 2)
    end
    return setmetatable({ kind = "pattern", pattern = pat }, schema_mt)
end

function M.ref(name)
    if type(name) ~= "string" or name == "" then
        error("lshape.t: ref expects non-empty string name", 2)
    end
    return setmetatable({ kind = "ref", name = name }, schema_mt)
end

--- T.any_of(variants) — untagged union. First-match wins at validation
--- time. D5: ≥2 variants required (single variant is redundant; use the
--- variant directly). D7: distinct from `one_of` — `one_of` holds
--- primitive literals, `any_of` holds nested schemas.
function M.any_of(variants)
    if type(variants) ~= "table" then
        error("lshape.t: any_of expects a variants table as argument", 2)
    end
    local n = 0
    for _ in pairs(variants) do
        n = n + 1
    end
    if n < 2 then
        error("lshape.t: any_of expects at least two variants", 2)
    end
    local copy = {}
    for i = 1, n do
        local v = variants[i]
        if v == nil then
            error("lshape.t: any_of expects a 1-based dense array of variants", 2)
        end
        if not is_schema(v) then
            error(string.format("lshape.t: any_of variant at index %d must be a schema", i), 2)
        end
        copy[i] = v
    end
    return setmetatable({ kind = "any_of", variants = copy }, schema_mt)
end

--- T.tuple(items) — fixed-length positional array. Each position has
--- its own schema; the value must be a dense 1-based array of exactly
--- `#items` elements (a `#items + 1`-th element is a violation).
--- Optional elements are rejected at construction for the same C1
--- rationale as `array_of`: a nil hole at position i is
--- indistinguishable from a short tuple. Model nil-admission at the
--- enclosing field instead (e.g. `T.tuple({...}):is_optional()`).
function M.tuple(items)
    if type(items) ~= "table" then
        error("lshape.t: tuple expects an items table as argument", 2)
    end
    local n = 0
    for _ in pairs(items) do
        n = n + 1
    end
    if n == 0 then
        error("lshape.t: tuple expects at least one item", 2)
    end
    local copy = {}
    for i = 1, n do
        local v = items[i]
        if v == nil then
            error("lshape.t: tuple expects a 1-based dense array of schemas", 2)
        end
        if not is_schema(v) then
            error(string.format("lshape.t: tuple item at index %d must be a schema", i), 2)
        end
        local probe = v
        while rawget(probe, "kind") == "described" do
            probe = rawget(probe, "inner")
        end
        if rawget(probe, "kind") == "optional" then
            error(
                "lshape.t: tuple(optional(T)) is not allowed — a nil hole at "
                    .. "a tuple position is indistinguishable from a short tuple. "
                    .. "Model the nil-admission at the enclosing field "
                    .. "(e.g. T.tuple({...}):is_optional()).",
                2
            )
        end
        copy[i] = v
    end
    return setmetatable({ kind = "tuple", items = copy }, schema_mt)
end

function M.map_of(key, val)
    if not is_schema(key) then
        error("lshape.t: map_of expects a schema as key argument", 2)
    end
    if not is_schema(val) then
        error("lshape.t: map_of expects a schema as val argument", 2)
    end
    return setmetatable({ kind = "map_of", key = key, val = val }, schema_mt)
end

M._internal = {
    schema_mt = schema_mt,
    combinators = combinators,
    is_schema = is_schema,
}

return M
