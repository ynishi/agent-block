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

### Design documentation lives in the code

Settled design is written into the code's own documentation and nowhere else:
Rust crate and module docs (`//!`) plus item docs, and the `---` module headers of
the embedded Lua libraries. There is no separate design document tree — a document
beside the code drifts and goes unread, a module doc is compiled, linted and read
with the code it describes.

The entry points for the kernel are the module docs of
`crates/agent-block-core/src/knl/mod.rs` (the kernel's invariants),
`crates/agent-block-core/src/bridge/knl.rs` (the syscall surface Lua sees) and
`crates/agent-block-core/blocks/lib/knl/init.lua` (the Lua kernel: session, device,
beat, Outcome, shapes). `cargo doc --open` renders the Rust side.

## Architecture

The repository is a Cargo workspace with 4 crates (strict one-way
dependency `bin → core → mcp → types`):

| Crate | Role | Deps |
|---|---|---|
| `agent-block-types`  | shared `error` + `obs` (sanitize_url 等) | leaf |
| `agent-block-mcp`    | rmcp wrapper + Lua↔JSON converters | types |
| `agent-block-core`   | host runtime + Lua stdlib bridge + EventBus | mcp, types |
| `agent-block` (bin)  | thin CLI on top of `core` | core, mcp |

Downstream Rust applications can depend on `agent-block-core` (or just
`agent-block-types` for error/obs) without pulling in clap / the CLI.

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

## Installation

```sh
# From crates.io
cargo install agent-block

# Prebuilt binaries (GitHub Releases, built by cargo-dist for
# linux x86_64 / macOS x86_64 + aarch64 / windows x86_64).
# Handy in CI where a cargo build is too slow:
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/ynishi/agent-block/releases/latest/download/agent-block-installer.sh | sh
```

Windows PowerShell: use `agent-block-installer.ps1` from the same release.
Per-platform archives (`agent-block-<target>.tar.xz` / `.zip`) with sha256
checksums are attached to each release for direct download.

## Usage

```sh
# Basic
agent-block --script crates/agent-block/examples/hello.lua

# A registered block, by name (see "Blocks and libraries" below)
agent-block --block summarize --prompt "Summarise the README"

# With project context
agent-block --script scripts/test_fcloop.lua --project .

# With mesh
ANTHROPIC_API_KEY=... agent-block --script my_agent.lua --relay ws://localhost:9090/ws

# Pass a prompt and system context from the CLI
agent-block --script my_agent.lua \
    --prompt "Summarise the README" \
    -c "You are a concise technical writer."
```

CLI flags `--prompt` and `-c / --context` inject the `_PROMPT` and `_CONTEXT` Lua globals
into the script. Use them with `agent.run`:

```lua
-- my_agent.lua
local agent = require("agent")
local result = agent.run({
    prompt = _PROMPT,    -- nil when --prompt is omitted (agent.run will error — expected)
    system = _CONTEXT,   -- nil when -c is omitted (system prompt is optional)
})
print(result.content)
```

Both flags also accept environment variables as fallback:

| Flag | Env var |
|---|---|
| `--prompt` | `AGENT_BLOCK_PROMPT` |
| `-c / --context` | `AGENT_BLOCK_CONTEXT` |

## Blocks and libraries

Two directories, two jobs, at two tiers:

```text
<project>/blocks/<name>.lua | <name>/init.lua   entry points — run by name
<project>/lib/<name>.lua    | <name>/init.lua   modules     — require("<name>")
~/.agent-block/blocks/…                          the user's blocks, every project
~/.agent-block/lib/…                             the user's modules, every project
                                                 (`$AGENT_BLOCK_HOME` moves the pair)
```

A **block** is a script the host runs for the one value it returns. It is
callable by name — `agent-block --block <name>` from a shell, `run_block` over
MCP — and the two surfaces share one registry: `<project>/blocks/` then
`$AGENT_BLOCK_HOME/blocks/`, the project winning a clash. A **module** is what
a block `require`s: `script_dir` → `<project>/lib/` → `$AGENT_BLOCK_HOME/lib/`
→ embedded, first hit wins. Nothing crosses: a file in `blocks/` cannot be
required, a file in `lib/` is never run by name, so a helper dropped beside a
block does not become a callable block by accident.

The working shape is a script that grows a library:

```text
blocks/summarize.lua              write it, run it: agent-block -b summarize
lib/summarize_util.lua            pull the reusable part out; require("summarize_util")
~/.agent-block/lib/summarize_util.lua   mv it here when a second project wants it
crates/agent-block-core/blocks/lib/…    upstream, once it is general (EMBEDDED_LIBS)
```

The file and its `require` name never change on the way up; each tier
resolves the same name. `--project` names the project root only (`.env`, the
sandbox write root, the kernel database) — it is not a library path.

## Serving blocks over MCP

`agent-block mcp` serves the registered blocks to an MCP client as one
`run_block` tool. The same scripts the CLI runs become callable from an agent
that speaks MCP, without that agent knowing where they live:

```sh
agent-block mcp --project .
```

```json
{
  "mcpServers": {
    "agent-block": {
      "command": "agent-block",
      "args": ["mcp", "--project", "/abs/path/to/project"]
    }
  }
}
```

That serves `<project>/blocks/` and `~/.agent-block/blocks/`; `--block-dir
<dir>` (repeatable) adds a directory that lives elsewhere.

A block runs against its own model with its own credentials, so its LLM turns
never enter the calling agent's context — only its return value does. That is
the point of the mode: a caller strong at planning and review hands the loop
off and gets one value back.

**The contract is one line: a block returns a JSON string.**

```lua
-- blocks/summarize.lua
-- Summarize the prompt. Returns { ok, text }.
local agent = require("agent")
local r = agent.run({ prompt = _PROMPT, system = _CONTEXT })
return std.json.encode({ ok = r.ok, text = r.text })
```

The host stringifies whatever the chunk evaluates to, and a Lua table
stringifies to `table: 0x…` — hence `std.json.encode`. `_PROMPT` / `_CONTEXT`
carry the tool's `prompt` / `context` arguments, exactly as the CLI flags do.

| Surface | Meaning |
|---|---|
| tool `run_block` | run one registered block; `block` is an enum of the registered names |
| resource `agent-block://guide` | the authoring contract, in full |
| resource `agent-block://blocks` | the registry as JSON |
| resource `agent-block://blocks/<name>` | one block's source |

A block is `<name>.lua` or `<name>/init.lua` directly inside a block root;
nothing deeper, and nothing under `lib/`, is callable. The roots are re-scanned
per request — a new block is callable as soon as the file lands, without
restarting the server. A block's leading `--` comment is what the caller reads
as its description, so it is worth writing.

A Lua error comes back as a failed tool call carrying the message. A block that
ran and concluded "no" should return normally and say so in its JSON: the two
are different events and only the block knows which happened.

Because stdio transport owns stdout, this mode writes its logs to stderr, which
is where MCP clients surface server logs — including the `ab.obs` lines.

## Sandbox mode (Linux)

`--sandbox` wraps the whole process in an OS-level execution boundary built from
[Landlock](https://landlock.io/) (filesystem + TCP) and a seccomp filter (io_uring).
It is off by default.

```sh
# Confine writes; TCP stays open
agent-block --sandbox --script my_agent.lua --project .

# Same, plus extra writable paths and no TCP at all
AGENT_BLOCK_SANDBOX_FS_RW=/opt/cache:/srv/data \
AGENT_BLOCK_SANDBOX_TCP=0 \
    agent-block --sandbox --script my_agent.lua
```

| Flag / Env var | Default | Effect |
|---|---|---|
| `--sandbox` / `AGENT_BLOCK_SANDBOX` | off | Install the boundary at startup |
| `AGENT_BLOCK_SANDBOX_FS_RW` | *(empty)* | `:`-separated extra writable paths |
| `AGENT_BLOCK_SANDBOX_TCP` | `true` | `0` / `false` / `no` / `off` denies TCP bind + connect |

What it enforces:

- **Reads and executes are unrestricted.** PATH lookups, shared libraries and
  ordinary tooling keep working — the boundary is about mutation, not secrecy.
- **Writes are denied** except under the project root (`--project`), the
  agent-block state dir (`AGENT_BLOCK_HOME`, default `$HOME/.agent-block`),
  `/tmp`, `/dev/null`, `/dev/urandom`, `/dev/tty`, and anything listed in
  `AGENT_BLOCK_SANDBOX_FS_RW`. Allowlist entries that do not exist are skipped.
- **io_uring is denied** (`io_uring_setup` / `_enter` / `_register` return `EPERM`),
  since a ring bypasses the syscall-level view a seccomp filter has.
- **Child processes inherit it.** Landlock rulesets and seccomp filters survive
  `fork`/`execve`, so `sh.exec` payloads and `mcp.connect` servers run inside the
  same boundary with no extra wiring. The Lua `os.*` / `io.*` stdlib is left
  intact and caught at the OS layer instead.
- **Fail-closed startup.** If the sandbox is requested but the kernel enforces
  nothing, the process exits with an error instead of running unconfined, and
  an unresolvable `--project` path is a startup error (it is the primary write
  grant). A partial enforcement of the default rights on older Landlock ABIs
  logs a warning naming what was dropped and continues — except an explicitly
  requested TCP denial (`AGENT_BLOCK_SANDBOX_TCP=0`), which aborts startup on
  kernels older than 6.7 rather than silently failing open.

Operational notes:

- **A build target outside `--project` needs an explicit grant.** The default
  write allowlist is only the project root, `AGENT_BLOCK_HOME`, `/tmp` and
  `/dev/{null,urandom,tty}`, so when the directory being worked on is not under
  `--project` (e.g. the checkout driving the run differs from the repo being
  built), add that checkout to `AGENT_BLOCK_SANDBOX_FS_RW`.
- **Toolchain cache dirs must be writable too.** `~/.cargo` is not in the
  default allowlist, so cargo fails creating its registry cache
  (`Permission denied`). Either list it in `AGENT_BLOCK_SANDBOX_FS_RW` or point
  the cache at an allowed path with `CARGO_HOME=/tmp/cargo` — the latter works
  from cold, since TCP stays open and downloads still succeed.

KNOWN LIMITATIONS:

1. **Linux only.** On other platforms `--sandbox` is a startup error, never a
   silent no-op.
2. **UDP and DNS are not restricted.** Landlock's network rights cover TCP
   bind/connect only, so `AGENT_BLOCK_SANDBOX_TCP=0` does not stop UDP traffic
   (DNS included) or unix-domain sockets.
3. **io_uring cannot be used inside the sandbox**, including by dependencies
   that would otherwise pick it up opportunistically.
4. **TCP is a single on/off switch** — no per-host or per-port granularity. This
   is an execution boundary, not a policy engine.
5. **The io_uring deny only exists on x86_64 / aarch64.** On other Linux
   architectures no seccomp filter is compiled and the deny is skipped with a
   warning; the Landlock filesystem boundary still applies.

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

See `crates/agent-block/examples/verify_echo_harness.lua` for the full verification script.

## MCP Resource Subscribe Smoke Server

A standalone binary example for shell-level smoke-testing the Resource Subscribe API
(`mcp.subscribe_resource` / `mcp.on_resource_update`). Starts an HTTP MCP server with
`resources.subscribe` capability enabled and fires at least one `notify_resource_updated`
event after each subscribe call.

```sh
# Ephemeral port — prints SUBSCRIBE_TEST_SERVER_URL=http://127.0.0.1:<port>/mcp
cargo run --example subscribe_test_server

# Fixed port
cargo run --example subscribe_test_server -- --port 7878

# Periodic notify every 500 ms (instead of single fire on subscribe)
cargo run --example subscribe_test_server -- --port 0 --interval 500
```

Shell smoke (requires the server URL printed above):

```sh
export MCP_HTTP_URL="$(cargo run --example subscribe_test_server 2>/dev/null \
    | grep SUBSCRIBE_TEST_SERVER_URL | cut -d= -f2-)"
agent-block -s tests/fixtures/mcp_on_resource_update_callback.lua
# Expect: SUBSCRIBE_OK, RESOURCE_UPDATE_EV_OK, UPDATE_HITS=1, FIXTURE_DONE
```

See `docs/runbooks/e2e-mcp-resource-subscribe.md` for the full positive/negative verification
procedure (Step 2 = shell positive, Step 3 = negative against a server without subscribe
capability).

## Lua API

### llm.*
- `llm.chat(messages, opts)` — LLM call (Anthropic Messages API)

### tool.*
- `tool.register(name, schema, handler [, meta])` — Register a tool. Optional `meta = { group = "..." }` assigns the tool to a named group for use with `agent.run({ tool_groups = {...} })`.
- `tool.call(name, input)` — Call a registered tool
- `tool.list()` — List registered tool names
- `tool.schema()` — Anthropic tools-format schema array (includes `group` field when set)

### mcp.*

Support status, capability matrix, and the tool-grouping design rationale
live in `docs/architecture/mcp-support.md`.

- `mcp.connect(name, command, args, opts)` — Spawn MCP server over stdio + initialize handshake.
  `opts.trace_context` (bool, default `false`) injects `__ab_obs` into `call_tool` arguments;
  `opts.cwd` (string) overrides the subprocess working directory (default: project root).
  The spawned server is a child process, so — exactly like `sh.exec` children — it does **not**
  inherit the host's own credential variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
  `AGENT_BLOCK_MESH_SECRET_KEY`). A server that legitimately needs one is handed it explicitly:
  `mcp.connect(name, cmd, args, { env = { ANTHROPIC_API_KEY = "..." } })` — `opts.env` is a
  string→string table applied *after* the removal, and is the only way to pass a stripped
  variable through. Every other variable is still inherited (this is not an allowlist); a
  non-string `opts.env` value raises an error rather than being dropped.
- `mcp.connect_http(name, url, opts)` — Connect to an MCP server over HTTP transport.
  `opts.transport = "sse" | "http"` (default `"http"` = Streamable HTTP; `"sse"` = SSE).
  `opts.headers` table is forwarded as request headers.
- `mcp.call(name, tool_name, arguments)` — Call an MCP tool
- `mcp.list_tools(name)` — List available tools
- `mcp.list_resources(name)` — List resources exposed by the server.
  Returns `{ ok=true, resources=[{uri, name, description, mimeType, ...}] }`.
- `mcp.list_resource_templates(name)` — List resource URI templates exposed by the server.
  Returns `{ ok=true, resource_templates=[{uriTemplate, name, ...}] }`.
- `mcp.read_resource(name, uri)` — Read a resource by URI.
  Returns `{ ok=true, contents=[{uri, mimeType, text|blob}] }`.
- `mcp.list_prompts(name)` — List prompt templates exposed by the server.
  Returns `{ ok=true, prompts=[{name, description, arguments}] }`.
- `mcp.get_prompt(name, prompt_name, args)` — Retrieve a rendered prompt template.
  Returns `{ ok=true, description, messages=[{role, content}] }`.
- `mcp.complete(name, ref, arg_name, arg_value)` — Request completion suggestions (MCP Completion typeahead, Phase 3).
  `ref` is `{type="ref/prompt", name=...}` or `{type="ref/resource", uri=...}`.
  Returns `{ ok=true, values=[...], total=number?, has_more=bool? }` or `{ ok=false, error=str }`.
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
- `mcp.set_elicitation_handler(server_name, fn)` — Register a per-server Lua function to respond
  to `elicitation/create` requests originating from the MCP server (server→client, Form variant
  only). `fn(server_name, message, schema_json)` must return a table with `action =
  "accept"|"decline"|"cancel"` and (for accept) a `content` table conforming to the schema.
  Url-variant elicitation requests are always declined without reaching the callback. Handler must
  be a pure Lua function.
- `mcp.set_roots_handler(server_name, fn)` — Register a per-server Lua function to respond to
  `roots/list` requests originating from the MCP server (server→client direction).
  `fn(server_name)` must return a Lua array of root tables, each with at least a `uri` field
  and an optional `name` field (e.g. `{ { uri="file:///home/user", name="home" } }`).
  When no handler is registered the server receives `method_not_found`. Handler must be a pure
  Lua function; C functions and Rust-bound callbacks are not supported.
- `mcp.notify_roots_list_changed(name)` — Send a `notifications/roots/list_changed`
  notification to the named server (client→server, fire-and-forget). Use this whenever the
  client's set of filesystem roots changes so the server can re-request the updated list via
  `roots/list`. Failures are logged at `warn` level and silently discarded.
- `mcp.server_info(name)` — Return the server's `InitializeResult` as a Lua table.
  Returns `{ ok=true, server_info={serverInfo, capabilities, ...} }` on success.
  Useful for inspecting which MCP capability groups (resources, prompts, tools, etc.)
  a server declares. Returns `{ ok=false, error="..." }` if the server is not connected.
- `mcp.ping(name)` — Send a `ping` keepalive request to the named server and measure
  round-trip latency. Returns `{ ok=true, latency_ms=N }` on success or
  `{ ok=false, error="..." }` on failure (unknown server, timeout, or RPC error).
- `mcp.subscribe_resource(server, uri)` — Send a `resources/subscribe` RPC for the given
  resource URI. Returns `{ ok=true }` on success or `{ ok=false, error="..." }` on failure.
  Requires the server to declare the `resources.subscribe` capability.
- `mcp.unsubscribe_resource(server, uri)` — Send a `resources/unsubscribe` RPC to stop
  receiving change notifications for the given URI. Same return shape as `subscribe_resource`.
- `mcp.on_resource_update(server, callback)` — Register a per-server callback for
  `notifications/resources/updated` events. `callback(ev)` where
  `ev = { type="resource_update", server, uri }`. Handler must be a pure Lua function.
- `mcp.on_resources_list_changed(server, callback)` — Register a per-server callback for
  `notifications/resources/list_changed` events. `callback(ev)` where
  `ev = { type="resources_list_changed", server }`.
- `mcp.on_tools_list_changed(server, callback)` — Register a per-server callback for
  `notifications/tools/list_changed` events. `callback(ev)` where
  `ev = { type="tools_list_changed", server }`.
- `mcp.on_prompts_list_changed(server, callback)` — Register a per-server callback for
  `notifications/prompts/list_changed` events. `callback(ev)` where
  `ev = { type="prompts_list_changed", server }`.
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
- `sh.exec(cmd, opts)` — Execute a shell command. `opts.cwd` (default: project root), `opts.timeout` (seconds, default 30). On timeout the child is SIGKILLed, not left running.
- Children inherit the environment **except the host's own credential variables**: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and `AGENT_BLOCK_MESH_SECRET_KEY` are removed from every `sh.exec` child, so code the agent runs (including code it just wrote) cannot read the keys the host itself uses.
- A script that legitimately needs an API key must be given its own — pass it under a different variable name, or through the block conf (`api_key` / `api_key_env`). Custom `api_key_env` names are *not* stripped, and this is not an allowlist: every other variable is still inherited.
- The same set is removed from MCP server subprocesses spawned by `mcp.connect`; that path takes an explicit `opts.env` table for servers that need a key (see `mcp.*` above).

### std.json.* (mlua-batteries)
- `std.json.encode(value)`, `std.json.decode(str)`, `std.json.encode_pretty(value)`

### std.env.* (mlua-batteries + agent-block extensions)
- `std.env.get(key)`, `std.env.set(key, value)`, `std.env.get_or(key, default)`, `std.env.home()`
- `std.env.agent_id()`, `std.env.project_root()` — agent-block specific

### std.path.* / std.time.* (mlua-batteries)
- `std.path.join(...)`, `std.path.basename(path)`, `std.path.dirname(path)`
- `std.time.now()`, `std.time.sleep(secs)`, `std.time.measure(fn)`

### std.kv.* (mlua-batteries, SQLite-backed)
- `std.kv.get(ns, key)` — retrieve a value by namespace + key; returns `nil` if absent
- `std.kv.set(ns, key, value)` — store a value (any Lua value, JSON-encoded internally)
- `std.kv.delete(ns, key)` — delete a key; returns `true` if it existed, `false` otherwise
- `std.kv.list(ns, prefix?)` — list keys in a namespace, optionally filtered by prefix
- `std.kv.register_tools()` — register `kv_get`, `kv_set`, `kv_delete`, `kv_list` as LLM-callable tools

Storage: `AGENT_BLOCK_HOME/kv.sqlite` (override via `AGENT_BLOCK_KV_PATH`; `:memory:` supported).

### std.sql.* (mlua-batteries, SQLite-backed)
- `std.sql.execute(sql, params?)` — execute a DML statement; returns `{ affected = N }`
- `std.sql.query(sql, params?)` — execute a query; returns an array of row tables
- `std.sql.register_tools()` — register `sql_execute`, `sql_query` as LLM-callable tools

Storage: `AGENT_BLOCK_HOME/sql.sqlite` (override via `AGENT_BLOCK_SQL_PATH`; `:memory:` supported).

### std.ts.* (agent-block, SQLite-backed TSDB)
- `std.ts.append(series, value, tags?, at?)` — append a data point; `value` is a Lua
  number or table (JSON-encoded, losslessly decoded on read); `tags` is an optional
  `{key=value}` table; `at` is an optional Unix timestamp in milliseconds (default: now)
- `std.ts.query(series, opts)` — range query; `opts` fields:
  - `from`, `to` (integer ms) — time range (default: full range)
  - `tags` (table) — AND-filter; each key-value pair uses SQLite `json_extract`
  - `agg` (string) — `"count"` | `"sum"` | `"avg"` | `"last"` (optional)
  - `bucket_ms` (integer) — bucket width; requires `agg`; produces time-bucketed rows
  - `limit`, `offset` (integer) — pagination
- `std.ts.last(series, tags?)` — most-recent data point; same tag AND-filter as `query`
- `std.ts.register_tools()` — register `ts_append`, `ts_query`, `ts_last` as LLM-callable tools

Ordering guarantee: raw-path results (`query` without `agg`) are ordered by `(ts ASC, rowid ASC)`;
`last` and `query` with `agg="last"` resolve same-millisecond ties by `(ts DESC, rowid DESC)` so
the last-appended row always wins. This is a deterministic SQLite rowid tie-breaker — no DDL or
index change is required.

Storage: `AGENT_BLOCK_HOME/ts.sqlite` (override via `AGENT_BLOCK_TS_PATH`; `:memory:` supported).

### Embedded blocks: four layers

The crate's `blocks/` directory is baked into the binary, so `require("agent")` works
after `cargo install` with no path configuration. A filesystem copy of a module in a
`lib/` tier normally wins over the embedded one — but the four layers below differ in
whether that is the intended way to change them.

| layer | modules | how to change it |
|---|---|---|
| kernel + declaration | `knl`, `knl_adapter`, `knl_types`, `lshape` (and `lshape.t` / `.check` / `.reflect` / `.luacats`) | **Sealed** — a filesystem copy fails the run rather than replacing the module. The kernel is one thing across Rust and Lua, held together by declaration tests a Lua-side replacement would pass while meaning something else. Change it upstream. `AGENT_BLOCK_UNSEAL=1` downgrades the refusal to a warning, for work on the kernel itself and not for shipping. |
| shell packs | `policy`, `supervisor` | Do not shadow: a pack is a value you hand to `knl.device` or consult in your own loop, not a registry the host reads. Write your own pack beside them and pass that. For a partial change, delegate through `embedded.<name>`. |
| consumers | `agent`, `compile_loop`, `coding_agent` | Copy-on-write is the intended way — drop your own `lib/agent/init.lua` in the project root (or `~/.agent-block/lib/`) and it is the `agent` your scripts get. For a partial change, delegate through `embedded.<name>`. |
| utilities | `llm_proto` (with `.openai` / `.anthropic`), `mcp_tools`, `session` | Shadowing works, but `knl_adapter` requires `llm_proto` and `mcp_tools`, so replacing either replaces a sealed module's dependency. Prefer delegation. |

Resolution order, highest priority first: the script's own directory →
`project_root/lib/` → `$AGENT_BLOCK_HOME/lib/` → embedded. `blocks/` directories are
not on it (see [Blocks and libraries](#blocks-and-libraries)). The seal is checked once
at start over those filesystem roots, before the script runs; the error names the module
and the file that would have replaced it.

Delegation — a shadowing module reaching the one it replaced:

```lua
-- project_root/lib/agent/init.lua
local base = require("embedded.agent")
local M = setmetatable({}, { __index = base })
function M.run(opts) print("OVERRIDE"); return base.run(opts) end
return M
```

`embedded.<name>` resolves from memory and only from memory: every embedded module is
registered a second time under that prefix, ahead of the filesystem roots, so a
`lib/embedded/` directory cannot stand in for it. The alias evaluates the embedded
source under its own name, which means `require("agent")` (yours) and
`require("embedded.agent")` (the base) are two tables — exactly the pair the idiom needs.
It exists for sealed modules too: `require("embedded.knl")` reads the kernel, which is
fine; replacing it is what the seal refuses.

Promotion runs the other way. A module that starts as `project_root/lib/<name>/init.lua`
moves to `~/.agent-block/lib/` when a second project wants it, and becomes an embedded
lib by a change upstream once it proves general — the same file, moved under
`crates/agent-block-core/blocks/` and listed in `EMBEDDED_LIBS`. Projects still carrying
their own copy keep resolving to it until they delete it.

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
        -- info keys: turn_number, content, tool_calls, usage. Returning false
        -- stops the run.
        print("turn", info.turn_number, "#tools", #info.tool_calls)
    end,
    extra_tools = {},                                   -- optional extra Anthropic tool defs
    tool_groups = { "outline" },                        -- optional; nil = every tool
    history     = prior,                                -- optional prior messages (e.g. session.load)
    store       = "mem",                                -- optional; where the session log goes.
                                                        -- Omitted = the host's own database;
                                                        -- "mem" or { sqlite = <path> } otherwise.
})

if result.ok then
    print(result.content)
else
    print("error:", result.error)
end
-- result fields: ok, content, usage{input_tokens,output_tokens,total_tokens}, num_turns, error, messages
```

**Correlation ids**

There is no option for them. `agent.run` reads `AGENT_BLOCK_TRACE_ID`, `AGENT_BLOCK_RUN_ID`, `AGENT_BLOCK_AGENT_ID` and `AGENT_BLOCK_AGENT_NAME` off the environment and stamps whichever are set onto the run's seed event as `meta` labels — the same four the HTTP bridge puts on its `ab.obs` `http_request` / `http_response` lines. So the session log is selected by the id the log lines are grepped by:

```lua
session:query("SELECT * FROM events WHERE json_extract(meta, '$.run_id') = ?",
              { std.env.get("AGENT_BLOCK_RUN_ID") })
```

With `AGENT_BLOCK_AGENT_ID` unset the obs lines still carry a per-process id the host makes up, and the seed event carries no `agent_id`; set it and both sides agree.

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

**Protocol options (`llm_proto`)**

Request building lives in the `llm_proto` package, reached through the provider Port every block's device carries. The vocabulary is OpenAI's; each adapter renames or drops what its provider does not accept, so the same options work on both paths.

```lua
local result = agent.run({
    prompt = "...",
    -- "auto" | "none" | "required" | { type = "function", name = "grep" }
    -- Anthropic spellings ("any", { type = "tool", name = ... }) are accepted too.
    tool_choice = "required",
    parallel_tool_calls = false,

    -- Reasoning / extended thinking. `false` turns it off where that is expressible.
    thinking = { effort = "medium" },        -- or { budget_tokens = 8000 }, or true / false
    dialect  = "vllm",                       -- openai | vllm | llamacpp | ollama (default: from base_url)

    -- Structured outputs (OpenAI shape; mapped to output_config.format on Anthropic).
    response_format = { type = "json_schema", json_schema = { name = "out", schema = { ... } } },

    -- Sampling and request knobs, forwarded where the provider supports them.
    temperature = 0.2, top_p = 0.9, top_k = 40, stop = { "END" }, seed = 7,
    max_retries = 2,                         -- transient failures only (rate limit / overload / 5xx)
})
```

Notable translations:

| you pass | Anthropic | OpenAI | vLLM / llama.cpp / Ollama |
|---|---|---|---|
| `tool_choice = "required"` | `{type="any"}` | `"required"` | as OpenAI (ignored by Ollama) |
| `parallel_tool_calls = false` | `tool_choice.disable_parallel_tool_use` | top-level | top-level |
| `thinking = { effort = ... }` | `{type="adaptive"}` + `output_config.effort` (4.7+) or `{type="enabled", budget_tokens}` (4.5 and earlier) | `reasoning_effort` | `chat_template_kwargs` (+ `reasoning_effort`); Ollama takes `reasoning_effort` only |
| `stop` | `stop_sequences` | `stop` | `stop` |
| `max_tokens` | `max_tokens` | `max_completion_tokens` on o-series / gpt-5, else `max_tokens` | `max_tokens` |

Combinations the API would reject are refused locally instead: forced tool use under manual extended thinking, a thinking budget that does not fit in `max_tokens`, and tools plus reasoning on gpt-5.6+. Values a model cannot accept (`temperature` on reasoning models, `temperature ~= 1` on Claude past Opus 4.6, `top_k` on api.openai.com) are dropped with a `warn` rather than sent.

Responses are normalized to one shape regardless of provider: reasoning arrives as a `thinking` content block whether the server sent `reasoning_content`, `reasoning`, Anthropic thinking blocks, or raw `<think>` tags in the text; `usage` carries `cache_read_input_tokens` / `cache_creation_input_tokens` / `thinking_tokens` on both paths.

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
- MCP tools are automatically assigned to a group for use with `tool_groups`. Group resolution follows this priority: (1) the tool's `_meta.group` field (string, non-empty) declared by the server takes precedence — rmcp serialises `Tool.meta` as `_meta` via `#[serde(rename = "_meta")]`; (2) fallback to the server name. Pass `tool_groups = { "outline" }` (for example) to `agent.run` to include only tools from that MCP server. This aligns with the MCP SEP-986 tool-name prefix grouping guidance and the `mcp__<server>__*` convention used by Claude Code. Tools without an explicit group (e.g. plain registered Lua tools) fall into the `"default"` group.
- Tool dispatch: MCP tools via `mcp.call()`, registered Lua tools via `tool.call()`.
- Never throws — all errors returned as `{ ok=false, error="..." }`.
- Context editing is on by default: once the conversation crosses ~80K input tokens, Anthropic evicts all but the most recent 3 tool-use / tool-result pairs server-side so the loop can keep running. Works on Sonnet 4 / Sonnet 4.5 / Haiku 4.5 / Opus 4 / 4.1 / 4.5. Pass `context_management = false` to disable, or `context_management_config = { edits = { ... } }` to replace the default entirely (the whole table is forwarded as `body.context_management`; no partial merge).
- `on_turn(info)` is handed exactly four keys — `turn_number`, `content`, `tool_calls`, `usage` — and returning `false` from it stops the run. What the server did with context editing is not among them; the response that carried it is in the session log as `llm_response`.
- `agent` is a consumer block: a local `lib/agent/init.lua` in the project root replaces it, and can delegate to the embedded one through `require("embedded.agent")`. See [Embedded blocks: four layers](#embedded-blocks-four-layers).
- No block emits an LLM dump. Each model call is recorded in the session log (`llm_request` / `llm_response` / `llm_call_failed`) instead, and `AGENT_BLOCK_LLM_DUMP` is gone with the layer that read it.

### compile_loop (StdPkg — `require("compile_loop")`)

Tool factory for the autonomous compile-and-fix loop. The primary surface is
`compile_loop.make(conf)`, which returns a `tool_def` consumable directly by `agent.run`.

One iteration is one beat of the kernel (`knl.beat`: a model call plus the tools that
call asked for), and the thing this block adds to it is the guarantee it sells: **the
verify is not a tool**. `conf.runner` runs after every beat, whatever the model asked
for and whatever it answered, and its verdict — not the model's — ends the run. A tool
the model can decline to call cannot carry "it compiles".

`max_iters` is the session's grant, so the iteration ceiling is the budget: the beat
past it stops with nothing called.

Embedded, and a consumer block: `lib/compile_loop/init.lua` in the project root replaces
it. See [Embedded blocks: four layers](#embedded-blocks-four-layers).

```lua
local compile_loop = require("compile_loop")
local agent        = require("agent")

-- Define a caller-supplied runner function
local LUA_RUNNER_TIMEOUT = 60

local function lua_runner(file_path)
    local res = sh.exec("lua " .. file_path, { timeout = LUA_RUNNER_TIMEOUT })
    if not res.ok then
        -- spawn failure or timeout: no exit code exists
        return { ok = false, stdout = "", stderr = tostring(res.error), exit_code = -1 }
    end
    local pass = res.code == 0 and res.stdout:find("ALL_PASS", 1, true) ~= nil
    return { ok = pass, stdout = res.stdout, stderr = res.stderr, exit_code = res.code }
end

-- Build a tool_def and pass it to the parent agent
local td = compile_loop.make({
    runner    = lua_runner,       -- required: function(path) → {ok, stdout, stderr, exit_code}
    max_iters = 5,                -- optional, default 5
    lang      = "lua",            -- optional, default "lua"
    -- conf.llm is forwarded to the provider Port verbatim. Nothing is inherited
    -- from the calling agent: a device is passed, not discovered, so an omitted
    -- api_key falls through to llm_proto's own env resolution and nowhere else.
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

**`compile_loop.make(conf)`** returns `{ name, schema, handler }` and nothing else
happens to it: registering it is the caller's, and `agent.run({ extra_tools = { td } })`
takes it directly. A caller that wants it in the global registry calls
`tool.register(td.name, td.schema, td.handler)` itself (which is what
`coding_agent.register_tool` does). The tool name defaults to `"compile_loop"`; pass
`conf.name` to override.

**Multi-file mode**: pass `target_files = {pathA, pathB, ...}` together with `edit_mode = "diff"` to edit several files in a single loop. The runner signature changes to `function(paths)` (list). `tool_mode = "auto" | "read_only"` controls which tools are declared — `"read_only"` withholds the edit tool, which makes the run a dry run that can inspect and cannot converge. Callers can inject their own tools via `extra_tools = {{name, schema, handler}, ...}`; a name that collides with a built-in is a loud error rather than a silent winner. See the `crates/agent-block/examples/test_anthropic_compile_loop_multi*.lua` smoke scripts.

**The tools diff mode declares**: `std.fs`' own `fs_read` and `fs_edit`, path-locked to
the target files for the duration of the call, plus `read_file_range`.

**Large files**: a whole-file `fs_read` of something over 10 000 characters is refused
with the file's length and a pointer at the range read (`fs_read` takes `start_line` /
`end_line`, and `read_file_range` hands back a verbatim, line-numbered slice of at most
500 lines). There is no digest, no summarising sub-call and no cache: a model that asked
for a 400KB file did not mean to spend its context on one, and a summary of a file is not
a thing `fs_edit` can address.

**Tool input** (supplied by the LLM at call time): `spec` (string, required),
`target_file` **or** `target_files` (absolute paths, mutually exclusive), `lang`
(string, optional).

**`edit_mode` (opt-in diff mode)**: pass `edit_mode = "diff"` to `compile_loop.make` to
have the child LLM edit through the tools instead of emitting the whole file on every
iteration. This is the preferred mode for large existing files where minimal-edit is
critical (e.g. fixing a single function in a 500-line file). The target files must
already exist and be non-empty — diff needs something to diff against, and a mode that
silently became `"full"` was worse than an error.

```lua
local td = compile_loop.make({
    runner    = lua_runner,
    edit_mode = "diff",        -- opt-in; default is "full"
    llm       = { provider = "anthropic", model = "claude-haiku-4-5-20251001" },
})
```

The child LLM edits by calling `fs_edit`, which addresses lines rather than searching for
text: `start_line`, `end_line`, and the `expect`ed current content of those lines, checked
before anything is applied. A rejected edit comes back as a tool result naming what is
actually at those lines, so the model can correct itself from the answer instead of
re-reading the file.

**`target_file` dual role** (full mode): when `target_file` already exists at loop entry,
its content is embedded in the initial user message as `=== Current file content ===` so
the child LLM can build on it rather than generating from scratch, and the file is
overwritten on every iteration. When it is absent or empty, the message carries `spec`
only — the synthesis case.

**Target model class**: the full-file output strategy is designed for Qwen3 / Haiku-grade
mid-weight models. Emitting the whole file on each iteration avoids the apply-failure cost of
diff/Edit-tool workflows and keeps the feedback loop simple and fast. For the latest
Sonnet/Opus with native edit-tool support, a diff-based block is a future consideration
(separate issue; out of scope here).

**Tool output JSON** (never contains `code` or `history`: the run's transcript is the
session log, and handing a caller one contaminates its context. The shape is closed, so
there is no field a transcript could leave by):

```
{ ok, iters, summary, failure_reason?, last_error?, artifact_path }
```

`failure_reason` values: `"llm_call"` | `"open_target_file"` | `"stagnation"` |
`"no_edits_applied"` | `"max_iters"` | `"stopped"` (the kernel stopping a beat for a
reason other than the grant, which a caller should not normally see; `last_error` carries
the kernel's word for it). `modified_files` is present in diff mode (the paths whose edits
landed, on every ending, including a give-up); `artifact_path` is the single path in
single-file mode.

**LLM resolution**: `conf.llm` is forwarded to the provider Port verbatim, and nothing is
inherited from a calling agent — a device is passed, not discovered. An omitted `api_key`
falls through to `llm_proto`'s env resolution (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`, or
whatever `api_key_env` names). `provider` defaults to `"anthropic"` and picks the Port at
`make()` time, so an unknown one is an error there rather than a failure five iterations in.

**Giving up**: two readings of the log, each with its own reason.

- `"stagnation"` — the verify said the same thing three times running
  (`policy.stagnation` over the recorded `verify` events).
- `"no_edits_applied"` — three consecutive iterations landed no edit at all. Distinct
  from the above: the model is acting and nothing is changing.

Both are independent of the remaining budget; `"max_iters"` is the budget itself.

**Observability**: there is none of this block's own, and that is the point. Every model
call is a durable record in the session log (`llm_request` / `llm_response` /
`llm_call_failed`), every tool call is a recorded pair, and each iteration's verify is a
`verify` event stamped with the beat it judges. Read them with `session:query` or
`knl.views` rather than from stdout. `AGENT_BLOCK_LLM_DUMP` and the `ab.obs` iteration
trail are gone.

**Provider support**: `"anthropic"` and `"openai"`-compatible endpoints (vLLM, llama.cpp,
OpenRouter, RunPod, etc.) are both fully implemented in `conf.llm`.

| `conf.llm.provider` | Default key env     | Override via                        |
|---------------------|---------------------|-------------------------------------|
| `"anthropic"`       | `ANTHROPIC_API_KEY` | `conf.llm.api_key` / `api_key_env`  |
| `"openai"`          | `OPENAI_API_KEY`    | `conf.llm.api_key` / `api_key_env`  |

#### External runner examples

| Example | Runner | Provider |
|---|---|---|
| `crates/agent-block/examples/test_anthropic_compile_loop.lua` | inline lua | Anthropic |
| `crates/agent-block/examples/test_qwen_compile_loop.lua` | inline lua | Qwen (OpenAI-compat) |
| `crates/agent-block/examples/test_qwen_compile_loop_rust.lua` | inline cargo | Qwen (OpenAI-compat) |
| `crates/agent-block/examples/test_qwen_compile_loop_lust.lua` | mlua-probe MCP | Qwen (OpenAI-compat) |
| `crates/agent-block/examples/test_compile_loop_parent.lua` | inline lua | Anthropic parent + Qwen child |
| `crates/agent-block/examples/test_anthropic_compile_loop_pytest.lua` | inline pytest | Anthropic |
| `crates/agent-block/examples/test_anthropic_compile_loop_multi.lua` | inline lua (multi-file) | Anthropic |
| `tests/fixtures/compile_loop_range_mock.lua` | e2e fixture (oversized file, range read) | Anthropic |

### coding_agent (StdPkg — `require("coding_agent")`, thin facade)

Backward-compatible facade over `compile_loop`. Prefer the `compile_loop.make()` API for
new code. `coding_agent` is retained for existing callers.

It is three things and nothing else: the two built-in runners below, one call to
`compile_loop.make`, and the `tool.register` that `make` deliberately does not do. There
is no loop here — iterations, the verify, the give-up gates and the result shape are all
`compile_loop`'s. `edit_mode`, `tool_mode` and `extra_tools` are not on the facade's opts;
a caller who wants them calls `compile_loop.make` directly.

Embedded, and a consumer block: `lib/coding_agent/init.lua` in the project root replaces
it. See [Embedded blocks: four layers](#embedded-blocks-four-layers).

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
        local res = sh.exec("lua " .. file_path, { timeout = 60 })
        if not res.ok then  -- spawn failure or timeout: no exit code exists
            return { ok = false, stdout = "", stderr = tostring(res.error), exit_code = -1 }
        end
        return { ok = res.code == 0, stdout = res.stdout, stderr = res.stderr, exit_code = res.code }
    end,
    on_iter = function(info) print("iter", info.iter, info.result.ok) end,
})

-- res fields:
--   ok             boolean
--   artifact_path  string      absolute path of the target file
--   iters          int
--   summary        string      "PASS in N iters" or "give-up: <reason>"
--   failure_reason string?     see the compile_loop section above for the full set
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
| `"lua"`       | Runs `lua <file>` and passes on exit 0 + `ALL_PASS` in stdout (60 s timeout) |
| `"cargo"`     | Runs `cargo test --offline` in the file's directory; passes on exit 0 + `"test result: ok"` (300 s timeout) |
| function      | Called as `runner(file_path)` — must return `{ ok, stdout, stderr, exit_code }` |

Both built-ins execute through `sh.exec`, so the host's own credential variables are stripped
from the child (see `sh.*`), stdout and stderr stay separate, and a hung command is SIGKILLed
when the timeout above expires (`sh.exec` spawns with `kill_on_drop`). Caller-supplied runner
functions should use `sh.exec` for the same reasons — `io.popen` children inherit the host
environment unfiltered and outlive any timeout.

Runner commands execute with **cwd = the project root** (`sh.exec`'s default), not the directory
`agent-block` was started from — deliberately deterministic, and a change from the earlier
`io.popen` behaviour, which inherited the host process's cwd. Pass `cwd` in the `sh.exec` opts to
override; the `"cargo"` built-in does exactly that with the target file's directory.

### lshape (Vendored package — `require("lshape")`)

`lshape` is vendored under `blocks/lib/lshape/` so scripts can use schema validation
and LuaCATS generation without external installation.

```lua
local lshape = require("lshape")
local T = lshape.t
local User = T.shape({ name = T.string, age = T.number })
local ok, why = lshape.check.check({ name = "Ada", age = 36 }, User)
assert(ok, why)
```

### Lua kernel (knl — `require("knl")`)

`knl` is the Lua half of a kernel/shell split. Rust owns the **session**: an
append-only event log, a budget the owner granted it, and a scope it is written
under. Lua owns the **beat** — one model call plus the tools that call asks for
— and the **device** it runs against, a frozen bundle of policy (`llm`, `tools`,
`tool_policy`, `fold`, `filters`, `system`, `cost`). `knl.beat(session, device)`
takes the two separately because they differ in owner, lifetime and mutability.
There is no run loop: a caller writes the loop, which is why the primitive is
one beat.

```lua
local kernel  = require("knl")
local adapter = require("knl_adapter")

local device = kernel.device({
    llm   = adapter.anthropic:open({ model = "claude-haiku-4-5-20251001", max_tokens = 1024 }),
    tools = adapter.tools({ ... }),        -- flat specs or ToolPorts
})

kernel.session({ owner = "u", budget = { amount = 8, tag = "beats" } }, function(s)
    s:append({ kind = "msg_user", data = { content = "..." } })

    local out                                -- the loop is yours to write
    for _ = 1, 8 do
        out = kernel.beat(s, device)         -- ok | refused | error | stopped
        if not kernel.Outcome.is_ok(out) then break end
        if #out.out.tools == 0 then break end -- nothing left to answer
    end

    local rows = kernel.views.usage(s)       -- one SELECT over the log
    for _, row in ipairs(rows) do
        print(row.calls, row.input_tokens, row.output_tokens)
    end
end)                                         -- the bracket closes either way
```

Views come in two tiers. A **built-in view** is a kernel read reached with
`s:view(name, opts?)`, and there are exactly two fixed reads —
`s:events(from)` and `view("tail", { n })`. Everything else is a **query
view**: a named Lua function running one `SELECT` through `s:query(sql,
params?, opts?)` over the published event table. `knl.views.beats` /
`tool_pairs` / `ledger` / `usage` are the four shipped, and a consumer's own
view is a function of the same form — nothing about the four is privileged.

The design is in three module docs: `crates/agent-block-core/src/knl/mod.rs`
(the kernel's invariants), `crates/agent-block-core/src/bridge/knl.rs` (the
syscall surface Lua sees) and
`crates/agent-block-core/blocks/lib/knl/init.lua` (this half: beat, device,
Outcome, shapes, views).

### log.*
- `log.info/warn/error/debug(msg)`

## Testing

### Rust (e2e + integration)

```
cargo test --workspace
```

### Lua block unit specs (mlua-lspec)

The embedded blocks (`crates/agent-block-core/blocks/agent`, `.../compile_loop`) expose
their pure, I/O-free helpers via a `_test_helpers()` accessor. Branch-level unit specs
live under `crates/agent-block/tests/fixtures/*_test.lua` and run with the
mlua-lspec framework (`describe` / `it` / `expect`) — they need no API keys and no
network. Run them via the `lua-debugger` MCP `test_launch` tool with the block
directory on the search path:

```
mcp__lua-debugger__test_launch(
  code_file    = "crates/agent-block/tests/fixtures/agent_helpers_test.lua",
  search_paths = ["crates/agent-block-core/blocks"]
)
mcp__lua-debugger__test_launch(
  code_file    = "crates/agent-block/tests/fixtures/compile_loop_helpers_test.lua",
  search_paths = ["crates/agent-block-core/blocks"]
)
```

What a spec can reach is what needs no kernel. `compile_loop`'s loop opens a
`knl` session, which is a syscall the pure spec runner does not have, so the
loop itself is covered by `tests/e2e_compile_loop.rs` against a mock provider
and the specs cover the helpers around it. Each spec file's header documents its
exact `test_launch` invocation.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
