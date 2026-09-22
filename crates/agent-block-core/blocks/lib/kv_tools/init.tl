--- kv_tools — the library behind `std.kv.register_tools`.
---
--- `src/bridge/kv.rs` registers the Rust half of the `std.kv` bridge and then
--- `require`s this module by name, installing every function it exports onto
--- `std.kv` (`bridge::load_tools_module`). This module writes nothing onto
--- `std` itself: it is a library, and the wiring is the bridge's.
---
--- The tier order every other module follows applies here too: a project's
--- `.agent-block/lib/kv_tools/` wins, this source is the fallback.
--- `agent-block vendor kv_tools` therefore changes the tool surface a model
--- is handed with no install in between, and a vendored copy delegates the
--- way any other module does — `local base = require("embedded.kv_tools")`,
--- wrap what it answers, `return M`.
---
--- Exports:
---   register_tools(opts?) -> array of registered tool names
---   shapes.register_tools_opts — the opts contract, as data
---
--- opts (all optional):
---   allowed : array of op names   (default: {"get","set","delete","list"})
---   prefix  : tool name prefix    (default: "kv_")
---   ns_lock : string              (locks the ns arg; LLM cannot choose namespace)
---
--- Each tool this registers is one `knl.shapes.tool_spec` — `name`,
--- `description`, `input_schema`, `handler` — but the kernel is deliberately
--- NOT required here: a tool module has to load on a VM that has none (the
--- bridge's fallback path runs this source directly on a bare `Lua::new()`).
--- The field set is named by doc, and checked where both halves exist:
--- `crates/agent-block/tests/e2e_embedded_blocks.rs`.
---
--- Written in Teal and embedded through `include_tl!` (`host.rs` and
--- `bridge/kv.rs`); the Lua the host holds is what Teal generates from this
--- file, without these comments. The globals it reads are declared through
--- `host_types`, and `lshape` through `lshape.d.tl` beside the vendored copy.

--- `lshape`, the tolerant way, for the one VM that has no `require` at all.
---
--- `bridge::load_tools_module` falls back to running this source directly
--- when the name resolves nowhere, which is a Rust unit test's bare
--- `Lua::new()`. Nothing resolves there — not `lshape`, not anything — and
--- that VM only needs `std.kv` to hold its functions; it never calls one. So
--- an absent `lshape` costs the dev-mode reading of `opts` and nothing else:
--- every bound that matters in prod is checked beside it in plain Lua.
--- `knl/init.lua` requires `knl_types` on the same terms and for the same
--- reason. (`host_types`, the one `require` a Teal module cannot do without,
--- is answered on that VM by the bridge itself — `bridge/mod.rs`.)
require("host_types")

local shape_ok, lshape = pcall(require, "lshape")
local T = shape_ok and lshape.t or nil
local shape = shape_ok and lshape.check or nil

local record M
    --- What `register_tools` takes.
    record Opts
        allowed: {string}
        prefix: string
        ns_lock: string
    end
    --- What the tools take, all four in one shape: `ns` unless locked,
    --- `key` for get / set / delete, `value` for set, `prefix` for list.
    record Input
        ns: string
        key: string
        value: any
        prefix: string
    end
    --- One tool, before it is named. Built whole in `defs`, one per op:
    --- every field is set there, and the check holds each literal to it.
    record Def ---@struct
        description: string
        input_schema: {string:any}
        handler: function(Input): {string:any}
    end
    --- The opts contract, as data. Built whole, once.
    record Shapes ---@struct
        register_tools_opts: lshape.Schema
    end
    shapes: Shapes
end

--- What `register_tools` is configured with. CLOSED on purpose: an option
--- this module does not know is a typo, and a typo that quietly became a
--- no-op is a tool surface nobody chose — it looks exactly like the caller
--- naming the ops and the prefix it meant.
local REGISTER_TOOLS_OPTS: lshape.Schema
if shape_ok then
    REGISTER_TOOLS_OPTS = T.shape({
        allowed = T.array_of(T.string)
        :describe('the ops to register: "get", "set", "delete", "list"; default all four')
        :is_optional(),
        prefix = T.string:describe('prepended to every tool name; default "kv_"'):is_optional(),
        ns_lock = T.string
        :describe("the one namespace every tool acts on; the model is then not asked for `ns` at all")
        :is_optional(),
    }, { open = false })
end

M.shapes = { register_tools_opts = REGISTER_TOOLS_OPTS }

--- Register the LLM-facing kv tools into the global tool registry.
---
--- Returns: array of registered tool names.
function M.register_tools(opts: M.Opts): {string}
    opts = opts or {}
    if shape ~= nil then
        shape.assert_dev(opts, REGISTER_TOOLS_OPTS, "std.kv.register_tools opts")
    end
    local allowed = opts.allowed or { "get", "set", "delete", "list" }
    local prefix = opts.prefix or "kv_"
    local ns_lock = opts.ns_lock

    local function build_schema(extra_props: {string:any}, extra_required: {string}): {string:any}
        local props: {string:any} = {}
        local required: {string} = {}
        if not ns_lock then
            props.ns = { type = "string", description = "Namespace (logical group)" }
            table.insert(required, "ns")
        end
        for k, v in pairs(extra_props) do
            props[k] = v
        end
        for _, r in ipairs(extra_required) do
            table.insert(required, r)
        end
        local schema: {string:any} = { type = "object", properties = props }
        -- Empty Lua tables serialize as JSON objects, but Anthropic expects
        -- `required` to be a JSON array. Omit the field when empty.
        if #required > 0 then
            schema.required = required
        end
        return schema
    end

    local defs: {string:M.Def} = {
        get = {
            description = "Get a value from the agent's local key-value store (JSON-file backed, persists across runs, agent-private). Returns { value = ... } (nil if the key is missing).",
            input_schema = build_schema({ key = { type = "string" } }, { "key" }),
            handler = function(input: M.Input): {string:any}
                local ns = ns_lock or input.ns
                return { value = std.kv.get(ns, input.key) }
            end,
        },
        set = {
            description = "Store a value in the agent's local key-value store (JSON-file backed, persists across runs, agent-private). Overwrites any existing value at the same key.",
            input_schema = build_schema({
                key = { type = "string" },
                value = { description = "Value to store (string / number / bool / table)" },
            }, { "key", "value" }),
            handler = function(input: M.Input): {string:any}
                local ns = ns_lock or input.ns
                std.kv.set(ns, input.key, input.value)
                return { ok = true }
            end,
        },
        delete = {
            description = "Delete a key from the agent's local key-value store. Returns { deleted = bool } (false if the key did not exist).",
            input_schema = build_schema({ key = { type = "string" } }, { "key" }),
            handler = function(input: M.Input): {string:any}
                local ns = ns_lock or input.ns
                return { deleted = std.kv.delete(ns, input.key) }
            end,
        },
        list = {
            description = "List keys in a namespace of the agent's local key-value store, optionally filtered by prefix. Returns { keys = [...] }.",
            input_schema = build_schema(
                { prefix = { type = "string", description = "Optional key prefix filter" } },
                {}
            ),
            handler = function(input: M.Input): {string:any}
                local ns = ns_lock or input.ns
                return { keys = std.kv.list(ns, input.prefix) }
            end,
        },
    }

    local registered: {string} = {}
    for _, op in ipairs(allowed) do
        local d = defs[op]
        if d then
            local name = prefix .. op
            tool.register(name, { description = d.description, input_schema = d.input_schema }, d.handler)
            table.insert(registered, name)
        end
    end
    return registered
end

return M
