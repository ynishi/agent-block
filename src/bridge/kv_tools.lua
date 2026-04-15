-- Embedded Lua source loaded by src/bridge/kv.rs.
-- Defines std.kv.register_tools(opts?) — LLM-facing tool registration helper.
--
-- opts (all optional):
--   allowed : array of op names   (default: {"get","set","delete","list"})
--   prefix  : tool name prefix    (default: "kv_")
--   ns_lock : string              (locks the ns arg; LLM cannot choose namespace)
--
-- Returns: array of registered tool names.

std.kv.register_tools = function(opts)
    opts = opts or {}
    local allowed = opts.allowed or { "get", "set", "delete", "list" }
    local prefix = opts.prefix or "kv_"
    local ns_lock = opts.ns_lock

    local function build_schema(extra_props, extra_required)
        local props = {}
        local required = {}
        if not ns_lock then
            props.ns = { type = "string", description = "Namespace (logical group)" }
            table.insert(required, "ns")
        end
        for k, v in pairs(extra_props) do props[k] = v end
        for _, r in ipairs(extra_required) do table.insert(required, r) end
        local schema = { type = "object", properties = props }
        -- Empty Lua tables serialize as JSON objects, but Anthropic expects
        -- `required` to be a JSON array. Omit the field when empty.
        if #required > 0 then schema.required = required end
        return schema
    end

    local defs = {
        get = {
            description = "Fetch a value by namespace/key. Returns { value = ... } (value may be nil if missing).",
            input_schema = build_schema({ key = { type = "string" } }, { "key" }),
            handler = function(input)
                local ns = ns_lock or input.ns
                return { value = std.kv.get(ns, input.key) }
            end,
        },
        set = {
            description = "Store a value under namespace/key.",
            input_schema = build_schema({
                key   = { type = "string" },
                value = { description = "Value to store (string / number / bool / table)" },
            }, { "key", "value" }),
            handler = function(input)
                local ns = ns_lock or input.ns
                std.kv.set(ns, input.key, input.value)
                return { ok = true }
            end,
        },
        delete = {
            description = "Delete key. Returns { deleted = bool }.",
            input_schema = build_schema({ key = { type = "string" } }, { "key" }),
            handler = function(input)
                local ns = ns_lock or input.ns
                return { deleted = std.kv.delete(ns, input.key) }
            end,
        },
        list = {
            description = "List keys in the namespace, optionally filtered by prefix.",
            input_schema = build_schema(
                { prefix = { type = "string", description = "Optional key prefix filter" } },
                {}
            ),
            handler = function(input)
                local ns = ns_lock or input.ns
                return { keys = std.kv.list(ns, input.prefix) }
            end,
        },
    }

    local registered = {}
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
