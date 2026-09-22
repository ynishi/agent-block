--- mcp_tools — MCP's tool vocabulary, translated once.
---
--- Two callers bind an MCP server's tools onto a model request — the agent
--- block's `connect_mcp_servers` and knl_adapter's `ToolPort.mcp` — and both
--- need the same two translations: MCP's declaration into the one a request
--- carries, and an MCP call's content blocks into the text a tool_result
--- carries. Two copies of a NAMESPACE is the dangerous kind of duplicate:
--- the day they disagree, the same tool has two names and the model's call
--- finds neither.
---
--- They live in a module of their own rather than inside `llm_proto`
--- because they are not the wire format: nothing here builds a request or
--- parses a response, and a caller that binds MCP tools has no reason to
--- pull in a provider protocol. The module is registered in `host.rs`
--- (EMBEDDED_LIBS) so `require("mcp_tools")` resolves in the host — through
--- `include_tl!`, which is what makes a type error here a `cargo build`
--- error, and what strips these comments from the Lua the host embeds.
---
--- Nothing here touches the `mcp` bridge global: these are pure functions
--- over values a caller already has, so they load and run in a VM with no
--- MCP at all. The one global read is `std.json`, declared through
--- `host_types` (see that file for why the declaration assigns nothing).
require("host_types")

local record mcp_tools
    --- One item of `mcp.list_tools(server).tools`, as the server declared it.
    --- Both spellings of the schema key are accepted, and either may be
    --- absent.
    record Tool
        name: string
        description: string
        inputSchema: {string:any}
        input_schema: {string:any}
    end
    --- The neutral tool declaration a request carries. Built whole by
    --- `tool_decl`, so every field is set there, and the check says so.
    record Decl ---@struct
        name: string
        description: string
        input_schema: {string:any}
    end
    --- One block of `mcp.call(...).content`.
    record Block
        type: string
        text: string
    end
end

--- One `tools/list` entry as the neutral tool declaration a request carries.
---
--- MCP's private vocabulary is closed here: the `<server>__<tool>` name that
--- keeps two servers' tools apart, the camelCase `inputSchema` under the
--- snake_case name every adapter build reads, an empty description rather
--- than a missing one, and an empty object schema for a server that declared
--- none (a provider will reject a tool with no schema at all).
function mcp_tools.tool_decl(server: string, listed: mcp_tools.Tool): mcp_tools.Decl
    return {
        name = server .. "__" .. listed.name,
        description = listed.description or "",
        input_schema = listed.inputSchema or listed.input_schema or { type = "object", properties = {} },
    }
end

--- An MCP call's content blocks as the text a tool_result carries: a single
--- text block verbatim, no blocks the empty string, anything else (several
--- blocks, or one that is not text) JSON-encoded so nothing is dropped.
function mcp_tools.result_text(blocks: {mcp_tools.Block}): string
    blocks = blocks or {}
    if #blocks == 0 then
        return ""
    end
    local only = blocks[1]
    if #blocks == 1 and only and only.type == "text" then
        return only.text
    end
    return std.json.encode(blocks)
end

return mcp_tools
