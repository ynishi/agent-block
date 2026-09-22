--- policy.tools — the policies that wrap a device's `tools` map.
---
--- Two of the eleven: `repeat_cap`, which refuses the same call again with
--- nothing changed in between, and `require_args`, which refuses a call that
--- arrived without an argument its own schema declares required. Each takes
--- the map `knl_adapter.tools` built and hands back a new one whose handlers
--- hold the call to the rule before the tool runs; neither retries, repairs
--- or remembers.
---
--- Exported through `policy`; see that header for the rules every policy
--- keeps. This file is `require`d by `policy/init.lua` and not meant to be
--- reached for directly.

local lshape = require("lshape")
local shared = require("policy.shared")
local T = lshape.t
local shape = lshape.check

local M = {}

-- What this file shares with its siblings, by the names the code was
-- written against (`policy.shared`).
local DEFAULT_REPEAT_MAX = shared.DEFAULT_REPEAT_MAX
local only = shared.only
local whole_log = shared.whole_log
local needs_session = shared.needs_session
local opts_contract = shared.opts_contract
local arg_of = shared.arg_of
local SESSION_ARG = shared.SESSION_ARG

--- What `policy.repeat_cap` is configured with. `resets` names the tools whose
--- success makes an old call new again — an edit changes what a read would
--- answer.
local REPEAT_CAP_OPTS, REPEAT_CAP_ARG = opts_contract({
    max = T.number
        :describe("how many times one call (tool + arguments) may be made with nothing reset in between; default 2")
        :is_optional(),
    resets = T.table
        :describe("tool names whose successful result starts the count over for every call; default none")
        :is_optional(),
})

--- What `policy.require_args` is configured with: nothing. The list of
--- arguments a call must carry is the tool's own (`input_schema.required`),
--- so there is no threshold here to name and no model-specific number to
--- tune. The contract is declared all the same — an option passed to it is a
--- caller expecting a knob this policy does not have, and saying so is worth
--- more than accepting it silently.
local REQUIRE_ARGS_OPTS, REQUIRE_ARGS_ARG = opts_contract({})

-- ============================================================
-- repeat_cap — the same call again, with nothing changed, is refused
-- ============================================================

