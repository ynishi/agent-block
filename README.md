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

## MCP Echo Harness

A self-contained reference MCP server for smoke-testing the agent-block MCP client bridge.
Exposes tools, resources, prompts, logging, and sampling over stdio or HTTP.

```sh
# stdio (default) — connect via mcp.connect("echo", "target/debug/examples/echo_mcp_server", {})
cargo run --example echo_mcp_server

# HTTP on an ephemeral port — prints ECHO_MCP_URL=http://127.0.0.1:<port>/mcp
cargo run --example echo_mcp_server -- --transport http --port 0

# Also emit 5 log notifications (1-second intervals) and attempt a sampling round-trip
cargo run --example echo_mcp_server -- --transport http --port 0 --emit-logs --request-sampling
```

Verify from Lua (requires the server to be running with `--transport http`):

```lua
local url = os.getenv("ECHO_MCP_URL")
mcp.connect_http("echo", url)
print(mcp.list_tools("echo"))         -- 2 tools: echo, slow_echo
print(mcp.list_resources("echo"))     -- 2 resources: text://hello, text://note
print(mcp.list_prompts("echo"))       -- 1 prompt: greet
-- call slow_echo to exercise progress notifications
mcp.on_progress("echo", function(tok, prog, total, msg)
    print("progress", prog, total, msg)
end)
print(mcp.call("echo", "slow_echo", { msg = "hi", steps = 3 }))
```

See `examples/verify_echo_harness.lua` for the full verification script.

## Lua API

### llm.*
- `llm.chat(messages, opts)` — LLM call (Anthropic Messages API)

### tool.*
- `tool.register(name, schema, handler)` — Register a tool
- `tool.call(name, input)` — Call a registered tool
- `tool.list()` — List registered tool names
- `tool.schema()` — Anthropic tools-format schema array

### mcp.*
- `mcp.connect(name, command, args)` — Spawn MCP server over stdio + initialize handshake
- `mcp.connect_http(name, url, opts)` — Connect to an MCP server over HTTP transport.
  `opts.transport = "sse" | "http"` (default `"http"` = Streamable HTTP; `"sse"` = SSE).
  `opts.headers` table is forwarded as request headers.
- `mcp.call(name, tool_name, arguments)` — Call an MCP tool
- `mcp.list_tools(name)` — List available tools
- `mcp.list_resources(name)` — List resources exposed by the server.
  Returns `{ ok=true, resources=[{uri, name, description, mimeType, ...}] }`.
- `mcp.read_resource(name, uri)` — Read a resource by URI.
  Returns `{ ok=true, contents=[{uri, mimeType, text|blob}] }`.
- `mcp.list_prompts(name)` — List prompt templates exposed by the server.
  Returns `{ ok=true, prompts=[{name, description, arguments}] }`.
- `mcp.get_prompt(name, prompt_name, args)` — Retrieve a rendered prompt template.
  Returns `{ ok=true, description, messages=[{role, content}] }`.
- `mcp.on_progress(name, handler)` — Register a per-server progress notification callback.
  `handler(token, progress, total, message)` is called for each `notifications/progress`
  event from the named server. Handler must be a pure Lua function.
- `mcp.on_log(name, handler)` — Register a per-server log notification callback.
  `handler(level, logger, data)` is called for each `notifications/message` event from
  the named server. When no handler is registered the notification is forwarded to the
  Rust `tracing` target `"lua"` at the corresponding level (debug/info/notice/warning/
  error/critical/alert/emergency). Handler must be a pure Lua function.
- `mcp.cancel(name, request_id)` — Send a `notifications/cancelled` notification to the
  named server for the given `request_id`. Also fired automatically when `mcp.call` times
  out. Explicit use is only needed for manual cancellation flows.
- `mcp.set_sampling_handler(server_name, handler)` — Register a per-server Lua function
  to respond to `sampling/createMessage` requests from the MCP server.
  `handler(params)` receives the `CreateMessageRequest` table and must return a table
  matching `CreateMessageResult` (`{ model, stop_reason, role, content }`).
  When no handler is registered the server receives `method_not_found`.
- `mcp.server_info(name)` — Return the server's `InitializeResult` as a Lua table.
  Returns `{ ok=true, server_info={serverInfo, capabilities, ...} }` on success.
  Useful for inspecting which MCP capability groups (resources, prompts, tools, etc.)
  a server declares. Returns `{ ok=false, error="..." }` if the server is not connected.
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
        -- HTTP/SSE form: use `url` instead of `command`
        { name = "remote", url = "https://example.com/mcp",
          transport_opts = { transport = "sse" } },     -- transport = "sse" | "http" (default "http")
    },
    sampling = function(params) ... end,                -- optional: called for sampling/createMessage
                                                        -- from every connected MCP server
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

