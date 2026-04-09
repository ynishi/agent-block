# agent-block

Single-purpose agent building block with built-in mesh communication.

## What is agent-block?

A headless agent runtime. Each agent runs as a single process, executes its task, then exits. No rich interactive TUI, no sub-agent orchestration — orchestration belongs to the caller (shell, A2A, CI, etc.).

agent-block handles the infrastructure that individual agents shouldn't have to — mesh connectivity (A2A), MCP server management, LLM API access — so that Lua code focuses purely on domain logic.

Think of it like Envoy for agents: the process itself is simple, but the communication layer is fully capable.

## Design Decisions

- **Single run** — One process, one task, one exit. Orchestration belongs to the caller (shell, A2A, CI, etc.), not inside the agent
- **Headless** — No terminal UI. Agents are composed via A2A/mesh protocols, not interactive prompts
- **Runtime owns the protocol** — Mesh, MCP, and HTTP are provided by the runtime. Lua code never deals with connection management or wire formats
- **Lua for logic, Rust for plumbing** — Domain logic in Lua. VM, networking, and protocol handling in Rust

## Architecture

```text
┌─────────────────────────────────────────────┐
│              agent-block (binary)            │
│                                             │
│  ┌─────────┐  ┌──────────┐  ┌───────────┐  │
│  │ mlua-isle│  │ mesh-sdk │  │ llm-client│  │
│  │ (Lua VM) │  │ (relay)  │  │ (API)     │  │
│  └────┬─────┘  └────┬─────┘  └─────┬─────┘  │
│       │             │              │         │
│  ─────┴─────────────┴──────────────┴─────── │
│              Lua Stdlib Bridge               │
│  mesh.send / mesh.on / llm.chat / fs.read   │
│  tool.register / tool.call / log.* / env.*  │
│  mcp.connect / mcp.call / mcp.list_tools    │
└─────────────────────────────────────────────┘
         ↕ WebSocket              ↕ stdio
┌─────────────────┐    ┌──────────────────┐
│   agent-mesh     │    │  MCP Servers     │
│   relay          │    │  (outline-mcp)   │
└─────────────────┘    └──────────────────┘
```

## Usage

```sh
# Basic
agent-block --script scripts/hello.lua

# With project context
agent-block --script scripts/test_fcloop.lua --project .

# With mesh
ANTHROPIC_API_KEY=... agent-block --script my_agent.lua --relay ws://localhost:9090/ws
```

## Lua API

### llm.*
- `llm.chat(messages, opts)` — LLM call (Anthropic Messages API)

### tool.*
- `tool.register(name, schema, handler)` — Register a tool
- `tool.call(name, input)` — Call a registered tool
- `tool.list()` — List registered tool names
- `tool.schema()` — Anthropic tools-format schema array

### mcp.*
- `mcp.connect(name, command, args)` — Spawn MCP server + initialize handshake
- `mcp.call(name, tool_name, arguments)` — Call an MCP tool
- `mcp.list_tools(name)` — List available tools
- `mcp.disconnect(name)` — Disconnect server

### mesh.*
- `mesh.send(agent_id, payload)` — Synchronous send (raises Lua error on failure)
- `mesh.request(agent_id, payload)` — Request-response
- `mesh.agent_id()` — Own AgentId

### std.fs.* (mlua-batteries)
- `std.fs.read(path)`, `std.fs.write(path, content)`, `std.fs.glob(pattern)`, `std.fs.exists(path)`
- `std.fs.walk(dir)`, `std.fs.copy(src, dst)`, `std.fs.mkdir(path)`, `std.fs.remove(path)`
- `std.fs.is_file(path)`, `std.fs.is_dir(path)`, `std.fs.read_binary(path)`, `std.fs.write_binary(path, bytes)`

### sh.*
- `sh.exec(cmd, opts)` — Execute a shell command

### std.json.* (mlua-batteries)
- `std.json.encode(value)`, `std.json.decode(str)`, `std.json.encode_pretty(value)`

### std.env.* (mlua-batteries + agent-block extensions)
- `std.env.get(key)`, `std.env.set(key, value)`, `std.env.get_or(key, default)`, `std.env.home()`
- `std.env.agent_id()`, `std.env.project_root()` — agent-block specific

### std.path.* / std.time.* (mlua-batteries)
- `std.path.join(...)`, `std.path.basename(path)`, `std.path.dirname(path)`
- `std.time.now()`, `std.time.sleep(secs)`, `std.time.measure(fn)`

### log.*
- `log.info/warn/error/debug(msg)`

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
