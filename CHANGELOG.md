# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-04-15

### Added

- `blocks/` StdPkg system: Lua modules embedded via `include_str!` are bundled into the binary and loadable with `require()`. File-system copies in `project_root/blocks/` or `exe_dir/blocks/` take precedence (hot-reload friendly). No path configuration required after `cargo install`.
- Generic Agent module (`require("agent")`): ReAct loop with MCP tool integration and dual budget control (`max_iterations` + `max_tokens_budget`). Connects to MCP servers, merges their tool schemas with registered Lua tools, dispatches `tool_use` responses, and returns a structured result `{ ok, content, usage, num_turns, error, messages }`.
- E2E tests and sample script for the agent module (`tests/e2e_agent.rs`, `tests/fixtures/agent_require.lua`, `examples/test_agent.lua`).
- `--mcp-timeout-secs` CLI flag for per-RPC MCP timeout, applied uniformly to `connect` / `list_tools` / `call_tool` / `disconnect`.
- `tracing::warn!` on every MCP error path (spawn / initialize / list_tools / call_tool / disconnect — timeout and protocol failures alike) so autonomous runs leave a Rust-side log trail in addition to the Lua-visible error. Structured fields include `server`, `tool`, `timeout`, `error`.
- E2E regression tests for the "autonomous-agent visibility" contract: CLI must reject `--mcp-timeout-secs 0` at parse time; MCP timeouts and unknown-server errors must propagate to Lua AND emit `tracing::warn!` (`tests/e2e_mcp.rs`, `tests/fixtures/mcp_errors.lua`).

### Changed

- MCP client (`src/mcp_client.rs`) migrated from a bespoke JSON-RPC stdio implementation to [rmcp](https://crates.io/crates/rmcp) 1.4.x. The `McpServer` struct and hand-rolled request/response loop are replaced by `RunningService<RoleClient, ()>` from rmcp. The `()` unit type provides the default `ClientHandler`, which returns `method_not_found` for `sampling/createMessage` (Sampling API not advertised).
- `McpManager::call_tool` replaces the generic `call(method, params)` method. The Lua-visible API (`mcp.connect`, `mcp.list_tools`, `mcp.call`, `mcp.disconnect`) is unchanged.
- `mcp.list_tools` now uses `list_all_tools()` internally, which handles cursor-based pagination automatically.
- `mcp.call` and `mcp.list_tools` now return rmcp typed results serialised via `serde_json::to_value`, preserving the existing Lua JSON shape (`tools[].name`, `tools[].inputSchema`, `content[].type`, `content[].text`).
- `mcp.call` return table now includes `is_error` (mirrors MCP `isError`) and optional `structured_content` (mirrors MCP `structuredContent`). `ok` remains reserved for transport / protocol / timeout failures; tool-execution errors are passed through so the LLM can self-correct, matching the MCP 2025-06-18 spec intent. `blocks/agent` ReAct loop now forwards `is_error` to the Anthropic `tool_result` block.
- `McpManager::call_tool` now validates that `arguments` is a JSON object (or `Null` for "no arguments") up-front; arrays/scalars are rejected with a clear error rather than being silently dropped.
- `McpManager::disconnect` now reuses the configured `rpc_timeout` for the cancel round-trip (removed the separately hardcoded 5s `CANCEL_TIMEOUT`). `disconnect_all` logs 2nd-and-later errors at `warn` level instead of discarding them silently.
- `mcp.connect` argv iteration now uses integer indices (`1..=len`) instead of `pairs`, guaranteeing argument order regardless of Lua table layout.
- Internal manager concurrency primitive switched from `tokio::sync::Mutex<McpManager>` to `tokio::sync::RwLock<McpManager>`. `list_tools` / `call_tool` take `&self` so concurrent RPCs — including against the same server — proceed in parallel under read guards; `connect` / `disconnect` take the write guard. Per-server request multiplexing is delegated to rmcp's `Peer`. Covered by in-process concurrency tests.
- **Breaking (Rust API)**: `McpManager::with_rpc_timeout` now returns `BlockResult<Self>` and rejects `Duration::ZERO`. A zero timeout would silently turn every `tokio::time::timeout` into an immediate error; for an autonomous agent that "everything broken" mode must surface at construction, not at the first RPC. The CLI flag is additionally range-checked by clap (`value_parser!(u64).range(1..)`) so the misconfig fails at parse time. The Lua-visible API is unchanged.

## [0.2.0] - 2026-04-10

### Added

- `llm.*` bridge: `llm.extract_json(text)`, `llm.strip_fences(text)` via llm-extract
- `.env` file loading via dotenvy for API key management
- `.env.example` template for quick setup

### Changed

- Moved sample scripts from `scripts/` to `examples/`

## [0.1.0] - 2026-04-10

### Added

- Lua-first async agent runtime built on mlua-isle
- Bridge modules: HTTP, MCP, Mesh, Tool, Shell, Log
- MCP client manager for connecting to external MCP servers
- AgentMesh integration for inter-agent communication
- CLI interface with `clap` for script execution
- Dual license: MIT OR Apache-2.0
