# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `blocks/` StdPkg system: Lua modules embedded via `include_str!` are bundled into the binary and loadable with `require()`. File-system copies in `project_root/blocks/` or `exe_dir/blocks/` take precedence (hot-reload friendly). No path configuration required after `cargo install`.
- Generic Agent module (`require("agent")`): ReAct loop with MCP tool integration and dual budget control (`max_iterations` + `max_tokens_budget`). Connects to MCP servers, merges their tool schemas with registered Lua tools, dispatches `tool_use` responses, and returns a structured result `{ ok, content, usage, num_turns, error, messages }`.
- E2E tests and sample script for the agent module (`tests/e2e_agent.rs`, `tests/fixtures/agent_require.lua`, `examples/test_agent.lua`).

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
