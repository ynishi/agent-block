-- blocks/agent/init.lua — the SDK's agent: MCP wiring, and a loop over `knl`.
--
-- Usage:
--   local agent = require("agent")
--   local result = agent.run({
--       prompt = "...",
--       system = "...",
--       model = "claude-sonnet-4-20250514",
--       max_tokens = 4096,
--       timeout = 120,
--       max_iterations = 20,
--       max_tokens_budget = nil,
--       mcp_servers = { { name = "outline", command = "outline-mcp", args = {} } },
--       on_turn = function(turn_info) end,
--       extra_tools = {},
--       provider = "anthropic",  -- "anthropic" | "openai"
--       base_url = "http://localhost:8080/v1",
--       api_key = "sk-...",
--       api_key_env = "MY_OPENAI_KEY",
--       context_management = true,
--       tool_choice = "required",
--       thinking    = { effort = "medium" },
--       max_retries = 2,
--   })
--
-- result: { ok, content, usage, num_turns, error, messages }
--
-- What this module is
--   A CONSUMER of the kernel. `knl` provides one beat — a model call plus the
--   tools that call asked for — and states that a loop is written on the spot
--   rather than provided; this module writes that loop, and everything else it
--   does is the part that makes it an *agent* rather than a loop: connecting
--   MCP servers, assembling one tool set out of the Lua registry, those servers
--   and the caller's own tools, and taking the run's answer apart into the
--   result its callers have always read.
--
--   The shape is the one every knl consumer takes (see `examples/knl_beat.lua`):
--
--       local d = knl.device{ llm = adapter.<provider>:open(conf), tools = ... }
--       knl.session({ budget = { amount = max_iterations } }, function(s)
--           s:append{ kind = "msg_user", data = { content = prompt } }
--           while true do
--               local out = knl.beat(s, d)
--               ... Outcome.match ...
--           end
--       end)
--
--   The vocabulary below is the kernel's — session, device, beat, Outcome,
--   budget. "Turn" survives in one place only: the keys of the `on_turn`
--   payload, which callers read.
--
-- What it deliberately does not do
--   * No dump. `AGENT_BLOCK_LLM_DUMP` and the `ab.llm` request / response /
--     summary lines are gone. Every call is already a durable fact — the
--     session log holds `llm_request` with the request as sent, `llm_response`
--     with content, usage and stop_reason, and `llm_call_failed` with the
--     failure's classification — so a second, lossier copy on stdout was a
--     transcription of the record rather than a reading of it. HTTP status,
--     latency and the prompt-cache counters are not recorded anywhere; a caller
--     who needs them wraps the `llm` closure itself.
--   * No implicit context. There is no module-level stack of the caller's
--     provider / model / key for a child tool to find: nothing is injected
--     behind a caller's back, and a device is passed rather than discovered.
--   * No second budget tally. The session's grant is the iteration cap and
--     the kernel deducts it; the token spend is a separate reading
--     (`knl.views.usage`) that this loop takes after each beat, and
--     `max_tokens_budget` is this loop stopping on that reading. The kernel
--     never folds usage back into the quota.
--   * No first-wins tool merge. Two sources claiming one name is a wiring
--     bug, and `knl_adapter.tools` says so loudly.
--
-- Temporary: the compile_loop shims
--   `M._llm_ctx_top` and `M._log_meta` are kept for `blocks/tools/compile_loop`,
--   which still runs its own loop and resolves its own correlation ids from the
--   environment. Both go when compile_loop moves onto knl — under the kernel
--   the ids are the beat id and the session id, minted per beat and stamped on
--   every event.
--
-- Notes:
--   - All MCP/HTTP bridge calls are async (coroutine yield). agent.run() must be
--     called inside isle.coroutine_eval() or equivalent async context.
--   - tool.call() is sync. tool.schema() is sync.
--   - NEVER throws. All errors returned as { ok=false, error=... }.
--   - on_turn payload keys: turn_number, content, tool_calls, usage.

local M = {}

--- The kernel. Named `kernel` because the bare `knl` is the syscall bridge
--- global in a host VM and shadowing it here would read as the wrong one.
local kernel = require("knl")
local Outcome = kernel.Outcome

--- The Ports a device is built from: the `llm` closure and the tools map.
local adapter = require("knl_adapter")

local proto = require("llm_proto")

--- Re-exported through `M._test_helpers` only.
local proto_openai = proto.adapter("openai")

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

--- The provider Ports, by the name `opts.provider` uses. The Port is chosen
--- here and nowhere else: a caller who picked `openai` is not holding
--- anthropic's conf by accident, because the adapter it reaches drops what its
--- provider does not accept.
local PORTS = {
    anthropic = adapter.anthropic,
    openai = adapter.openai,
}