**Provider Switching**

By default `agent.run` uses the Anthropic Messages API. Pass `provider = "openai"` to route to any OpenAI-compatible endpoint (vLLM, llama.cpp, OpenRouter, RunPod, etc.):

```lua
-- Anthropic (default) — requires ANTHROPIC_API_KEY
local result = agent.run({ prompt = "Hello", model = "claude-haiku-4-5-20251001" })

-- OpenAI — requires OPENAI_API_KEY (or opts.api_key)
local result = agent.run({
    prompt  = "Hello",
    provider = "openai",
    model   = "gpt-4o-mini",
})

-- Local vLLM / llama.cpp / RunPod — custom base_url
local result = agent.run({
    prompt   = "Hello",
    provider = "openai",
    base_url = "http://localhost:8080/v1",
    model    = "Qwen/Qwen3-0.6B",
    api_key  = "token-abc123",           -- or api_key_env = "MY_KEY"
})
```

Environment variables used per provider:

| provider     | default key env     | override via           |
|--------------|---------------------|------------------------|
| `anthropic`  | `ANTHROPIC_API_KEY` | `opts.api_key` / `opts.api_key_env` |
| `openai`     | `OPENAI_API_KEY`    | `opts.api_key` / `opts.api_key_env` |

`opts.base_url` overrides the endpoint root. Default for `openai` is `https://api.openai.com/v1`.

`cache_control`, `context_management`, and `context_management_config` are Anthropic-only: they are operative when `provider="anthropic"` (or unset) and emit a `warn`-level log message then are ignored when `provider="openai"`.

Key behaviours:
- MCP servers listed in `mcp_servers` are connected automatically and disconnected on exit (even on error).
- Each entry may use the stdio form `{ name, command, args }` or the HTTP form `{ name, url, transport_opts }`. Both forms can coexist in the same list.
- Pass `sampling = fn` in `agent.run` opts to register a single Lua function as the `sampling/createMessage` handler for every connected MCP server (`mcp.set_sampling_handler` is called per server automatically).
- Pass `enable_resources = true` in `agent.run` opts to automatically register `{server}__mcp_list_resources` and `{server}__mcp_read_resource` as LLM-callable tools for each connected server that declares the `resources` capability. Default `false`. If a server does not declare `resources`, the opt-in is silently skipped (logged at `info`).
- Pass `enable_prompts = true` in `agent.run` opts to automatically register `{server}__mcp_list_prompts` and `{server}__mcp_get_prompt` as LLM-callable tools for each connected server that declares the `prompts` capability. Default `false`. Capability check and silent skip apply the same way as `enable_resources`.
- Pass `on_progress = fn(ev)` in `agent.run` opts to receive progress notifications from all connected MCP servers. The callback is called with an envelope table `{ type="progress", server, token, progress, total, message }`. No capability gate — all servers are registered. User callback errors are swallowed and logged at `warn`.
- Pass `progress_to_log = true` in `agent.run` opts to bridge progress notifications to `log.info` automatically. Ignored when `on_progress` is also set (callback takes priority). Default `false`.
- Pass `on_log = fn(ev)` in `agent.run` opts to receive log notifications from servers that declare the `logging` capability. The callback is called with an envelope table `{ type="log", server, level, logger, data }`. Servers without logging capability are silently skipped (logged at `info`). User callback errors are swallowed and logged at `warn`.
- Pass `log_to_stderr = true` in `agent.run` opts to bridge server log notifications to `log.debug|info|warn|error` automatically. Ignored when `on_log` is also set (callback takes priority). Logging capability gate applies the same way as `on_log`. Default `false`.
- MCP tool names are namespaced as `server_name__tool_name` to avoid collisions.
- Tool dispatch: MCP tools via `mcp.call()`, registered Lua tools via `tool.call()`.
- Never throws — all errors returned as `{ ok=false, error="..." }`.
- Context editing is on by default: once the conversation crosses ~80K input tokens, Anthropic evicts all but the most recent 3 tool-use / tool-result pairs server-side so the loop can keep running. Works on Sonnet 4 / Sonnet 4.5 / Haiku 4.5 / Opus 4 / 4.1 / 4.5. Pass `context_management = false` to disable, or `context_management_config = { edits = { ... } }` to replace the default entirely (the whole table is forwarded as `body.context_management`; no partial merge).
- `on_turn(info)` gains an additive `info.context_management` field that forwards the raw `response.context_management` from Anthropic (`{ applied_edits = { { type, cleared_tool_uses, cleared_input_tokens }, ... } }`). The field is absent on turns where the server did not fire any edit — nil-guard before indexing.
- The `blocks/` directory is embedded in the binary; place a local `blocks/agent/init.lua` in the project root to override.
- LLM dump logging is safe-by-default and ENV-driven:
  - `AGENT_BLOCK_LLM_DUMP=off|meta|full` (default `off`)
  - when unset, `RUST_LOG` containing `debug` or `trace` enables `meta`
  - `full` is downgraded to `meta` when `AGENT_BLOCK_ENV=prod|production` unless `AGENT_BLOCK_LLM_DUMP_ALLOW_PROD=true`
  - request auth headers (`x-api-key` / `authorization`) are always redacted in dump logs
  - log lines use fixed-order `key=value` format with a unique marker (`prefix=ab.obs component=llm`); legacy `prefix=ab.llm` lines are also emitted for compatibility
  - `meta` includes call correlation and runtime signals (`call`, `turn`, `iter`, `latency_ms`, `stop_reason`, `tool_uses`, token usage, context edit count)
  - optional `agent.run({ log_meta = { trace_id, agent_id, agent_name, run_id } })` appends external context to dump lines (same keys can also come from `AGENT_BLOCK_TRACE_ID`, `AGENT_BLOCK_AGENT_ID`, `AGENT_BLOCK_AGENT_NAME`, `AGENT_BLOCK_RUN_ID`)

