# MCP Support Status

This document tracks what `agent-block` supports as an **MCP client**, and
records the design position on topics where the MCP spec itself is still
moving (most notably **tool grouping**). The Lua-facing API reference lives
in `README.md` (`mcp.*` section); this document is the status/rationale view.

SDK: `rmcp` 1.7.0. Transports: stdio (`mcp.connect`) and Streamable
HTTP / SSE (`mcp.connect_http`).

## Capability matrix

| MCP area | Status | Entry points |
|---|---|---|
| Tools (list / call) | Supported | `mcp.list_tools` / `mcp.call`, auto-exposed to `agent.run` |
| Tools list_changed | Supported (callback) | `mcp.on_tools_list_changed` |
| Resources (list / read / templates) | Supported | `mcp.list_resources` / `mcp.read_resource` / `mcp.list_resource_templates` |
| Resources subscribe / updated | Supported (capability-gated) | `mcp.subscribe_resource` / `mcp.on_resource_update` |
| Resources as LLM tools | Opt-in | `agent.run` `enable_resources = true` → `{server}__mcp_list_resources` etc. |
| Prompts (list / get) | Supported | `mcp.list_prompts` / `mcp.get_prompt` |
| Prompts as LLM tools | Opt-in | `agent.run` `enable_prompts = true` |
| Completion (typeahead) | Supported | `mcp.complete` |
| Progress notifications | Supported | `mcp.on_progress`, `agent.run` `on_progress` |
| Logging notifications | Supported (fallback to `tracing`) | `mcp.on_log` |
| Cancellation | Supported (auto on timeout) | `mcp.cancel` |
| Sampling (server→client) | Supported (per-server handler) | `mcp.set_sampling_handler` |
| Elicitation (server→client) | Form variant only (Url declined) | `mcp.set_elicitation_handler` |
| Roots (server→client) | Supported (per-server handler) | `mcp.set_roots_handler` / `mcp.notify_roots_list_changed` |
| Ping / keepalive | Supported | `mcp.ping` |
| Server info / capabilities | Supported | `mcp.server_info` |
| Tasks (experimental, 2025-11-25) | Not supported | — |

## Tool naming and routing

Tools from a connected server are exposed to the LLM as
`{server}__{tool}` (double underscore). `dispatch_tool` parses the prefix
and routes the call back to the owning server.

This is the de-facto ecosystem pattern for MCP aggregation — the protocol
has no first-class namespace, so the group identifier is embedded at the
head of the tool name and parsed for routing:

- MetaMCP: `ServerName__tool` (nested: `Outer__Inner__tool`)
- Docker MCP Gateway: `server:toolname` (colon variant; conflicts with the
  spec name pattern `^[a-zA-Z0-9_-]{1,64}$`)
- Claude Code: `mcp__server__tool`, with `mcp__server__*` permission globs

`agent-block` uses the underscore variant, which stays inside the spec
name pattern.

## Grouping

### Spec status (as of 2026-06)

The MCP spec has **no first-class group concept**. The relevant history:

- **SEP-986** (adopted, 2025-11-25 naming guidance): multi-tool servers
  should prefix tool names to group related functionality
  (e.g. `github_list_repos`). Name prefix is the only normalized grouping
  mechanism in the spec.
- **SEP-2084 "Primitive Grouping"** (top-level `groups` property + Group
  primitive): **rejected 2026-02** — maintainers consider standardization
  premature while tool-search ecosystems evolve.
- **SEP-1300** (`groups` / `tags` on tool definitions + `tools/list`
  filter): rejected, same family.
- **experimental-ext-grouping** (official org, Interest Group): active
  exploration, wire format not settled.

Anthropic Messages API tool definitions likewise have no `group` field;
unknown fields are rejected with HTTP 400
(`tools.N.custom.group: Extra inputs are not permitted`).

### agent-block design: two layers

Grouping in `agent-block` is **client-side only** — nothing group-related
is sent to the LLM API or required from servers.

1. **Server-level group (automatic).** Every MCP tool is assigned
   `group = <server name>`, alongside the `{server}__` name prefix.
   `agent.run` `opts.tool_groups = {...}` then includes/excludes MCP tools
   per server via the standard `build_tools()` filter — the same mechanism
   used for `tool.register(name, schema, handler, { group = "..." })`.
   Functionally equivalent to Claude Code's `mcp__server__*` globs.
2. **Server-declared group (override).** If a tool carries a non-empty
   string at `_meta.group` (rmcp serializes `Tool.meta` as `_meta`), it
   takes precedence over the server name. This lets a large server split
   its tools into categories (resolution: `M._resolve_mcp_group`).

The `group` field is **filter-internal**: `build_tools()` strips it from
the emitted tool defs, because the Anthropic API rejects unknown fields
(see above). Tools without any group belong to the implicit `"default"`
group — note that MCP tools therefore no longer match
`tool_groups = {"default"}`.

### Interim convention and reopen trigger

`_meta.group` (plain string) is an agent-block ecosystem convention, not a
spec key. When experimental-ext-grouping (or a successor SEP) lands a
first-class wire format, add it as an accepted alias in
`M._resolve_mcp_group` and prefer the spec key over the convention key.

## Related

- `README.md` — `mcp.*` Lua API reference, `agent.run` MCP options,
  tool group usage
- `docs/runbooks/e2e-mcp-resource-subscribe.md` — resource subscribe E2E
- `blocks/agent/init.lua` — `connect_mcp_servers` / `build_tools` /
  `dispatch_tool` / `M._resolve_mcp_group`
- MCP spec: https://modelcontextprotocol.io/specification/2025-11-25
- SEP-986: https://modelcontextprotocol.io/seps/986-specify-format-for-tool-names
- SEP-2084 (rejected): https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2084
- experimental-ext-grouping: https://github.com/modelcontextprotocol/experimental-ext-grouping
