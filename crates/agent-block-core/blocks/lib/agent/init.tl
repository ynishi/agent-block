-- blocks/lib/agent/init.tl — the SDK's agent: MCP wiring, and a loop over `knl`.
--
-- Usage:
--   local agent = require("agent")
--   local result = agent.run({
--       prompt = "...",
--       system = "...",
--       model = "claude-sonnet-4-20250514",
--       max_tokens = nil,       -- absent: the server's own cap (see llm_proto)
--       timeout = 120,          -- required: how long this model's reply may take
--       max_iterations = 20,    -- required: the session's grant
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
--       compact = { at = 0.8, reserve = 4096 },
--                               -- fold the record into a summary once the last request
--                               -- took `at` of the window (policy.compact); needs
--                               -- context_window beside the model, and max_tokens or
--                               -- `reserve` for the reply's room. Off unless given
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
--   The loop's decisions are all in `run_loop`, in the four `Outcome.match`
--   arms and nowhere else: what ends a run (a settled answer, an exhausted
--   grant, a failed or refused beat, a caller's `on_turn` answering false),
--   and what the token budget does when it is crossed.
--
--   A run is three steps, and the file is in their order. `prepare` turns the
--   opts into a `Run` — the checked opts, the connected servers, the device
--   built from the provider Port and the one tools map — and everything above
--   it is what it calls. `run_loop` beats over that `Run`, with the readers it
--   takes each beat apart with just above it. `result_of` reads what the loop
--   ended with into the result. `M.run` is the three in a row, and nothing
--   else: everything above `run_loop` prepares its arguments; everything below
--   reads its result.
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
-- Notes:
--   - All MCP/HTTP bridge calls are async (coroutine yield). agent.run() must be
--     called inside isle.coroutine_eval() or equivalent async context.
--   - tool.call() is sync. tool.schema() is sync.
--   - NEVER throws. All errors returned as { ok=false, error=... }.
--   - on_turn payload keys: turn_number, content, tool_calls, usage.
--
-- Written in Teal and embedded through `include_tl!` (`host.rs`, as one of
-- the two EMBEDDED_BLOCKS); the Lua the host holds is what Teal generates,
-- without these comments. What crosses the module's boundary — the opts, the
-- result, the `on_turn` payload, an MCP server declaration, a tool candidate
-- — is a record here. The kernel is read through its own record, the host
-- globals (`tool`, `mcp`, `std`, `log`) through `host_types`.
local host = require("host_types")

--- The kernel. Named `kernel` because the bare `knl` is the syscall bridge
--- global in a host VM and shadowing it here would read as the wrong one.
local kernel = require("knl")
local Outcome = kernel.Outcome

--- The Ports a device is built from: the `llm` closure and the tools map.
local adapter = require("knl_adapter")
--- The policies a run with `compact` plugs in: the window from the summary
--- on, the compaction, the ledger, the window line.
local policy = require("policy")
local type shared = require("policy.shared")
local type room = require("policy.room")
local type pcompact = require("policy.compact")

local proto = require("llm_proto")
local type openai = require("llm_proto.openai")
local type mcp_tools = require("mcp_tools")

--- Re-exported through `M._test_helpers` only.
local proto_openai = proto.adapter("openai") as openai

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

-- ============================================================
-- What crosses the boundary
-- ============================================================

--- One MCP server to connect: `{ name, command, args? }` over stdio, or
--- `{ name, url }` over HTTP.
local record McpServer
    name: string
    command: string
    args: {string}
    url: string
    transport_opts: {string:any}
    trace_context: boolean
end

--- The nested half of the nested `extra_tools` shape.
local record ToolSchema
    description: string
    input_schema: any
end

--- One entry of `extra_tools`, in either of its two shapes (see
--- `extra_candidates`).
local record ExtraTool
    name: string
    description: string
    input_schema: any
    schema: ToolSchema
    handler: function(any): any
    group: string
end

--- One candidate: the binding, and the group the filter reads beside it.
local record Candidate ---@struct
    group: string
    bind: any
end

--- A flat spec, as this module builds one for `knl_adapter.tools`. `schema`
--- rides along only on the `extra_tools` path that has to reach the binder's
--- loud error (see `extra_candidates`).
local record FlatBind ---@struct
    name: string
    description: string
    input_schema: any
    schema: ToolSchema ---@optional
    handler: function(any): any
end

--- Token accounting, as a result and an `on_turn` payload carry it.
local record Usage ---@struct
    input_tokens: number
    output_tokens: number
    total_tokens: number
    thinking_tokens: number
end

--- What `on_turn` is handed, once per beat that reached the provider.
local record TurnInfo ---@struct
    turn_number: number
    content: {proto.Block}
    tool_calls: {proto.Block}
    usage: kernel.Usage
end

--- What `run` takes. The keys this module reads are named; every other key
--- is forwarded to the provider Port verbatim (see `port_conf`).
local record RunOpts
    prompt: string
    history: {proto.Message}
    system: any
    store: any
    timeout: number
    max_tokens: number
    max_iterations: number
    max_tokens_budget: number
    provider: string
    mcp_servers: {McpServer}
    extra_tools: {ExtraTool}
    tool_groups: {string}
    on_turn: function(TurnInfo): any
    sampling: function(any): any
    on_progress: function(host.Mcp.Notification): any
    progress_to_log: boolean
    on_log: function(host.Mcp.Notification): any
    log_to_stderr: boolean
    enable_resources: boolean
    enable_prompts: boolean
    context_management: any
    context_management_config: {string:any}
    compact: any
end

--- `compact`, as a table: `policy.compact`'s `at` and `min_beats`, and the
--- tokens the window holds back for the reply when `max_tokens` names no
--- cap on the wire.
local record CompactOpt
    at: number
    min_beats: integer
    reserve: number
end

--- What `run` answers: `content` on success, `error` on failure, never both
--- (the check is `M.shapes.run_result`).
local record RunResult
    ok: boolean
    content: string
    error: string
    usage: Usage
    num_turns: number
    messages: {any}
end

--- The provider Port, as far as this module reaches into it: the call it
--- opens, and — for a run that compacts — the two questions the window
--- policies ask it.
local record Port
    open: function(Port, ?{string:any}): function(kernel.Request): kernel.LlmResult, kernel.CallError
    count: function(Port, any, any): number
    profile: function(Port, any): shared.Profile
end

--- The contracts `M.shapes` publishes.
local record Shapes ---@struct
    usage: lshape.Schema
    run_result: lshape.Schema
    mcp_call_result: lshape.Schema
end

--- What `_test_helpers` answers.
local record TestHelpers ---@struct
    map_finish_reason: function(any): string
    normalize_openai_response: function({string:any}): proto.Decoded, string
    convert_messages_to_openai: function({proto.Message}, ?string): {openai.OaiMessage}
    tool_use_blocks: function({proto.Block}): {proto.Block}
    text_of: function({proto.Block}): string
end

local record M
    shapes: Shapes
    run: function(RunOpts): RunResult
    _build_tools: function({Candidate}, {string}): {string:kernel.ToolEntry}
    _registry_candidates: function(): {Candidate}
    _extra_candidates: function({ExtraTool}): {Candidate}
    _resolve_mcp_group: function({string:any}, string): string
    _test_helpers: function(): TestHelpers
end

--- The provider Ports, by the name `opts.provider` uses. The Port is chosen
--- here and nowhere else: a caller who picked `openai` is not holding
--- anthropic's conf by accident, because the adapter it reaches drops what its
--- provider does not accept.
local PORTS: {string:Port} = {
    anthropic = adapter.anthropic as Port,
    openai = adapter.openai as Port,
}

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

local function zero_usage(): Usage
    return { input_tokens = 0, output_tokens = 0, total_tokens = 0, thinking_tokens = 0 }
end

--- A failure that happened before, or instead of, a run.
local function failed(err: string): RunResult
    return { ok = false, error = err, usage = zero_usage(), num_turns = 0, messages = {} }
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
local function resolve_mcp_group(tool_json: {string:any}, server_name: string): string
    local meta = tool_json._meta
    if meta is {string:any} then
        local g = meta.group
        if g is string and g ~= "" then
            return g
        end
    end
    return server_name
end

--- The active-group set, or nil when every group passes.
local function group_set_of(active_groups: {string}): {string:boolean}
    if not active_groups or #active_groups == 0 then
        return nil
    end
    local set: {string:boolean} = {}
    for _, g in ipairs(active_groups) do
        set[g] = true
    end
    return set
end

--- Whether a candidate's group is one the caller asked for. A tool with no
--- group is in "default", as it always was.
local function in_groups(group: string, group_set: {string:boolean}): boolean
    if group_set == nil then
        return true
    end
    return group_set[group or "default"] == true
end

--- Every registered Lua tool, as a candidate. The registry declares a tool
--- and the registry runs it, so the handler is `tool.call` — a raise from it
--- is closed by the kernel as an `ok = false` tool_result, which is how the
--- model gets to see the failure and pick something else.
local function registry_candidates(): {Candidate}
    local out: {Candidate} = {}
    for _, spec in ipairs(tool.schema()) do
        local name = spec.name
        local bind: FlatBind = {
            name = name,
            description = spec.description,
            input_schema = spec.input_schema,
            handler = function(input: any): any
                return tool.call(name, input)
            end,
        }
        out[#out + 1] = { group = spec.group, bind = bind }
    end
    return out
end

--- The caller's `extra_tools`, as candidates.
---
--- Two accepted shapes, because a tool factory that returns a def answers the
--- nested one: `{ name, schema = { description, input_schema }, handler }` is
--- flattened,
--- and a flat `{ name, description, input_schema, handler? }` passes through.
--- A flat entry that spells the schema field `schema` reaches
--- `knl_adapter.tools`' loud error rather than the provider with no schema.
---
--- An entry with no handler is a DECLARATION, and it dispatches through the
--- Lua registry exactly as it did before — including the case where nothing
--- registered it, which the kernel closes as a failed tool_result the model
--- can answer.
local function extra_candidates(extra_tools: {ExtraTool}): {Candidate}
    local out: {Candidate} = {}
    for _, t in ipairs(extra_tools or {}) do
        local name = t.name
        local bind: FlatBind
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
                handler = t.handler or function(input: any): any
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
local function build_tools(candidates: {Candidate}, active_groups: {string}): {string:kernel.ToolEntry}
    local group_set = group_set_of(active_groups)
    local binds: {any} = {}
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

--- One meta-tool a server's capability admits, as a candidate in the
--- server's group.
local function meta_candidate(sn: string, bind: FlatBind): Candidate
    return { group = sn, bind = bind }
end

--- The meta-tools that let a model reach a server's resources, as candidates.
--- Agent-provided tools bound through `knl_adapter.tools` like any other —
--- they are not put in the global `tool` registry, because a second run in one
--- process would then meet its own first run as a duplicate name.
local function resource_candidates(sn: string): {Candidate}
    return {
        meta_candidate(sn, {
            name = sn .. "__mcp_list_resources",
            description = "List available resources on MCP server '" .. sn .. "'",
            input_schema = { type = "object", properties = {} },
            handler = function(_input: any): any
                local r = mcp.list_resources(sn)
                if not r.ok then
                    return std.json.encode({ error = r.error })
                end
                return std.json.encode(r.resources)
            end,
        }),
        meta_candidate(sn, {
            name = sn .. "__mcp_read_resource",
            description = "Read a resource by URI from MCP server '" .. sn .. "'",
            input_schema = {
                type = "object",
                properties = { uri = { type = "string" } },
                required = { "uri" },
            },
            handler = function(input: any): any
                local r = mcp.read_resource(sn, (input as {string:string}).uri)
                if not r.ok then
                    return std.json.encode({ error = r.error })
                end
                return std.json.encode(r.contents)
            end,
        }),
    }
end

--- The same for a server's prompts.
local function prompt_candidates(sn: string): {Candidate}
    return {
        meta_candidate(sn, {
            name = sn .. "__mcp_list_prompts",
            description = "List available prompts on MCP server '" .. sn .. "'",
            input_schema = { type = "object", properties = {} },
            handler = function(_input: any): any
                local r = mcp.list_prompts(sn)
                if not r.ok then
                    return std.json.encode({ error = r.error })
                end
                return std.json.encode(r.prompts)
            end,
        }),
        meta_candidate(sn, {
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
            handler = function(input: any): any
                local args = input as {string:any}
                local r = mcp.get_prompt(sn, args.name as string, (args.args or {}) as {string:any})
                if not r.ok then
                    return std.json.encode({ error = r.error })
                end
                return std.json.encode(r.messages)
            end,
        }),
    }
end

local function append_all(into: {Candidate}, more: {Candidate})
    for _, item in ipairs(more) do
        into[#into + 1] = item
    end
end

--- Register the progress notification handler for one server (no capability
--- gate; all servers). `opts.on_progress` wins over `progress_to_log`.
local function wire_progress(sn: string, opts: RunOpts)
    if opts.on_progress then
        local user_cb = opts.on_progress
        -- Registered directly on the main Isle (no bytecode dump needed): the
        -- callback runs with upvalues intact because the main Isle is never
        -- crossed; only the event table `ev` is built on the Rust side.
        mcp.on_progress(sn, function(ev: host.Mcp.Notification)
            local ok, cb_err = pcall(user_cb, ev)
            if not ok then
                log.warn("agent: on_progress callback error: " .. tostring(cb_err))
            end
        end)
    elseif opts.progress_to_log then
        mcp.on_progress(sn, function(ev: host.Mcp.Notification)
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
local function wire_log(sn: string, opts: RunOpts)
    if opts.on_log then
        local user_cb = opts.on_log
        mcp.on_log(sn, function(ev: host.Mcp.Notification)
            local ok, cb_err = pcall(user_cb, ev)
            if not ok then
                log.warn("agent: on_log callback error: " .. tostring(cb_err))
            end
        end)
    else
        mcp.on_log(sn, function(ev: host.Mcp.Notification)
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
local function wire_capabilities(sn: string, opts: RunOpts): {Candidate}
    local candidates: {Candidate} = {}
    if not (opts.enable_resources or opts.enable_prompts or opts.on_log or opts.log_to_stderr) then
        return candidates
    end
    local si_result = mcp.server_info(sn)
    if not si_result.ok then
        log.warn("agent: mcp.server_info failed for '" .. sn .. "': " .. tostring(si_result.error))
        return candidates
    end
    local caps = ((si_result.server_info and si_result.server_info.capabilities) or {}) as {string:any}

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
local function mcp_tool_candidates(sn: string, active_groups: {string}): {Candidate}, string
    local list = mcp.list_tools(sn)
    if not list.ok then
        return nil, "mcp list_tools failed for '" .. sn .. "': " .. tostring(list.error)
    end
    local group_set = group_set_of(active_groups)
    local candidates: {Candidate} = {}
    for _, entry in ipairs(list.tools or {}) do
        local group = resolve_mcp_group(entry, sn)
        if in_groups(group, group_set) then
            candidates[#candidates + 1] = { group = group, bind = adapter.ToolPort.mcp(sn, entry as mcp_tools.Tool) }
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
local function connect_mcp_servers(servers: {McpServer}, opts: RunOpts): {Candidate}, string, {string}
    local candidates: {Candidate} = {}
    local connected: {string} = {}

    for _, srv in ipairs(servers) do
        local name = srv.name

        -- HTTP transport when srv.url is set, otherwise stdio.
        local ok: boolean
        local err: any
        if srv.url then
            -- Merge server-level trace_context into transport_opts when not already set.
            local transport_opts: {string:any} = {}
            for k, v in pairs(srv.transport_opts or {}) do
                transport_opts[k] = v
            end
            if transport_opts.trace_context == nil then
                transport_opts.trace_context = not not srv.trace_context
            end
            ok, err = pcall((mcp.connect_http as function(string, string, {string:any}): any), name, srv.url, transport_opts)
        else
            local connect_opts: {string:any} = { trace_context = not not srv.trace_context }
            ok, err = pcall(
                (mcp.connect as function(string, string, {string}, {string:any}): any),
                name,
                srv.command,
                srv.args or {},
                connect_opts
            )
        end
        if not ok then
            return nil, "mcp connect failed for '" .. name .. "': " .. tostring(err), connected
        end
        table.insert(connected, name)

        if opts.sampling then
            local sampling_ok, sampling_err = pcall(
                (mcp.set_sampling_handler as function(string, function(any): any): any),
                name,
                opts.sampling
            )
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
local function disconnect_mcp_servers(server_names: {string})
    for _, name in ipairs(server_names) do
        local ok, err = pcall((mcp.disconnect as function(string): any), name)
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
local DEFAULT_CONTEXT_MANAGEMENT: {string:any} = {
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
local AGENT_OPTS: {string:boolean} = {
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
    sampling = true,
    on_progress = true,
    progress_to_log = true,
    on_log = true,
    log_to_stderr = true,
    enable_resources = true,
    enable_prompts = true,
    context_management = true,
    context_management_config = true,
    compact = true,
}

--- `opts.context_management == false` opts out entirely (no beta header, no
--- body field). Strict equality, so nil (unset) is default-on.
local function resolve_context_management(opts: RunOpts): {string:any}
    if opts.context_management == false then
        return nil
    end
    return opts.context_management_config or DEFAULT_CONTEXT_MANAGEMENT
end

--- Everything the Port is opened with: the caller's knobs verbatim, the two
--- request defaults this module has always supplied, and the resolved
--- context-management config on the provider that has one.
local function port_conf(opts: RunOpts, provider: string): {string:any}
    local conf: {string:any} = {}
    for key, value in pairs(opts as {string:any}) do
        if not AGENT_OPTS[key] then
            conf[key] = value
        end
    end
    -- `max_tokens` passes as named or not at all: absent, an OpenAI-compatible
    -- server caps the reply at what its window has left, which is the cap the
    -- model actually has; a number placed here instead was a second, smaller
    -- ceiling nobody chose (the Anthropic adapter fills the field its API
    -- requires). `timeout` is how long this model's reply may take, which
    -- the caller who picked the model knows and this module does not.
    conf.max_tokens = opts.max_tokens
    conf.timeout = opts.timeout
    if provider ~= "openai" then
        conf.context_management = resolve_context_management(opts)
    end
    return conf
end

--- Anthropic-only knobs are warned about rather than dropped in silence.
local function warn_anthropic_only(opts: RunOpts, provider: string)
    if provider ~= "openai" then
        return
    end
    local raw = opts as {string:any}
    for _, name in ipairs({ "cache_control", "context_management", "context_management_config" }) do
        if raw[name] ~= nil then
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
local function seed_message(session: kernel.Session, message: proto.Message)
    local content = message.content
    if message.role == "assistant" then
        session:append({ kind = "llm_response", data = { content = content } })
        return
    end
    if not (content is table) then
        session:append({ kind = "msg_user", data = { content = content } })
        return
    end
    local rest: {any} = {}
    for _, item in ipairs(content as {any}) do
        local block = item as proto.Block
        if item is table and block.type == "tool_result" then
            session:append({
                kind = "tool_result",
                data = {
                    call_id = block.tool_use_id,
                    ok = (block as {string:any}).is_error ~= true,
                    result = block.content or "",
                },
            })
        else
            rest[#rest + 1] = item
        end
    end
    if #rest > 0 then
        session:append({ kind = "msg_user", data = { content = rest } })
    end
end

--- One correlation id: the `meta` key, and the env var it is read from.
local record Correlation ---@struct
    key: string
    var: string
end

--- The four correlation ids, as the env var each is read from and the `meta`
--- key it becomes. These are the same variables `obs::obs_context` reads for
--- the `ab.obs` http_request / http_response lines the http bridge writes, so
--- a run's log events and its model calls carry one set of ids rather than
--- two.
local CORRELATION_ENV: {Correlation} = {
    { key = "trace_id", var = "AGENT_BLOCK_TRACE_ID" },
    { key = "run_id", var = "AGENT_BLOCK_RUN_ID" },
    { key = "agent_id", var = "AGENT_BLOCK_AGENT_ID" },
    { key = "agent_name", var = "AGENT_BLOCK_AGENT_NAME" },
}

--- The seed event's labels: `label = "prompt"`, plus whichever correlation ids
--- the environment set.
---
--- Only the keys that are set — an unset one, and an empty one, is absent
--- rather than an empty string, so `json_extract(meta, '$.run_id')` answers
--- NULL for a run that had none instead of matching every other run that also
--- had none. Strings only, and flat: `meta` is the kernel's shallow label map
--- (`knl.shapes.event_meta`).
---
--- `agent_id` is the one that can be absent while the obs lines still carry
--- it: the http bridge falls back to a per-process id (`obs::process_agent_id`)
--- that Lua is not given a reader for. Set `AGENT_BLOCK_AGENT_ID` and both
--- sides say the same thing.
local function prompt_meta(): {string:string | number | boolean}
    local meta: {string:string | number | boolean} = { label = "prompt" }
    for _, entry in ipairs(CORRELATION_ENV) do
        local value = std.env.get(entry.var)
        if value ~= nil and value ~= "" then
            meta[entry.key] = value
        end
    end
    return meta
end

--- The caller's prior thread, then the prompt. History is a durable record
--- here rather than an argument: what `blocks/lib/session` saved is what this
--- lays back down, and the fold reads the two the same way.
local function seed(session: kernel.Session, opts: RunOpts)
    for _, message in ipairs(opts.history or {}) do
        seed_message(session, message)
    end
    session:append({ kind = "msg_user", meta = prompt_meta(), data = { content = opts.prompt } })
end

-- ============================================================
-- Preparing the run
-- ============================================================

--- What the loop needs, prepared once: the checked opts, the session it is
--- opened with, the device, and the servers to disconnect afterwards.
local record Run
    opts: RunOpts
    --- `knl.session`'s opts: the owner, the grant, the store.
    session: kernel.OpenOpts
    --- The provider Port the device's `llm` closure was opened from.
    port: Port
    candidates: {Candidate}
    device: kernel.Device
    --- The session's grant: one unit per beat.
    max_iter: number
    on_turn: function(TurnInfo): any
    --- `max_tokens_budget`: nil is unlimited.
    budget: number
    --- The servers `M.run` disconnects, whatever the loop did.
    connected: {string}
    --- With `compact`: the window's predicate, the compaction, and the
    --- ledger read beside every summary. All nil otherwise.
    fits: room.Fits
    compaction: Compaction
    ledger: pcompact.LedgerFn
end

--- The compaction, when the run has one: the question asked before every
--- beat, and the device the one beat that writes the summary runs with.
local record Compaction
    due: pcompact.Due
    writer: kernel.Device
end

--- `compact` as a table: nil when off, the caller's table, or an empty one
--- for `true`.
local function compact_of(given: any): CompactOpt
    if given == nil or given == false then
        return nil
    end
    if given == true then
        return {}
    end
    return given as CompactOpt
end

--- What a run that compacts needs beyond the plain device, or the failure
--- it comes back with: the window fold from the summary on, the fit
--- predicate, the compaction and the ledger. The window and the reply's
--- room are the two facts about the model this module cannot answer, so a
--- conf that names neither is refused rather than filled in.
local record Seams
    fold: room.Fold
    filters: {room.Filter}
    fits: room.Fits
    compaction: Compaction
    ledger: pcompact.LedgerFn
end

local function compaction_of(port: Port, conf: {string:any}, system: any, compact: CompactOpt): Seams, string
    if type(compact) ~= "table" then
        return nil, "compact must be true or { at?, min_beats?, reserve? }"
    end
    local profile = port:profile(conf)
    if type(profile) ~= "table" or type(profile.context_window) ~= "number" then
        return nil, "compact needs the model's window: declare context_window beside the model"
    end
    if conf.max_tokens == nil and compact.reserve == nil then
        return nil, "compact needs the reply's room: max_tokens, or compact.reserve (tokens held back for the reply)"
    end
    -- The window as one value, read by the fold and the window line alike.
    local the_room = policy.room({ port = port as shared.Port, conf = conf, reserve = compact.reserve as integer })
    local fold, fits = policy.window({ room = the_room, keep_seed = true, from_summary = true })
    local due, summarise = policy.compact({ at = compact.at, min_beats = compact.min_beats, fold = fold })
    local llm = port:open(conf)
    local writer = kernel.device({ llm = llm, system = system, fold = summarise })
    -- No check kinds: this loop records no verdicts. The tools' `path`
    -- arguments are what a summary of a run over files most often loses.
    local ledger = policy.ledger({ track = { "path" }, kinds = {} })
    return {
        fold = fold,
        filters = { policy.room_note({ room = the_room }) },
        fits = fits,
        compaction = { due = due, writer = writer },
        ledger = ledger,
    },
    nil
end

--- The opts, checked, and everything the loop runs on built from them.
---
--- Answers the `Run`, or nil and the failure a run that never started comes
--- back as. A server that connected before one that failed is disconnected
--- here; on success, the connected ones ride on the `Run` for `M.run` to
--- disconnect once the loop is done.
local function prepare(opts: RunOpts): Run, RunResult
    opts = opts or {}

    if not opts.prompt or opts.prompt == "" then
        return nil, failed("prompt is required")
    end
    local history = opts.history as any
    if history ~= nil and not (history is table) then
        return nil, failed("history must be a table (messages array)")
    end
    -- Both are the run's own: how long this model's reply may take, and how
    -- many beats the session is granted. Neither is a number this module
    -- could know, so neither has one here.
    local timeout = opts.timeout as any
    if not (timeout is number) then
        return nil, failed("timeout (seconds) is required")
    end
    local max_iterations = opts.max_iterations as any
    if not (max_iterations is number) then
        return nil, failed("max_iterations is required")
    end

    local provider = opts.provider or "anthropic"
    if PORTS[provider] == nil then
        return nil, failed("unknown provider '" .. tostring(provider) .. "' (anthropic | openai)")
    end
    warn_anthropic_only(opts, provider)

    local candidates = registry_candidates()
    local connected: {string} = {}
    if opts.mcp_servers and #opts.mcp_servers > 0 then
        local bound, err, partial = connect_mcp_servers(opts.mcp_servers, opts)
        if err then
            -- Disconnect any servers that did connect before the failure.
            disconnect_mcp_servers(partial)
            return nil, failed(err)
        end
        connected = partial
        append_all(candidates, bound)
    end
    append_all(candidates, extra_candidates(opts.extra_tools))

    -- The device raises for exactly the things a caller got wrong at wiring
    -- time — a duplicate tool name, a spec with no schema field it knows.
    -- `run` never throws, so those come back as the failure they are. With
    -- `compact`, the window seams go on it: the fold from the summary on
    -- and the window line, both sized by the Port.
    local port = PORTS[provider]
    local conf = port_conf(opts, provider)
    local compact = compact_of(opts.compact)
    local fits, compaction, ledger: room.Fits, Compaction, pcompact.LedgerFn = nil, nil, nil
    local built, device = pcall(function(): kernel.Device
        local config: kernel.DeviceConfig = {
            llm = port:open(conf),
            tools = build_tools(candidates, opts.tool_groups),
            system = opts.system,
        }
        if compact ~= nil then
            local seams, why = compaction_of(port, conf, opts.system, compact)
            if seams == nil then
                error(why, 0)
            end
            config.fold = seams.fold
            config.filters = seams.filters
            fits, compaction, ledger = seams.fits, seams.compaction, seams.ledger
        end
        return kernel.device(config)
    end)
    if not built then
        -- Always disconnect, regardless of outcome.
        disconnect_mcp_servers(connected)
        return nil, failed(tostring(device))
    end

    local max_iter = opts.max_iterations
    return {
        opts = opts,
        fits = fits,
        compaction = compaction,
        ledger = ledger,
        session = {
            owner = "agent",
            -- One unit per beat (the device's default cost), so the grant IS the
            -- iteration cap: the beat after the last one the caller allowed comes
            -- back `stopped` with nothing called and nothing recorded.
            budget = { amount = max_iter, tag = "beats", desc = "one unit per beat" },
            store = opts.store,
        },
        port = port,
        candidates = candidates,
        device = device as kernel.Device,
        max_iter = max_iter,
        on_turn = opts.on_turn,
        budget = opts.max_tokens_budget,
        connected = connected,
    }, nil
end

-- ============================================================
-- Reading an answer
-- ============================================================
--
-- `text_of` here and `error_text` / `refusal_text` below are consumer
-- plumbing: reading an Outcome is the caller's job, and every loop written
-- over the kernel does it the same way. A loop of your own writes its own,
-- or copies these — they are short on purpose.

--- The `tool_use` blocks of a response, in block order. This is what
--- `on_turn` has always been handed as `tool_calls`, and what the loop counts
--- to decide whether the model asked for anything.
local function tool_use_blocks(content: {proto.Block}): {proto.Block}
    local out: {proto.Block} = {}
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
local function text_of(content: {proto.Block}): string
    local parts: {string} = {}
    for _, block in ipairs(content or {}) do
        if block.type == "text" and block.text then
            parts[#parts + 1] = block.text
        end
    end
    return table.concat(parts, "\n")
end

-- ============================================================
-- Reading a beat
-- ============================================================

--- The token spend so far, read off the log.
---
--- A reading apart from the budget: the grant counts beats and the kernel
--- never folds usage back into it, so this is one SELECT over the counts every
--- `llm_response` already carries.
local function usage_of(session: kernel.Session): Usage
    local rows = kernel.views.usage(session)
    local row: {string:any} = (rows and rows[1]) or {}
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
local function error_text(o: kernel.OutcomeValue): string
    local detail = o.detail
    if not (detail is table) then
        return tostring(o.kind) .. ": " .. tostring(detail)
    end
    local d = detail as {string:any}
    local message = tostring(d.message or "unknown failure")
    if d.kind ~= nil then
        return tostring(o.kind) .. ": " .. tostring(d.kind) .. ": " .. message
    end
    return tostring(o.kind) .. ": " .. message
end

--- A refusal names its class. `reason` is the adapter's provider-neutral
--- classification ("model" / "content_filter"); the provider's own message,
--- when it sent one, is on the normalized refusal.
local function refusal_text(o: kernel.OutcomeValue): string
    local text = "model refused to respond (kind=" .. tostring(o.reason) .. ")"
    local detail = o.detail
    local said: any = nil
    if detail is table then
        local refusal = (detail as {string:any}).refusal
        if refusal is table then
            said = (refusal as {string:any}).detail
        end
    end
    if said is string and said ~= "" then
        text = text .. ": " .. said
    end
    return text
end

--- The caller's per-beat hook. A broken callback must not take the run down,
--- and a callback that answers `false` stops it.
local function fire_on_turn(
    on_turn: function(TurnInfo): any,
    turn_number: number,
    answer: kernel.LlmResult,
    calls: {proto.Block}
): any
    if not on_turn then
        return nil
    end
    local info: TurnInfo = {
        turn_number = turn_number,
        content = answer.content as {proto.Block},
        tool_calls = calls,
        usage = answer.usage,
    }
    local ok, verdict = pcall(on_turn, info)
    if not ok then
        log.warn("agent: on_turn callback error: " .. tostring(verdict))
        return nil
    end
    return verdict
end

-- ============================================================
-- The loop
-- ============================================================

--- What the loop ended with: the beats that reached the provider, the last
--- answer's text, the failure that ended it (nil for a settled answer), and
--- the spend read after the last beat.
local record Tally ---@struct
    turns: number
    content: string
    failure: string
    usage: Usage
end

--- Run the loop: one beat, then decide, until something ends it.
---
--- @return Tally  what `result_of` reads the result out of
local function run_loop(s: kernel.Session, run: Run): Tally
    local max_iter = run.max_iter
    local limit = run.budget
    local turns, content, failure: number, string, string = 0, "", nil
    local usage = zero_usage()

    --- One compaction: the summarising beat, and the `summary` event with
    --- the ledger beside what the model wrote. Answers the failure that
    --- ends the run, or nil.
    local function compact_once(why: string): string
        local o = kernel.beat(s, run.compaction.writer)
        if o.status == "stopped" then
            return "max_iterations (" .. run.max_iter .. ") reached"
        elseif o.status == "error" then
            return error_text(o)
        elseif o.status == "refused" then
            return refusal_text(o)
        end
        local text = text_of((o.out as kernel.LlmResult).content as {proto.Block})
        if not text:match("%S") then
            -- Nothing to fold from: the run goes on over the window it had,
            -- and the mark keeps `due` from asking again on the next beat.
            s:append({ kind = "compact_skipped", meta = { label = "compact", reason = why }, data = { why = "empty" } })
            return nil
        end
        s:append({
            kind = "summary",
            meta = { label = "compact", reason = why },
            data = { content = text, ledger = run.ledger(s) },
        })
        return nil
    end

    while true do
        if run.compaction ~= nil then
            -- Before the beat: a request the window could no longer hold is
            -- the request a summary makes room in, so the compaction is
            -- asked first and the fit is read on what it left.
            local why = run.compaction.due(s)
            if why ~= nil then
                failure = compact_once(why)
                if failure ~= nil then
                    break
                end
            end
            local over, tokens, allowed = run.fits(s, run.device)
            if over ~= nil then
                failure = string.format(
                    "context: the request does not fit the model's window (%d tokens, %d allowed)",
                    tokens as integer,
                    allowed as integer
                )
                break
            end
        end
        local going = Outcome.match(kernel.beat(s, run.device), {
            stopped = function(o: kernel.OutcomeValue): any
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
            error = function(o: kernel.OutcomeValue): any
                turns = turns + 1
                usage = usage_of(s)
                failure = error_text(o)
                return false
            end,
            refused = function(o: kernel.OutcomeValue): any
                turns = turns + 1
                usage = usage_of(s)
                failure = refusal_text(o)
                return false
            end,
            ok = function(o: kernel.OutcomeValue): any
                turns = turns + 1
                usage = usage_of(s)
                local answer = o.out as kernel.LlmResult
                local blocks = answer.content as {proto.Block}
                content = text_of(blocks)
                local calls = tool_use_blocks(blocks)
                if fire_on_turn(run.on_turn, turns, answer, calls) == false then
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

    return { turns = turns, content = content, failure = failure, usage = usage }
end

-- ============================================================
-- Reading the run
-- ============================================================

--- The loop's tally, and the thread it leaves in the log, as the result.
---
--- @return RunResult  the RUN_OK / RUN_ERR result
local function result_of(s: kernel.Session, run: Run, tally: Tally): RunResult
    local failure, usage, turns = tally.failure, tally.usage, tally.turns

    -- The thread, rebuilt from the log by the same fold the requests were
    -- built with, so what a caller saves and hands back as `history` is
    -- what this run actually sent.
    --
    -- The read is bounded and says when it cut (knl's header). A run can
    -- cross the cap on its last beat — the beat itself read the log before
    -- writing its request, response and tool pairs — so this readback is
    -- where it shows up. There is no half-history worth handing over:
    -- `messages` is what a caller feeds back as `history`, and a prefix
    -- missing the newest turns would seed the next run with a conversation
    -- that stops in the middle. So the run reports it and carries none.
    local recorded, truncated = s:events()
    if truncated then
        local cut = "the session log is longer than one read of it ("
            .. #recorded
            .. " events, the kernel's row cap), so this run's history cannot be rebuilt whole"
        return {
            ok = false,
            -- A failure that already ended the loop is what the caller
            -- needs first; this is why there are no messages beside it.
            error = failure ~= nil and (failure .. "; " .. cut) or cut,
            usage = usage,
            num_turns = turns,
            messages = {},
        }
    end
    local messages = kernel.fold(recorded as {any}, run.device).messages
    if failure ~= nil then
        return { ok = false, error = failure, usage = usage, num_turns = turns, messages = messages }
    end
    return { ok = true, content = tally.content, usage = usage, num_turns = turns, messages = messages }
end

-- ============================================================
-- Public: agent.run(opts)
-- ============================================================

--- Run a ReAct agent loop.
---
--- @param opts table  {
---   prompt          (required) Initial user prompt string
---   system          (optional) System prompt string
---   model           (optional) LLM model identifier
---   max_tokens      (optional) Per-request token limit. Absent, the request
---                   carries none and the server's own cap is the cap
---   timeout         (required) HTTP timeout in seconds
---   max_iterations  (required) Max beats. The session's grant.
---   max_tokens_budget (optional) Total token budget across all beats, read
---                   off the log after each one (default: nil = unlimited)
---   store           (optional) Where the session log goes. Omitted is the
---                   host's own database; "mem" and { sqlite = <path> } are
---                   the other two (see knl's header).
---   mcp_servers     (optional) Array of { name, command, args? } / { name, url }
---   on_turn         (optional) Callback function(turn_info). turn_info has
---                   keys: turn_number, content, tool_calls, usage.
---                   Returning false stops the run.
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
--- There is no correlation-id option. The run's seed event is stamped with
--- `AGENT_BLOCK_TRACE_ID` / `_RUN_ID` / `_AGENT_ID` / `_AGENT_NAME` as `meta`
--- labels — the same four the http bridge puts on its `ab.obs` lines — so a
--- run's events are selected the way its model calls are grepped:
---
---     session:query("SELECT * FROM events WHERE json_extract(meta, '$.run_id') = ?",
---                   { std.env.get("AGENT_BLOCK_RUN_ID") })
---
--- Only the variables that are set become labels; see `prompt_meta`.
---
--- The three steps in a row: `prepare`, then `run_loop` inside the session
--- the `Run` names (seeded first), then `result_of` inside the same bracket,
--- since the thread is read back out of the session before it closes.
---
--- The contract is checked on the way out in dev mode (LSHAPE_CHECK=1); see
--- `M.shapes.run_result`. Checked once, on the one value every path leaves
--- through, rather than asserted at each `return` — there are five of them,
--- and a sixth added later would be the one that skips the check.
function M.run(opts: RunOpts): RunResult
    local result: RunResult
    local run, refused = prepare(opts)
    if run == nil then
        result = refused
    else
        -- The loop raises for a store that would not take the log; `run`
        -- never throws, so that comes back as the failure it is.
        local ran, out = pcall(kernel.session, run.session, function(s: kernel.Session): RunResult
            seed(s, run.opts)
            return result_of(s, run, run_loop(s, run))
        end)

        -- Always disconnect, regardless of outcome.
        disconnect_mcp_servers(run.connected)

        if ran then
            result = out as RunResult
        else
            result = failed(tostring(out))
        end
    end
    shape.assert_dev(result, RUN_RESULT, "agent.run result")
    return result
end

-- ============================================================
-- Internals exposed for the specs
-- ============================================================

-- The tool-set builders, for tests only. One surface, not two: they used to be
-- reachable both here and through `M._test_helpers()`, and the fixtures only
-- ever reached for these.
M._build_tools = build_tools
M._registry_candidates = registry_candidates
M._extra_candidates = extra_candidates
M._resolve_mcp_group = resolve_mcp_group

--- The contracts this module holds itself to, as data.
---
--- Public so a caller checking a result, or a fixture checking what the
--- `mcp.call` bridge produced, can read the same schema rather than a doc
--- comment.
M.shapes = {
    usage = USAGE,
    run_result = RUN_RESULT,
    mcp_call_result = MCP_CALL_RESULT,
}

--- Pure internal helpers, for unit tests only. No side effects beyond the
--- std/log globals they read.
---
--- The three `proto_openai` entries are re-exports, not this module's code:
--- they are the only coverage `llm_proto`'s OpenAI finish-reason mapping and
--- message conversion have, and they are reached through here because that is
--- where the spec that covers them already runs. The tool-set builders are NOT
--- here — `M._build_tools` and friends above are their one surface.
function M._test_helpers(): TestHelpers
    return {
        map_finish_reason = proto_openai.map_finish_reason,
        normalize_openai_response = proto_openai.parse,
        convert_messages_to_openai = proto_openai.convert_messages,
        tool_use_blocks = tool_use_blocks,
        text_of = text_of,
    }
end

return M