### compile_loop (Filesystem block — `require("compile_loop")`)

Tool factory for the autonomous compile-and-fix loop. The primary surface is
`compile_loop.make(conf)`, which returns a `tool_def` consumable directly by `agent.run`.

Place `blocks/compile_loop/init.lua` in the project root (resolved via the filesystem
`blocks/` path; no `EMBEDDED_BLOCKS` entry is required).

```lua
local compile_loop = require("compile_loop")
local agent        = require("agent")

-- Define a caller-supplied runner function
local function lua_runner(file_path)
    local p = io.popen("lua " .. file_path .. ' 2>&1; echo "__EXIT__=$?"', "r")
    if not p then return { ok = false, stdout = "", stderr = "popen failed", exit_code = -1 } end
    local out = p:read("*a") or ""
    p:close()
    local exit_code = tonumber(out:match("__EXIT__=(%d+)%s*$") or "1")
    out = out:gsub("__EXIT__=%d+%s*$", "")
    local pass = exit_code == 0 and out:find("ALL_PASS", 1, true) ~= nil
    return { ok = pass, stdout = out, stderr = "", exit_code = exit_code }
end

-- Build a tool_def and pass it to the parent agent
local td = compile_loop.make({
    runner    = lua_runner,       -- required: function(path) → {ok, stdout, stderr, exit_code}
    max_iters = 5,                -- optional, default 5
    lang      = "lua",            -- optional, default "lua"
    -- conf.llm is optional: when omitted the parent agent's provider/model/api_key
    -- are inherited at call time via _AGENT_LLM_CTX (Crux #2).
    llm = {
        provider = "anthropic",
        model    = "claude-haiku-4-5-20251001",
        -- api_key / api_key_env / base_url / max_tokens / temperature / timeout
    },
})

local result = agent.run({
    prompt      = "Write a Lua function that returns the nth Fibonacci number.",
    model       = "claude-haiku-4-5-20251001",
    extra_tools = { td },         -- tool_def passed directly; no caller-side adaptation
})
```

**`compile_loop.make(conf)`** returns `{ name, schema, handler }`. As a side-effect
`tool.register(name, schema, handler)` is called, so the registry and `tool_def.handler`
share the same function identity. The tool name defaults to `"compile_loop"`; pass
`conf.name` to override (useful when registering multiple instances).

**Tool input** (supplied by the LLM at call time): `spec` (string, required),
`target_file` (absolute path, required), `lang` (string, optional).

**Tool output JSON** (never contains `code` or `history` — Counter WF-A defence):

```
{ ok, iters, summary, failure_reason?, last_error?, artifact_path }
```

`failure_reason` values: `"llm_call"` | `"open_target_file"` | `"stagnation"` | `"max_iters"`.