--- The four ab.obs correlation fields.
---
--- Closed: this stopped being an internal detail when compile_loop began
--- resolving its own obs ids through `_log_meta`. Every field is optional
--- because an unset environment is the ordinary case, but an unexpected key
--- means the two components have drifted apart on what the fields are — which
--- is the failure the convention exists to prevent, and it would otherwise show
--- up as a run that cannot be selected rather than as an error.
---
--- Checked only in dev mode (LSHAPE_CHECK=1).
local LOG_META = T.shape({
    trace_id = T.string:is_optional(),
    run_id = T.string:is_optional(),
    agent_id = T.string:is_optional(),
    agent_name = T.string:is_optional(),
}, { open = false })

--- Token accounting. `thinking_tokens` is optional because a caller may hold a
--- result produced before the counts were normalized to three.
local USAGE = T.shape({
    input_tokens = T.number,
    output_tokens = T.number,
    total_tokens = T.number,
    thinking_tokens = T.number:is_optional(),
})

--- What `run` returns, as the two things it can be.
---
--- `run` never throws; every failure comes back as `ok = false` with an
--- `error`. Two closed alternatives say that in a way that can be checked:
--- success carries `content` and no `error`, failure carries `error` and no
--- `content`. A single shape with both optional would accept the result that
--- has neither, which is the shape a caller cannot do anything with and the one
--- a half-finished error path produces.
local RUN_OK = T.shape({
    ok = T.literal(true),
    content = T.string,
    usage = USAGE,
    num_turns = T.number,
    messages = T.array_of(T.table),
}, { open = false })

local RUN_ERR = T.shape({
    ok = T.literal(false),
    error = T.string,
    usage = USAGE,
    num_turns = T.number,
    messages = T.array_of(T.table),
}, { open = false })

local RUN_RESULT = T.any_of({ RUN_OK, RUN_ERR })

--- What `mcp.call` hands back across the host boundary.
---
--- The bridge builds this table field by field in Rust and nothing on this side
--- described it, so the two languages agreed by coincidence and review. Closed:
--- a field added on the Rust side that this side does not know about is exactly
--- the drift worth catching, and the e2e tests run real servers through this
--- path, so the check has somewhere to fire.
---
--- `ok = false` covers transport, protocol and timeout failures and carries
--- `error`. A tool that ran and reported failure comes back `ok = true` with
--- `is_error = true`, which is why the two are separate fields rather than one.
local MCP_CALL_RESULT = T.shape({
    ok = T.boolean,
    error = T.string:is_optional(),
    content = T.table:is_optional(),
    is_error = T.boolean:is_optional(),
    structured_content = T.any:is_optional(),
}, { open = false })

-- ============================================================
-- Compat shims for compile_loop (this round only)
-- ============================================================

--- M._llm_ctx_top() → nil
---
--- Kept for compile_loop until it moves to knl. There is no stack any more:
--- nothing is injected behind a caller's back, so a child resolves its own
--- provider / model / key from its conf and then the environment, which is
--- what compile_loop already does when this answers nothing.
function M._llm_ctx_top()
    return nil
end

--- Build the four correlation ids from `opts.log_meta` over the environment.
local function build_log_meta(opts)
    local meta = opts and opts.log_meta or {}
    local trace_id = meta.trace_id or std.env.get("AGENT_BLOCK_TRACE_ID")
    if not trace_id then
        trace_id = meta.task_id or std.env.get("AGENT_BLOCK_TASK_ID")
        if trace_id then
            log.warn("agent: log_meta.task_id / AGENT_BLOCK_TASK_ID is deprecated; use trace_id / AGENT_BLOCK_TRACE_ID")
        end
    end
    return shape.assert_dev({
        trace_id = trace_id,
        agent_id = meta.agent_id or std.env.get("AGENT_BLOCK_AGENT_ID") or std.env.agent_id(),
        agent_name = meta.agent_name or std.env.get("AGENT_BLOCK_AGENT_NAME"),
        run_id = meta.run_id or std.env.get("AGENT_BLOCK_RUN_ID"),
    }, LOG_META, "agent log_meta")
end

--- The four ab.obs correlation fields, resolved from `opts.log_meta` and the
--- environment.
---
--- Kept for compile_loop until it moves to knl: that block emits its own
--- ab.obs lines and has to resolve the ids the same way, and a convention where
--- each component reaches for the environment slightly differently is one that
--- cannot be relied on to select a run. This loop reads none of them — under
--- the kernel a run is named by `session:id()` and a call by its beat id.
---
--- @param opts table|nil  May carry `log_meta` with any of the four fields.
--- @return table  { trace_id, run_id, agent_id, agent_name }, any of them nil.
function M._log_meta(opts)
    return build_log_meta(opts)
end

-- ============================================================
-- Reading an answer
-- ============================================================

