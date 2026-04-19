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

### agent (StdPkg — `require("agent")`)

Built-in ReAct loop module. Available without any path configuration after `cargo install`.

```lua
local agent = require("agent")

local result = agent.run({
    prompt  = "List files in the current directory and summarise them.",
    system  = "You are a helpful assistant.",           -- optional
    model   = "claude-haiku-4-5-20251001",             -- optional, env ANTHROPIC_MODEL as fallback
    max_tokens       = 4096,                            -- per-request token limit
    max_iterations   = 20,                              -- loop iteration cap
    max_tokens_budget = 50000,                          -- total token budget (nil = unlimited)
    timeout          = 120,                             -- HTTP timeout in seconds
    mcp_servers = {                                     -- optional MCP servers to connect
        { name = "outline", command = "outline-mcp", args = {} },
    },
    -- Anthropic server-side context editing (default ON). Pass `false` to opt out,
    -- or pass a full override table (replaces the default entirely).
    context_management        = true,                   -- default true; false disables beta header + body
    context_management_config = {                       -- default: trigger 80K, keep 3, clear_at_least 10K
        edits = {
            {
                type           = "clear_tool_uses_20250919",
                trigger        = { type = "input_tokens", value = 80000 },
                keep           = { type = "tool_uses",    value = 3 },
                clear_at_least = { type = "input_tokens", value = 10000 },
            },
        },
    },
    on_turn = function(info)                            -- optional per-turn callback
        print("turn", info.turn_number, "#tools", #info.tool_calls)
        -- info.context_management is present only on turns where the server fired
        -- an edit; nil-guard before indexing applied_edits.
        if info.context_management and info.context_management.applied_edits then
            for _, edit in ipairs(info.context_management.applied_edits) do
                print("  edit:", edit.type, "cleared", edit.cleared_tool_uses, "tool_uses")
            end
        end
    end,
    extra_tools = {},                                   -- optional extra Anthropic tool defs
})

if result.ok then
    print(result.content)
else
    print("error:", result.error)
end
-- result fields: ok, content, usage{input_tokens,output_tokens,total_tokens}, num_turns, error, messages
```

Key behaviours:
- MCP servers listed in `mcp_servers` are connected automatically and disconnected on exit (even on error).
- MCP tool names are namespaced as `server_name__tool_name` to avoid collisions.
- Tool dispatch: MCP tools via `mcp.call()`, registered Lua tools via `tool.call()`.
- Never throws — all errors returned as `{ ok=false, error="..." }`.
- Context editing is on by default: once the conversation crosses ~80K input tokens, Anthropic evicts all but the most recent 3 tool-use / tool-result pairs server-side so the loop can keep running. Works on Sonnet 4 / Sonnet 4.5 / Haiku 4.5 / Opus 4 / 4.1 / 4.5. Pass `context_management = false` to disable, or `context_management_config = { edits = { ... } }` to replace the default entirely (the whole table is forwarded as `body.context_management`; no partial merge).
- `on_turn(info)` gains an additive `info.context_management` field that forwards the raw `response.context_management` from Anthropic (`{ applied_edits = { { type, cleared_tool_uses, cleared_input_tokens }, ... } }`). The field is absent on turns where the server did not fire any edit — nil-guard before indexing.
- The `blocks/` directory is embedded in the binary; place a local `blocks/agent/init.lua` in the project root to override.

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
