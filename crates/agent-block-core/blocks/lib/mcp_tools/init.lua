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
--- (EMBEDDED_LIBS) so `require("mcp_tools")` resolves in the host.
---
--- Nothing here touches the `mcp` bridge global: these are pure functions
--- over values a caller already has, so they load and run in a VM with no
--- MCP at all.

local M = {}

--- One `tools/list` entry as the neutral tool declaration a request carries.
---
--- MCP's private vocabulary is closed here: the `<server>__<tool>` name that
--- keeps two servers' tools apart, the camelCase `inputSchema` under the
--- snake_case name every adapter build reads, an empty description rather
--- than a missing one, and an empty object schema for a server that declared
--- none (a provider will reject a tool with no schema at all).
---
--- @param server string  the connected server's name
--- @param tool table  one item of `mcp.list_tools(server).tools`
--- @return table decl  { name, description, input_schema }
function M.tool_decl(server, tool)
    return {
        name = server .. "__" .. tool.name,
        description = tool.description or "",
        input_schema = tool.inputSchema or tool.input_schema or { type = "object", properties = {} },
    }
end

--- An MCP call's content blocks as the text a tool_result carries: a single
--- text block verbatim, no blocks the empty string, anything else (several
--- blocks, or one that is not text) JSON-encoded so nothing is dropped.
---
--- @param blocks table|nil  `mcp.call(...).content`
--- @return string text
function M.result_text(blocks)
    blocks = blocks or {}
    if #blocks == 1 and blocks[1].type == "text" then
        return blocks[1].text
    elseif #blocks == 0 then
        return ""
    end
    return std.json.encode(blocks)
end

return M