--- The JSON a call's arguments render to: what "the same call" compares.
--- Key order is not promised by every encoder, so the keys are sorted here
--- before encoding — a table read back from the log and a table handed to a
--- handler must key alike.
local function call_key(name, args)
    if type(args) ~= "table" then
        return tostring(name) .. "\0" .. tostring(args)
    end
    local keys = {}
    for k in pairs(args) do
        keys[#keys + 1] = tostring(k)
    end
    table.sort(keys)
    local parts = {}
    for _, k in ipairs(keys) do
        local v = args[k]
        parts[#parts + 1] = k .. "=" .. (type(v) == "table" and std.json.encode(v) or tostring(v))
    end
    return tostring(name) .. "\0" .. table.concat(parts, "\1")
end

--- Whether a `tool_result` says its tool did what it was asked. The kernel's
--- `ok` is raise detection; a tool that refuses in its return value (`std.fs`
--- does) says so with `result.ok == false`. Both are read.
local function result_succeeded(data)
    if data.ok == false then
        return false
    end
    local result = data.result
    if type(result) == "table" and result.ok == false then
        return false
    end
    return true
end

--- Build a wrapper over a device's `tools` map that refuses a call already
--- made `max` times since the last reset.
---
---     local tools = policy.repeat_cap({ max = 2, resets = { "fs_edit" } })(session)(raw_tools)
---
--- The model, its context window full, drops old reads out of the
--- conversation and asks for them again — the same file, the same range —
--- and asks again when those drop too, without ever editing. Measured on a
--- vLLM-served model against a file larger than its window: the loop ran
--- out of budget having read one range eleven times. Nothing in the
--- kernel is wrong: every call was answered. The tool layer is where a
--- repeated question can be told it is repeated, and the answer that helps
--- is a refusal that says so, not the content again.
---
--- The count is read off the log, not kept. Like `carry`, the factory
--- answers a BINDER: `policy.repeat_cap{...}(session)` is the value that
--- wraps `tools`, and every call counts its own `tool_call` records since
--- the last successful result of a tool in `resets` — an edit that landed
--- makes an old read new, since the file it would read has changed. The
--- kernel records the `tool_call` before it runs the handler, so the call
--- being answered is in its own count: `max = 2` lets a call through twice
--- and refuses the third. A process that restarted resumes the same count;
--- two drivers on one log refuse alike.
---
--- The refusal is a return value, `{ ok = false, reason = "repeated", ... }`,
--- so the kernel records the pair and the model reads why. `result_cap` is
--- the sibling for the other way a tool result breaks a run; the two
--- compose in either order.
---
--- @param opts table|nil  { max?, resets? }
--- @return function binder  fn(session) -> fn(tools) -> tools
function M.repeat_cap(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.repeat_cap: opts must be a table", 2)
    end
    only(opts, { max = true, resets = true }, "policy.repeat_cap")
    if opts.max ~= nil and (type(opts.max) ~= "number" or opts.max < 1 or opts.max % 1 ~= 0) then
        error("policy.repeat_cap: max must be a whole number >= 1, got " .. tostring(opts.max), 2)
    end
    if opts.resets ~= nil then
        if type(opts.resets) ~= "table" then
            error("policy.repeat_cap: resets must be an array of tool names", 2)
        end
        for i, name in ipairs(opts.resets) do
            if type(name) ~= "string" or name == "" then
                error("policy.repeat_cap: resets[" .. i .. "] must be a non-empty tool name", 2)
            end
        end
    end
    shape.assert_dev(opts, REPEAT_CAP_OPTS, "policy.repeat_cap opts")

    local max = opts.max or DEFAULT_REPEAT_MAX
    local resets = {}
    for _, name in ipairs(opts.resets or {}) do
        resets[name] = true
    end

    return function(session)
        needs_session(session, "policy.repeat_cap")

        --- How many times `key` has been called since the last reset, off
        --- the log: every `tool_call` with that key after the newest
        --- successful `tool_result` of a reset tool.
        local function count(key)
            local events = whole_log(session, "policy.repeat_cap")
            local reset_tools_by_call = {}
            local since = 0
            for i, ev in ipairs(events) do
                local data = type(ev.data) == "table" and ev.data or {}
                if ev.kind == "tool_call" then
                    if resets[data.name] and data.call_id ~= nil then
                        reset_tools_by_call[data.call_id] = true
                    end
                elseif ev.kind == "tool_result" and data.call_id ~= nil and reset_tools_by_call[data.call_id] then
                    if result_succeeded(data) then
                        since = i
                    end
                end
            end
            local n = 0
            for i = since + 1, #events do
                local ev = events[i]
                if ev.kind == "tool_call" then
                    local data = type(ev.data) == "table" and ev.data or {}
                    if call_key(data.name, data.args) == key then
                        n = n + 1
                    end
                end
            end
            return n
        end

        return function(tools)
            if type(tools) ~= "table" then
                error("policy.repeat_cap: tools must be the device's map of name -> entry", 2)
            end
            local out = {}
            for name, entry in pairs(tools) do
                if type(entry) ~= "table" or type(entry.handler) ~= "function" then
                    error("policy.repeat_cap: tool '" .. tostring(name) .. "' has no handler", 2)
                end
                local capped = {}
                for k, v in pairs(entry) do
                    capped[k] = v
                end
                local handler = entry.handler
                capped.handler = function(args)
                    local seen = count(call_key(name, args))
                    if seen <= max then
                        return handler(args)
                    end
                    return {
                        ok = false,
                        reason = "repeated",
                        times = seen,
                        max = max,
                        error = string.format(
                            "'%s' was already called with exactly these arguments %d times and nothing has "
                                .. "changed since. The answer would be the same. Act on what you already saw "
                                .. "instead of asking again.",
                            tostring(name),
                            seen - 1
                        ),
                    }
                end
                out[name] = capped
            end
            return out
        end
    end
end

-- ============================================================
-- require_args — a call that arrived without its arguments is refused
-- ============================================================

--- Build a wrapper over a device's `tools` map that refuses a call missing
--- one of the arguments its own schema declares required.
---
---     tools = policy.require_args()(
---         knl_adapter.tools({ read_spec, edit_spec })
---     )
---
--- WHAT THIS IS ABOUT. A reply that runs out of room stops wherever it had
--- got to, and where it had got to may be the middle of a tool call's
--- arguments. The call still reaches the tool — with a `path` and no
--- `content`, with an `edits` array that was never opened — and the handler
--- then answers whatever its own vocabulary has for the field it found
--- missing, which says nothing about the reply having been cut
--- [measured 2026-09-17 in a sibling lane: five `fs_write` calls arrived with
---  no `content`, and on three of them nothing told the model the call had
---  been cut, so it sent the same shape again. The same accident is recorded
---  in this tree's own `fs_tools`, as `path_missing`].
---
--- THE CHECK IS ON THE ARGUMENTS, not on what the server said. The wire has
--- a field for exactly this — `finish_reason` / `stop_reason` — and the
--- servers do not fill it reliably: vLLM reports a call cut at the output
--- ceiling as a whole one (`tool_calls`), llama.cpp answers `stop` for
--- everything. So the fact is read where it cannot be misreported: the
--- arguments that arrived, against the list the tool declares. The two are a
--- pair rather than alternatives — a consumer that also reads `stop_reason`
--- learns it about a beat with no tool call at all, which this cannot see.
---
--- NOTHING IS RETRIED AND NOTHING IS REPAIRED. The refusal is a return value
--- the model reads, `{ ok = false, reason = "argument_missing", missing }`,
--- and the next move is the model's. Re-sending the same request on the
--- harness's own initiative is the failure mode the other harnesses report
--- (Cline: a required-argument check followed by the same prompt, looping);
--- guessing the missing argument is worse, and vLLM's own position on a
--- truncated call is that it will not be inferred from the JSON or repaired.
---
--- A tool whose schema declares no `required` list is handed back untouched —
--- the same entry, not a copy of it: there is nothing to check, so there is
--- no reason for a call to it to pass through one more function.
---
--- @param opts table|nil  no options; passing one is refused by name
--- @return function bind  fn(tools) -> tools (a new map; the argument is not changed)
function M.require_args(opts)
    opts = opts or {}
    if type(opts) ~= "table" then
        error("policy.require_args: opts must be a table", 2)
    end
    only(opts, {}, "policy.require_args")
    shape.assert_dev(opts, REQUIRE_ARGS_OPTS, "policy.require_args opts")

    return function(tools)
        if type(tools) ~= "table" then
            error("policy.require_args: tools must be the device's map of name -> entry", 2)
        end
        local out = {}
        for name, entry in pairs(tools) do
            if type(entry) ~= "table" or type(entry.handler) ~= "function" then
                error("policy.require_args: tool '" .. tostring(name) .. "' has no handler", 2)
            end
            local schema = entry.input_schema
            local declared = type(schema) == "table" and schema.required or nil
            local required = {}
            if type(declared) == "table" then
                for _, key in ipairs(declared) do
                    if type(key) == "string" then
                        required[#required + 1] = key
                    end
                end
            end
            if #required == 0 then
                out[name] = entry
            else
                local checked = {}
                for key, value in pairs(entry) do
                    checked[key] = value
                end
                local handler = entry.handler
                checked.handler = function(args)
                    local given = type(args) == "table" and args or {}
                    for _, key in ipairs(required) do
                        if given[key] == nil then
                            return {
                                ok = false,
                                reason = "argument_missing",
                                missing = key,
                                error = key
                                    .. " never arrived. A reply that runs out of room stops part-way through "
                                    .. "its arguments, so the call reaches the tool without them. Send a "
                                    .. "smaller call.",
                            }
                        end
                    end
                    return handler(args)
                end
                out[name] = checked
            end
        end
        return out
    end
end

--- The contracts this file publishes; `policy.shapes` gathers them.
M.shapes = {
    require_args_opts = REQUIRE_ARGS_OPTS,
}

--- This file's entries in `policy.shapes.api`, gathered there.
M.api = {
    repeat_cap = {
        args = { arg_of(REPEAT_CAP_ARG, "opts") },
        returns = "bind — fn(session) -> fn(tools) -> tools",
        members = {
            bind = {
                args = { SESSION_ARG },
                returns = "wrap — fn(tools) -> tools",
            },
            wrap = {
                args = { arg_of(T.table, "tools (the device's map of name -> entry)") },
                returns = "table — the same map, each handler refusing a repeated call",
            },
        },
    },
    require_args = {
        args = { arg_of(REQUIRE_ARGS_ARG, "opts") },
        returns = "bind — fn(tools) -> tools",
        members = {
            bind = {
                args = { arg_of(T.table, "tools (the device's map of name -> entry)") },
                returns = "table — the same map, each handler holding its call to the schema's `required`",
            },
        },
    },
}

return M