**LLM inheritance (Crux #2)**: when `conf.llm` is omitted (or individual fields are absent),
`compile_loop` resolves `provider`, `base_url`, `api_key`, `api_key_env`, and `model` from
the parent `agent.run` call context at tool-dispatch time. No hardcoded provider default;
no error for missing credentials at `make()` time.

**Stagnation detection**: when 3 consecutive iterations produce identical runner `stderr`
the loop gives up immediately, independent of the remaining iteration budget.
`failure_reason = "stagnation"`.

**Provider support**: `"anthropic"` and `"openai"`-compatible endpoints (vLLM, llama.cpp,
OpenRouter, RunPod, etc.) are both fully implemented in `conf.llm`.

| `conf.llm.provider` | Default key env     | Override via                        |
|---------------------|---------------------|-------------------------------------|
| `"anthropic"`       | `ANTHROPIC_API_KEY` | `conf.llm.api_key` / `api_key_env`  |
| `"openai"`          | `OPENAI_API_KEY`    | `conf.llm.api_key` / `api_key_env`  |

### coding_agent (Filesystem block — `require("coding_agent")`, thin facade)

Backward-compatible facade over `compile_loop`. Prefer the `compile_loop.make()` API for
new code. `coding_agent` is retained for existing callers.

Place `blocks/coding_agent/init.lua` in the project root.

**`coding_agent.run(opts)`** — run the loop directly from Lua (facade over `compile_loop`).

```lua
local coding = require("coding_agent")

local res = coding.run({
    provider    = "anthropic",                    -- "openai" | "anthropic"
    api_key     = "...",                          -- or api_key_env = "ANTHROPIC_API_KEY"
    model       = "claude-haiku-4-5-20251001",
    target_file = "/tmp/work/solution.lua",
    spec        = "Write a Lua function that returns the nth Fibonacci number.",
    lang        = "lua",                          -- code fence label (default "lua")
    max_iters   = 5,
    runner      = function(file_path)
        -- return { ok=bool, stdout, stderr, exit_code }
        local p = io.popen("lua " .. file_path .. " 2>&1; echo __EXIT__=$?", "r")
        local out = p:read("*a"); p:close()
        local ec = tonumber(out:match("__EXIT__=(%d+)") or "1")
        return { ok = ec == 0, stdout = out, stderr = "", exit_code = ec }
    end,
    on_iter = function(info) print("iter", info.iter, info.result.ok) end,
})

-- res fields:
--   ok             boolean
--   artifact_path  string      absolute path of the target file
--   iters          int
--   summary        string      "PASS in N iters" or "give-up: <reason>"
--   failure_reason string?     "llm_call"|"open_target_file"|"stagnation"|"max_iters"
--   last_error     string?     last runner stderr (trimmed to 800 chars) on failure
--
-- NOTE: "code" and "history" fields are no longer returned (removed in this release).
```

**`coding_agent.register_tool(opts)`** — register the `compile_loop` tool with the host
tool registry so a parent LLM can invoke it via `tool.call`. Returns the registered tool name.

```lua
local coding = require("coding_agent")

-- Register once (typically at agent startup)
coding.register_tool({
    provider    = "openai",
    base_url    = "http://localhost:8080/v1",
    api_key     = "...",
    model       = "Qwen/Qwen2.5-Coder-7B",
    runner_kind = "lua",    -- "lua" | "cargo" | runner function
    max_iters   = 5,
    lang        = "lua",
})

-- The parent LLM can now call the "compile_loop" tool with:
--   { spec = "...", target_file = "/abs/path/to/file.lua", lang = "lua" }
-- The tool response JSON contains: ok, artifact_path, iters, summary,
--   failure_reason?, last_error?   (code and history are excluded).
```

Built-in `runner_kind` values (resolved in the `coding_agent` facade; `compile_loop` itself
accepts only a runner function):

| `runner_kind` | Behaviour |
|---------------|-----------|
| `"lua"`       | Runs `lua <file>` and passes on exit 0 + `ALL_PASS` in stdout |
| `"cargo"`     | Runs `cargo test --offline` in the file's directory; passes on `"test result: ok"` |
| function      | Called as `runner(file_path)` — must return `{ ok, stdout, stderr, exit_code }` |

### lshape (Vendored package — `require("lshape")`)

`lshape` is vendored under `blocks/lshape/` so scripts can use schema validation
and LuaCATS generation without external installation.

```lua
local lshape = require("lshape")
local T = lshape.t
local User = T.shape({ name = T.string, age = T.number })
local ok, why = lshape.check.check({ name = "Ada", age = 36 }, User)
assert(ok, why)
```

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
