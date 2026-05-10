# Runbook: e2e-mcp-resource-subscribe — MCP Resource Subscribe verify

**Goal**: verify that the 6 `mcp.*` Lua APIs added in commit `1f8e1d7`
(`subscribe_resource` / `unsubscribe_resource` /
`on_resource_update` / `on_resources_list_changed` /
`on_tools_list_changed` / `on_prompts_list_changed`) work end-to-end:
positive path (in-process MCP server with `resources/subscribe` capability)
and negative path (real MCP server without the capability).

**Scope**: `src/mcp_client/handler.rs` (4 `ClientHandler` overrides),
`src/mcp_client/mod.rs` (subscribe / unsubscribe RPC),
`src/bridge/mcp.rs` (6 Lua surface fns + 4 callback global tables).

**When to run**: regression check when any of the above three files is
touched, and as a release-gate check before bumping versions that include
MCP Resource Subscribe changes.

**Duration**: 1–2 minutes.

## Prerequisites

### 1. Build

```sh
cargo build --quiet
```

### 2. Optional install (for shell smoke)

```sh
cargo install --path . --quiet
# → ~/.cargo/bin/agent-block
```

### 3. Optional MCP server for shell smoke

`outline-mcp` is convenient because it does **not** declare
`resources.subscribe` capability — it exercises the negative-path error
handling.

```sh
cargo install --git https://github.com/ynishi/outline-mcp --quiet
# → ~/.cargo/bin/outline-mcp
```

(Any MCP server that does not advertise `resources.subscribe` works for
the negative-path check.)

## Verify steps

### Step 1: positive path (cargo test e2e)

In-process MCP server with `enable_resources_subscribe()` capability,
verifies full `subscribe_resource` → server-side
`notify_resource_updated` → Lua `on_resource_update` callback fire +
correct `ev.uri` payload + `unsubscribe_resource` round-trip.

```sh
cargo test --test e2e_mcp_resource_subscribe
```

**Expected**:

```
running 1 test
test subscribe_resource_callback_fires_with_correct_uri ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

The test asserts the following stdout markers appear in the Lua fixture
(`tests/fixtures/mcp_on_resource_update_callback.lua`):

- `SUBSCRIBE_OK`
- `RESOURCE_UPDATE_EV_OK`
- `UPDATE_HITS=1`
- `FIXTURE_DONE`

If any marker is absent or `UPDATE_HITS != 1`, the dispatch chain has
regressed (most likely `handler.rs` notification overrides).

### Step 2: negative path (shell smoke against outline-mcp)

Validates that a server returning `-32601` (Method not found) for
`resources/subscribe` is surfaced as `{ok=false, error=...}` in Lua and
emits a `tracing::warn`, instead of panicking or hanging.

```sh
cd /tmp
agent-block -s /path/to/agent-block/examples/mcp_resource_subscribe.lua
```

**Expected log lines** (timestamps and span fields elided):

```
INFO  serve_inner: rmcp::service: Service initialized as client
       peer_info=Some(InitializeResult { ..., capabilities: ServerCapabilities {
         ..., resources: None, ... }, ... })
WARN  agent_block::mcp_client: mcp subscribe_resource failed
       server=outline uri=resource:///example
       error=Mcp error: -32601: resources/subscribe
WARN  lua: subscribe failed: MCP error: subscribe_resource
       'resource:///example' on 'outline':
       Mcp error: -32601: resources/subscribe
WARN  agent_block::mcp_client: mcp unsubscribe_resource failed
       server=outline uri=resource:///example
       error=Mcp error: -32601: resources/unsubscribe
WARN  lua: unsubscribe failed: ...
INFO  serve_inner: ... task cancelled
INFO  serve_inner: rmcp::transport::child_process: Child exited gracefully
```

Failure modes:

- panic / unwrap → regression in `mcp.rs` Lua surface error mapping
- hang → regression in `Peer::subscribe` timeout wiring (`mod.rs`)
- no `WARN` log on the Rust side → regression in `mcp_client/mod.rs`
  `tracing::warn` instrumentation
- no `WARN` log on the Lua side → regression in the `register` function
  table fields (`subscribe_resource` / `unsubscribe_resource` did not
  return a table to Lua)

## Pass criteria

Both Step 1 and Step 2 must pass cleanly. Step 1 (positive) covers the
dispatch chain end-to-end; Step 2 (negative) covers the error path that
the in-process test server cannot reach.

## See also

- Design doc: `docs/architecture/agent-state-primitives.md` §3.7
- MCP spec: https://modelcontextprotocol.io/specification/draft/server/resources
- Implementation commit: `1f8e1d7`
- Reference test: `tests/e2e_mcp_resource_subscribe.rs`
- Reference example: `examples/mcp_resource_subscribe.lua`
- Reference fixture: `tests/fixtures/mcp_on_resource_update_callback.lua`