--- The `tool_use` blocks of a response, in block order. This is what
--- `on_turn` has always been handed as `tool_calls`, and what the loop counts
--- to decide whether the model asked for anything.
local function tool_use_blocks(content)
    local out = {}
    for _, block in ipairs(content or {}) do
        if block.type == "tool_use" then
            out[#out + 1] = block
        end
    end
    return out
end

--- The text blocks of a response, joined — the run's "final answer" string.
--- The kernel keeps blocks because the provider does; a caller who wants one
--- string gets it here.
local function text_of(content)
    local parts = {}
    for _, block in ipairs(content or {}) do
        if block.type == "text" and block.text then
            parts[#parts + 1] = block.text
        end
    end
    return table.concat(parts, "\n")
end

-- ============================================================
-- The tool set: candidates, then one map
-- ============================================================
--
-- A CANDIDATE is what one source offers: the value `knl_adapter.tools` binds
-- (a ToolPort, or a flat `{ name, description?, input_schema?, handler }`
-- spec) plus the group label the filter reads. The group rides beside the
-- binding rather than inside it because a tool declaration has three fields
-- and `group` is not one of them — which is also why nothing has to strip it
-- before the wire any more.

--- Resolve the group label for an MCP tool definition.
-- Priority:
--   1. tool JSON `_meta.group` (string, non-empty) — server-declared group
--   2. fallback: server_name (current behaviour)
--
-- rmcp 1.4.0 serialises Tool.meta as `_meta` (via #[serde(rename = "_meta")]),
-- so the Lua-side JSON key is always `_meta`.
--
-- @param tool_json   table   Raw tool object from mcp.list_tools
-- @param server_name string  MCP server name (fallback)
-- @return string             Resolved group label
local function resolve_mcp_group(tool_json, server_name)
    local meta = tool_json._meta
    if type(meta) == "table" then
        local g = meta.group
        if type(g) == "string" and g ~= "" then
            return g
        end
    end
    return server_name
end

--- The active-group set, or nil when every group passes.
local function group_set_of(active_groups)
    if not active_groups or #active_groups == 0 then
        return nil
    end
    local set = {}
    for _, g in ipairs(active_groups) do
        set[g] = true
    end
    return set
end

--- Whether a candidate's group is one the caller asked for. A tool with no
--- group is in "default", as it always was.
local function in_groups(group, group_set)
    if group_set == nil then
        return true
    end
    return group_set[group or "default"] == true
end

--- Every registered Lua tool, as a candidate. The registry declares a tool
--- and the registry runs it, so the handler is `tool.call` — a raise from it
--- is closed by the kernel as an `ok = false` tool_result, which is how the
--- model gets to see the failure and pick something else.
local function registry_candidates()
    local out = {}
    for _, spec in ipairs(tool.schema()) do
        local name = spec.name
        out[#out + 1] = {
            group = spec.group,
            bind = {
                name = name,
                description = spec.description,
                input_schema = spec.input_schema,
                handler = function(input)
                    return tool.call(name, input)
                end,
            },
        }
    end
    return out
end

--- The caller's `extra_tools`, as candidates.
---
--- Two accepted shapes, because `compile_loop.make()` answers the nested one:
--- `{ name, schema = { description, input_schema }, handler }` is flattened,
--- and a flat `{ name, description, input_schema, handler? }` passes through.
--- A flat entry that spells the schema field `schema` reaches
--- `knl_adapter.tools`' loud error rather than the provider with no schema.
---
--- An entry with no handler is a DECLARATION, and it dispatches through the
--- Lua registry exactly as it did before — including the case where nothing
--- registered it, which the kernel closes as a failed tool_result the model
--- can answer.
local function extra_candidates(extra_tools)
    local out = {}
    for _, t in ipairs(extra_tools or {}) do
        local name = t.name
        local bind
        if t.schema and t.handler then
            bind = {
                name = name,
                description = t.schema.description,
                input_schema = t.schema.input_schema,
                handler = t.handler,
            }
        else
            bind = {
                name = name,
                description = t.description,
                input_schema = t.input_schema,
                schema = t.schema,
                handler = t.handler or function(input)
                    return tool.call(name, input)
                end,
            }
        end
        out[#out + 1] = { group = t.group, bind = bind }
    end
    return out
end

--- Bind the candidates the filter admits into the map a device takes.
---
--- A duplicate name raises out of `knl_adapter.tools`: two sources claiming
--- one name is a wiring bug, not a merge policy. That is the one behaviour
--- change from the first-wins merge this replaced — a silent winner became a
--- loud stop.
---
--- @param candidates table  array of { group?, bind }
--- @param active_groups table|nil  group names to include (nil = all)
--- @return table tools  knl's `config.tools` map (name -> entry)
local function build_tools(candidates, active_groups)
    local group_set = group_set_of(active_groups)
    local binds = {}
    for _, c in ipairs(candidates) do
        if in_groups(c.group, group_set) then
            binds[#binds + 1] = c.bind
        end
    end
    return adapter.tools(binds)
end

-- ============================================================
-- MCP: connect, bind, notify, disconnect
-- ============================================================

--- The meta-tools that let a model reach a server's resources, as candidates.
--- Agent-provided tools bound through `knl_adapter.tools` like any other —
--- they are not put in the global `tool` registry, because a second run in one
--- process would then meet its own first run as a duplicate name.
local function resource_candidates(sn)
    return {
        {
            group = sn,
            bind = {
                name = sn .. "__mcp_list_resources",
                description = "List available resources on MCP server '" .. sn .. "'",
                input_schema = { type = "object", properties = {} },
                handler = function(_input)
                    local r = mcp.list_resources(sn)
                    if not r.ok then
                        return std.json.encode({ error = r.error })
                    end
                    return std.json.encode(r.resources)
                end,
            },
        },
        {
            group = sn,
            bind = {
                name = sn .. "__mcp_read_resource",
                description = "Read a resource by URI from MCP server '" .. sn .. "'",
                input_schema = {
                    type = "object",
                    properties = { uri = { type = "string" } },
                    required = { "uri" },
                },
                handler = function(input)
                    local r = mcp.read_resource(sn, input.uri)
                    if not r.ok then
                        return std.json.encode({ error = r.error })
                    end
                    return std.json.encode(r.contents)
                end,
            },
        },
    }
end

--- The same for a server's prompts.
local function prompt_candidates(sn)
    return {
        {
            group = sn,
            bind = {
                name = sn .. "__mcp_list_prompts",
                description = "List available prompts on MCP server '" .. sn .. "'",
                input_schema = { type = "object", properties = {} },
                handler = function(_input)
                    local r = mcp.list_prompts(sn)
                    if not r.ok then
                        return std.json.encode({ error = r.error })
                    end
                    return std.json.encode(r.prompts)
                end,
            },
        },
        {
            group = sn,
            bind = {
                name = sn .. "__mcp_get_prompt",
                description = "Get a prompt by name from MCP server '" .. sn .. "'",
                input_schema = {
                    type = "object",
                    properties = {
                        name = { type = "string" },
                        args = { type = "object" },
                    },
                    required = { "name" },
                },
                handler = function(input)
                    local r = mcp.get_prompt(sn, input.name, input.args or {})
                    if not r.ok then
                        return std.json.encode({ error = r.error })
                    end
                    return std.json.encode(r.messages)
                end,
            },
        },
    }
end

local function append_all(into, more)
    for _, item in ipairs(more) do
        into[#into + 1] = item
    end
end

--- Register the progress notification handler for one server (no capability
--- gate; all servers). `opts.on_progress` wins over `progress_to_log`.
local function wire_progress(sn, opts)
    if opts.on_progress then
        local user_cb = opts.on_progress
        -- Registered directly on the main Isle (no bytecode dump needed): the
        -- callback runs with upvalues intact because the main Isle is never
        -- crossed; only the event table `ev` is built on the Rust side.
        mcp.on_progress(sn, function(ev)
            local ok, cb_err = pcall(user_cb, ev)
            if not ok then
                log.warn("agent: on_progress callback error: " .. tostring(cb_err))
            end
        end)
    elseif opts.progress_to_log then
        mcp.on_progress(sn, function(ev)
            local msg = "mcp progress: server="
                .. tostring(ev.server)
                .. " token="
                .. tostring(ev.token)
                .. " p="
                .. tostring(ev.progress)
                .. "/"
                .. tostring(ev.total or "")
            if ev.message and ev.message ~= "" then
                msg = msg .. " msg=" .. ev.message
            end
            log.info(msg)
        end)
    end
end

--- Register the log notification handler for one server. `opts.on_log` wins
--- over `log_to_stderr`; both are gated on the logging capability.
local function wire_log(sn, opts)
    if opts.on_log then
        local user_cb = opts.on_log
        mcp.on_log(sn, function(ev)
            local ok, cb_err = pcall(user_cb, ev)
            if not ok then
                log.warn("agent: on_log callback error: " .. tostring(cb_err))
            end
        end)
    else
        mcp.on_log(sn, function(ev)
            local msg = "mcp log: server="
                .. tostring(ev.server)
                .. " logger="
                .. tostring(ev.logger)
                .. " data="
                .. tostring(ev.data)
            if ev.level == "debug" then
                log.debug(msg)
            elseif ev.level == "warning" then
                log.warn(msg)
            elseif ev.level == "error" then
                log.error(msg)
            else
                log.info(msg)
            end
        end)
    end
end

--- The capability-gated opt-ins: resources / prompts meta-tools and the log
--- handler. One `server_info` call serves all three.
---
--- @return table candidates  the meta-tools this server admitted
local function wire_capabilities(sn, opts)
    local candidates = {}
    if not (opts.enable_resources or opts.enable_prompts or opts.on_log or opts.log_to_stderr) then
        return candidates
    end
    local si_result = mcp.server_info(sn)
    if not si_result.ok then
        log.warn("agent: mcp.server_info failed for '" .. sn .. "': " .. tostring(si_result.error))
        return candidates
    end
    local caps = (si_result.server_info and si_result.server_info.capabilities) or {}

    if opts.enable_resources then
        if caps.resources ~= nil then
            append_all(candidates, resource_candidates(sn))
        else
            log.info("agent: server '" .. sn .. "' has no resources capability; skipping register")
        end
    end
    if opts.enable_prompts then
        if caps.prompts ~= nil then
            append_all(candidates, prompt_candidates(sn))
        else
            log.info("agent: server '" .. sn .. "' has no prompts capability; skipping register")
        end
    end
    if opts.on_log or opts.log_to_stderr then
        if caps.logging ~= nil then
            wire_log(sn, opts)
        else
            log.info("agent: server '" .. sn .. "' has no logging capability; on_log/log_to_stderr skipped")
        end
    end
    return candidates
end

--- Every tool a connected server lists, as candidates.
---
--- ONE `tools/list` per server. The group is `_meta.group` with the server name
--- as the fallback, and it is read off the raw entry — which is why the entries
--- are bound here, one `ToolPort.mcp` at a time, rather than through
--- `knl_adapter.mcp_tools`: that binder lists the server itself, so asking it
--- for an allow-list computed from a listing of our own would list twice for
--- one answer. It is the same binder either way (`mcp_tools(server, opts)` is
--- this loop plus the filter), and the `<server>__<tool>` namespace, the
--- camelCase inputSchema and both failure forms stay closed inside the Port.
---
--- A tool outside the active groups is not bound at all. `build_tools` applies
--- the same filter again on the candidates it is handed; the two agree, and the
--- second pass is what keeps one filter rule for every source.
---
--- @return table|nil candidates
--- @return string|nil err
local function mcp_tool_candidates(sn, active_groups)
    local list = mcp.list_tools(sn)
    if not list.ok then
        return nil, "mcp list_tools failed for '" .. sn .. "': " .. tostring(list.error)
    end
    local group_set = group_set_of(active_groups)
    local candidates = {}
    for _, entry in ipairs(list.tools or {}) do
        local group = resolve_mcp_group(entry, sn)
        if in_groups(group, group_set) then
            candidates[#candidates + 1] = { group = group, bind = adapter.ToolPort.mcp(sn, entry) }
        end
    end
    return candidates
end

--- Connect the servers, wire their notifications, and collect their tools.
---
--- @param servers table  Array of { name, command, args? } or { name, url }
--- @param opts table     the run opts (sampling / on_progress / on_log / ...)
--- @return table|nil     candidates
--- @return string|nil    Error string on failure
--- @return table         connected server names (for cleanup on failure)
local function connect_mcp_servers(servers, opts)
    local candidates = {}
    local connected = {}

    for _, srv in ipairs(servers) do
        local name = srv.name

        -- HTTP transport when srv.url is set, otherwise stdio.
        local ok, err
        if srv.url then
            -- Merge server-level trace_context into transport_opts when not already set.
            local transport_opts = {}
            for k, v in pairs(srv.transport_opts or {}) do
                transport_opts[k] = v
            end
            if transport_opts.trace_context == nil then
                transport_opts.trace_context = not not srv.trace_context
            end
            ok, err = pcall(mcp.connect_http, name, srv.url, transport_opts)
        else
            local connect_opts = { trace_context = not not srv.trace_context }
            ok, err = pcall(mcp.connect, name, srv.command, srv.args or {}, connect_opts)
        end
        if not ok then
            return nil, "mcp connect failed for '" .. name .. "': " .. tostring(err), connected
        end
        table.insert(connected, name)

        if opts.sampling then
            local sampling_ok, sampling_err = pcall(mcp.set_sampling_handler, name, opts.sampling)
            if not sampling_ok then
                log.warn("agent: mcp set_sampling_handler failed for '" .. name .. "': " .. tostring(sampling_err))
            end
        end

        local bound, list_err = mcp_tool_candidates(name, opts.tool_groups)
        if list_err then
            return nil, list_err, connected
        end
        append_all(candidates, bound)

        wire_progress(name, opts)
        append_all(candidates, wire_capabilities(name, opts))
    end

    return candidates, nil, connected
end

--- Gracefully disconnect from MCP servers. Logs errors but does not throw.
--- @param server_names table  Array of server name strings
local function disconnect_mcp_servers(server_names)
    for _, name in ipairs(server_names) do
        local ok, err = pcall(mcp.disconnect, name)
        if not ok then
            log.warn("agent: mcp disconnect error for '" .. name .. "': " .. tostring(err))
        end
    end
end

-- ============================================================
-- The port conf
-- ============================================================

-- Anthropic server-side rolling history via clear_tool_uses_20250919.
-- Trigger at 80K input_tokens, keep last 3 tool_uses, clear at least 10K.
local DEFAULT_CONTEXT_MANAGEMENT = {
    edits = {
        {
            type = "clear_tool_uses_20250919",
            trigger = { type = "input_tokens", value = 80000 },
            keep = { type = "tool_uses", value = 3 },
            clear_at_least = { type = "input_tokens", value = 10000 },
        },
    },
}

--- The opts this module consumes itself. Everything else goes to the Port
--- verbatim — a whitelist here would silently strip every knob added to
--- llm_proto upstream, which is the bug this list is inverted to avoid.
--- `system` is not a conf key: it is the device's, composed into the request
--- by `knl.fold` each beat.
local AGENT_OPTS = {
    prompt = true,
    history = true,
    system = true,
    store = true,
    mcp_servers = true,
    extra_tools = true,
    tool_groups = true,
    on_turn = true,
    max_iterations = true,
    max_tokens_budget = true,
    log_meta = true,
    sampling = true,
    on_progress = true,
    progress_to_log = true,
    on_log = true,
    log_to_stderr = true,
    enable_resources = true,
    enable_prompts = true,
    context_management = true,
    context_management_config = true,
}

--- `opts.context_management == false` opts out entirely (no beta header, no
--- body field). Strict equality, so nil (unset) is default-on.
local function resolve_context_management(opts)
    if opts.context_management == false then
        return nil
    end
    return opts.context_management_config or DEFAULT_CONTEXT_MANAGEMENT
end

--- Everything the Port is opened with: the caller's knobs verbatim, the two
--- request defaults this module has always supplied, and the resolved
--- context-management config on the provider that has one.
local function port_conf(opts, provider)
    local conf = {}
    for key, value in pairs(opts) do
        if not AGENT_OPTS[key] then
            conf[key] = value
        end
    end
    conf.max_tokens = opts.max_tokens or 4096
    conf.timeout = opts.timeout or 120
    if provider ~= "openai" then
        conf.context_management = resolve_context_management(opts)
    end
    return conf
end

--- Anthropic-only knobs are warned about rather than dropped in silence.
local function warn_anthropic_only(opts, provider)
    if provider ~= "openai" then
        return
    end
    for _, name in ipairs({ "cache_control", "context_management", "context_management_config" }) do
        if opts[name] ~= nil then
            log.warn("agent: " .. name .. " is anthropic-only; ignored for provider=openai")
        end
    end
end

-- ============================================================
-- Seeding the log
-- ============================================================

--- One prior message, as the events it is made of.
---
--- A `tool_result` block becomes an event of its own rather than riding inside
--- the user message that carried it: `knl.fold` pairs the tool_use ids of an
--- assistant message against the tool_result EVENTS that answered them, and a
--- result that stayed inside a `msg_user` would leave its call unanswered — so
--- the fold's repair would close it a second time and the provider would see
--- two results for one id.
local function seed_message(session, message)
    local content = message.content
    if message.role == "assistant" then
        session:append({ kind = "llm_response", data = { content = content } })
        return
    end
    if type(content) ~= "table" then
        session:append({ kind = "msg_user", data = { content = content } })
        return
    end
    local rest = {}
    for _, block in ipairs(content) do
        if type(block) == "table" and block.type == "tool_result" then
            session:append({
                kind = "tool_result",
                data = {
                    call_id = block.tool_use_id,
                    ok = block.is_error ~= true,
                    result = block.content or "",
                },
            })
        else
            rest[#rest + 1] = block
        end
    end
    if #rest > 0 then
        session:append({ kind = "msg_user", data = { content = rest } })
    end
end

--- The caller's prior thread, then the prompt. History is a durable record
--- here rather than an argument: what `blocks/lib/session` saved is what this
--- lays back down, and the fold reads the two the same way.
local function seed(session, opts)
    for _, message in ipairs(opts.history or {}) do
        seed_message(session, message)
    end
    session:append({ kind = "msg_user", meta = { label = "prompt" }, data = { content = opts.prompt } })
end

-- ============================================================
-- Reading the run
-- ============================================================

local function zero_usage()
    return { input_tokens = 0, output_tokens = 0, total_tokens = 0, thinking_tokens = 0 }
end

--- The token spend so far, read off the log.
---
--- A reading apart from the budget: the grant counts beats and the kernel
--- never folds usage back into it, so this is one SELECT over the counts every
--- `llm_response` already carries.
local function usage_of(session)
    local rows = kernel.views.usage(session)
    local row = (rows and rows[1]) or {}
    local input = tonumber(row.input_tokens) or 0
    local output = tonumber(row.output_tokens) or 0
    return {
        input_tokens = input,
        output_tokens = output,
        total_tokens = input + output,
        thinking_tokens = tonumber(row.thinking_tokens) or 0,
    }
end

--- A failed beat as one sentence: the stage, the classification the port or
--- the kernel put on it, and the message.
local function error_text(o)
    local detail = o.detail
    if type(detail) ~= "table" then
        return tostring(o.kind) .. ": " .. tostring(detail)
    end
    local message = tostring(detail.message or "unknown failure")
    if detail.kind ~= nil then
        return tostring(o.kind) .. ": " .. tostring(detail.kind) .. ": " .. message
    end
    return tostring(o.kind) .. ": " .. message
end

--- A refusal names its class. `reason` is the adapter's provider-neutral
--- classification ("model" / "content_filter"); the provider's own message,
--- when it sent one, is on the normalized refusal.
local function refusal_text(o)
    local text = "model refused to respond (kind=" .. tostring(o.reason) .. ")"
    local detail = o.detail
    local said = type(detail) == "table" and type(detail.refusal) == "table" and detail.refusal.detail or nil
    if type(said) == "string" and said ~= "" then
        text = text .. ": " .. said
    end
    return text
end

--- The caller's per-beat hook. A broken callback must not take the run down,
--- and a callback that answers `false` stops it.
local function fire_on_turn(on_turn, turn_number, answer, calls)
    if not on_turn then
        return nil
    end
    local ok, verdict = pcall(on_turn, {
        turn_number = turn_number,
        content = answer.content,
        tool_calls = calls,
        usage = answer.usage,
    })
    if not ok then
        log.warn("agent: on_turn callback error: " .. tostring(verdict))
        return nil
    end
    return verdict
end

-- ============================================================
-- Public: agent.run(opts)
-- ============================================================

--- Run the loop: one beat, then decide, until something ends it.
---
--- @return table  the RUN_OK / RUN_ERR result
local function run_loop(opts, provider, candidates, max_iter)
    local device = kernel.device({
        llm = PORTS[provider]:open(port_conf(opts, provider)),
        tools = build_tools(candidates, opts.tool_groups),
        system = opts.system,
    })
    local limit = opts.max_tokens_budget

    return kernel.session({
        owner = "agent",
        -- One unit per beat (the device's default cost), so the grant IS the
        -- iteration cap: the beat after the last one the caller allowed comes
        -- back `stopped` with nothing called and nothing recorded.
        budget = { amount = max_iter, tag = "beats", desc = "one unit per beat" },
        store = opts.store,
    }, function(s)
        seed(s, opts)

        local turns, content, failure = 0, "", nil
        local usage = zero_usage()

        while true do
            local going = Outcome.match(kernel.beat(s, device), {
                stopped = function(o)
                    -- Not a failure and not the model's doing: the quota would
                    -- not cover another beat. The only grant here is the
                    -- iteration cap, so that is what it says.
                    if o.reason == "budget" then
                        failure = "max_iterations (" .. max_iter .. ") reached"
                    else
                        failure = "stopped: " .. tostring(o.reason)
                    end
                    return false
                end,
                error = function(o)
                    turns = turns + 1
                    usage = usage_of(s)
                    failure = error_text(o)
                    return false
                end,
                refused = function(o)
                    turns = turns + 1
                    usage = usage_of(s)
                    failure = refusal_text(o)
                    return false
                end,
                ok = function(o)
                    turns = turns + 1
                    usage = usage_of(s)
                    local answer = o.out
                    content = text_of(answer.content)
                    local calls = tool_use_blocks(answer.content)
                    if fire_on_turn(opts.on_turn, turns, answer, calls) == false then
                        return false
                    end
                    -- Settled: the answer asked for no tool, so there is
                    -- nothing left for another beat to carry forward. A
                    -- `pause_turn` is the server pausing its own tool loop —
                    -- the turn is unfinished, so the loop goes on.
                    if #calls == 0 and answer.stop_reason ~= "pause_turn" then
                        return false
                    end
                    if limit ~= nil and usage.total_tokens >= limit then
                        failure = "token budget exceeded (" .. usage.total_tokens .. "/" .. limit .. ")"
                        return false
                    end
                    return true
                end,
            })
            if not going then
                break
            end
        end

        -- The thread, rebuilt from the log by the same fold the requests were
        -- built with, so what a caller saves and hands back as `history` is
        -- what this run actually sent.
        local messages = kernel.fold(s:events(), device).messages
        if failure ~= nil then
            return { ok = false, error = failure, usage = usage, num_turns = turns, messages = messages }
        end
        return { ok = true, content = content, usage = usage, num_turns = turns, messages = messages }
    end)
end

--- Run a ReAct agent loop.
---
--- @param opts table  {
---   prompt          (required) Initial user prompt string
---   system          (optional) System prompt string
---   model           (optional) LLM model identifier
---   max_tokens      (optional) Per-request token limit (default: 4096)
---   timeout         (optional) HTTP timeout in seconds (default: 120)
---   max_iterations  (optional) Max beats (default: 20). The session's grant.
---   max_tokens_budget (optional) Total token budget across all beats, read
---                   off the log after each one (default: nil = unlimited)
---   store           (optional) Where the session log goes. Omitted is the
---                   host's own database; "mem" and { sqlite = <path> } are
---                   the other two (see knl's header).
---   mcp_servers     (optional) Array of { name, command, args? } / { name, url }
---   on_turn         (optional) Callback function(turn_info). turn_info has
---                   keys: turn_number, content, tool_calls, usage.
---                   Returning false stops the run.
---   log_meta        (optional) External metadata, kept for the sibling block
---                   that still emits ab.obs lines. This loop reads none of it:
---                   a run is named by its session id and a call by its beat id.
---   history         (optional) Prior messages array (e.g. from session.load).
---                   Laid down as the events they are made of before the new
---                   prompt, so the fold sees the full thread.
---   extra_tools     (optional) Extra tool definitions to include
---   tool_groups     (optional) Group names to include (nil = every tool)
---   provider        (optional) "anthropic" (default) | "openai"
---   base_url        (optional) Base URL override for OpenAI-compatible endpoints
---   api_key         (optional) Per-call API key override
---   api_key_env     (optional) Custom env var name for the API key
---   context_management        (optional, default true) false opts out of
---                   Anthropic server-side context editing entirely.
---                   Anthropic-only: warn+ignored when provider="openai".
---   context_management_config (optional) Full override table.
---   ...             every other key is forwarded to the provider Port
---                   verbatim (tool_choice / thinking / temperature / dialect /
---                   extra_body / betas / max_retries / ... — llm_proto's
---                   vocabulary, not reinterpreted here).
--- }
---
--- @return table  {
---   ok         boolean
---   content    string  (final text response)
---   usage      { input_tokens, output_tokens, total_tokens, thinking_tokens }
---   num_turns  number  (beats that reached the provider)
---   error      string  (when ok=false)
---   messages   table   (the thread, folded back out of the log)
--- }
---
--- The contract is checked on the way out in dev mode (LSHAPE_CHECK=1); see
--- `M.shapes.run_result`. Wrapped rather than asserted at each `return` because
--- there are five of them and a sixth added later would be the one that skips
--- the check.
function M.run(opts)
    return shape.assert_dev(M._run_impl(opts), RUN_RESULT, "agent.run result")
end

--- A failure that happened before, or instead of, a run.
local function failed(err)
    return { ok = false, error = err, usage = zero_usage(), num_turns = 0, messages = {} }
end

function M._run_impl(opts)
    opts = opts or {}

    if not opts.prompt or opts.prompt == "" then
        return failed("prompt is required")
    end
    if opts.history ~= nil and type(opts.history) ~= "table" then
        return failed("history must be a table (messages array)")
    end

    local provider = opts.provider or "anthropic"
    if PORTS[provider] == nil then
        return failed("unknown provider '" .. tostring(provider) .. "' (anthropic | openai)")
    end
    warn_anthropic_only(opts, provider)

    local candidates = registry_candidates()
    local connected = {}
    if opts.mcp_servers and #opts.mcp_servers > 0 then
        local bound, err, partial = connect_mcp_servers(opts.mcp_servers, opts)
        if err then
            -- Disconnect any servers that did connect before the failure.
            disconnect_mcp_servers(partial)
            return failed(err)
        end
        connected = partial
        append_all(candidates, bound)
    end
    append_all(candidates, extra_candidates(opts.extra_tools))

    -- The loop raises for exactly the things a caller got wrong at wiring time
    -- — a duplicate tool name, a spec with no schema field it knows — and for
    -- a store that would not take the log. `run` never throws, so those come
    -- back as the failure they are.
    local ran, result = pcall(run_loop, opts, provider, candidates, opts.max_iterations or 20)

    -- Always disconnect, regardless of outcome.
    disconnect_mcp_servers(connected)

    if not ran then
        return failed(tostring(result))
    end
    return result
end

-- ============================================================
-- Internals exposed for the specs
-- ============================================================

M._build_tools = build_tools -- internal: for tests only
M._registry_candidates = registry_candidates -- internal: for tests only
M._extra_candidates = extra_candidates -- internal: for tests only
M._resolve_mcp_group = resolve_mcp_group -- internal: for tests only

--- The contracts this module holds itself to, as data.
---
--- Public so a sibling block consuming `_log_meta`, or a fixture checking what
--- the `mcp.call` bridge produced, can read the same schema rather than a doc
--- comment.
M.shapes = {
    log_meta = LOG_META,
    usage = USAGE,
    run_result = RUN_RESULT,
    mcp_call_result = MCP_CALL_RESULT,
}

--- Pure internal helpers, for unit tests only. No side effects beyond the
--- std/log globals they read.
function M._test_helpers()
    return {
        map_finish_reason = proto_openai.map_finish_reason,
        normalize_openai_response = proto_openai.parse,
        convert_messages_to_openai = proto_openai.convert_messages,
        tool_use_blocks = tool_use_blocks,
        text_of = text_of,
        build_tools = build_tools,
        registry_candidates = registry_candidates,
        extra_candidates = extra_candidates,
        resolve_mcp_group = resolve_mcp_group,
    }
end

return M
