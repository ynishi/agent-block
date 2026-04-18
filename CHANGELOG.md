# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- EventBus infrastructure landing (Subtask 1 of 4) — pure-Rust core types for an upcoming reactive / long-running agent mode. New `src/bus/` module: `Event` struct (kind / id / payload / meta + `oneshot` ack sender), `Source` async trait, `EventBus` struct with a serial `run(shutdown: CancellationToken)` dispatcher loop that fans a single bounded-mpsc input out to kind-specific handlers (with an `on_any` fallback), `Handler` / `HandlerKey` trait placeholders, and `panic::catch_unwind` isolation so a faulting handler does not kill the loop. Added `BlockError::Bus(String)` variant for bus-local errors. The module is currently compiled under `#[allow(dead_code)]` in `src/main.rs` — nothing is reachable from Lua yet.
- EventBus handler Isle split (Subtask 2 of 4) — `bus.on(kind, fn)` / `bus.on_any(fn)` now dump the Lua handler to bytecode via `Function::dump(true)` and reload it on a dedicated FullAsync Isle worker thread (`handler_isle`). The main VM keeps only a thin dispatcher that forwards `Event` to the Isle via channel. Rationale: CPU-bound Lua handlers (tight loops that do not `yield`) previously occupied the main-thread `LocalSet`, which blocked the `AGENT_BLOCK_TASK_GRACE_MS` shutdown waker — grace=1000ms got stretched to ~10s on real mesh because the busy handler never released the poll. With the Isle split, the main thread stays free to observe `shutdown.cancelled()` and the grace window is honoured with only scheduling overhead.
- Handler registration now rejects non-Lua closures (C functions / Rust closures wrapped via `create_function`) with a clear `bus.on: handler must be a pure Lua function` error, because `Function::dump` only supports Lua bytecode.
- Upvalue semantics change: upvalues captured by the handler closure must be serialisable to Lua bytecode — primitives / tables of primitives work, but `userdata` / `thread` / C-function upvalues fail at `dump` time. The doc comments on `bus.on` / `bus.on_any` / the `src/bridge/bus.rs` module header document the full contract.
- New `examples/bus-handler-grace/` runnable example (handler.lua + verify.sh + README) and `docs/runbooks/e2e-bus-handler.md` step-by-step runbook exercise the grace window end-to-end against the public mesh relay (`wss://agent-mesh.fly.dev/relay/ws`). The runbook pattern follows NATS-by-Example (runnable) + etcd/K8s runbook (prose) conventions for AI/Agent-reproducible E2E.
- **Not user-visible in this release.** The mesh source adapter (`mesh.on`) and Subtask 4 acceptance tests are deferred. Expect no behavioural change for existing single-run scripts until those land.

## [0.5.1] - 2026-04-17

### Changed

- `std.task` / `std.sql` / `std.kv` Lua bridge implementation moved upstream to `mlua-batteries` 0.3 (`task` / `sql` / `kv` features). The `bridge/task` / `bridge/sql` / `bridge/kv` modules become thin adapters that translate `AGENT_BLOCK_TASK_DRIVER` / `AGENT_BLOCK_TASK_GRACE_MS` / `AGENT_BLOCK_SQL_*` env vars into `mlua_batteries::task::TaskConfig` / `mlua_batteries::sql::SqlConfig` and delegate to `register_with`. Lua tool helpers (`sql_tools.lua` / `kv_tools.lua`) stay host-side because they require the `tool` global. No behavioural change: all 35 e2e tests pass unchanged. Net diff: −1656 lines.

## [0.5.0] - 2026-04-16

### Added

- `std.task` Lua bridge — structured concurrency primitives on `tokio::task::LocalSet`. Public API: `task.spawn(fn, opts?) -> handle`, `task.scope(name?, fn)`, `task.with_timeout(ms, fn, opts?)`, `task.sleep(ms)`, `task.yield()`, `task.checkpoint()`, `task.cancelled()`, `task.current()`. `handle:join()` / `handle:cancel()` / `scope:spawn()` / `scope:cancel()` surface per-task and per-scope control. Child tasks inherit a `CancelToken` via `tokio::task_local!`, so a parent cancel propagates cooperatively to every descendant at the next suspension point.
- `task.with_timeout` 3-stage graceful-abort teardown (Kubernetes / ASP.NET Core / Spring Boot pattern): (1) `token.cancel()` on deadline, (2) `drain_scope` under `timeout(grace_ms)`, (3) `AbortHandle` hard-abort for any child that did not reach a checkpoint. `tracing` events at `target = "task"` trace each stage (`debug` on cancel / normal drain, `warn` on grace expiry with remaining child count).
- Per-call grace override via `opts.grace_ms` and VM-wide override via `AGENT_BLOCK_TASK_GRACE_MS` env var. Default grace is 1 s — long enough for local cleanup (DB flush, fsync, HTTP release), short enough not to mask real hangs.
- Cancel-aware sleep and yield: `task.sleep` / `task.yield` / `task.checkpoint` all observe the enclosing `CancelToken`, so `pcall`-swallowed cancellations reappear at the next checkpoint and cannot be silently suppressed.
- Optional `coroutine` driver (`opts.driver = "coroutine"` or `AGENT_BLOCK_TASK_DRIVER=coroutine`): drives the user function via `Thread::resume` rather than `Function::call_async`, enabling `coroutine.yield(ms)` as a cancel-aware sleep inside a raw Lua thread.

#### Limits and silent behaviour

- **Per-scope child cap**: `scope:spawn` rejects beyond 32 concurrent children. Long fan-outs must batch or use a worker-pool pattern.
- **Dropped-handle error suppression**: if a `task.spawn` `handle` is dropped without `handle:join()`, the child's error is recorded via `tracing::error` but is **not** propagated into the surrounding scope body (first-error / `Task.WhenAll` semantics; no `ExceptionGroup`). To surface child errors, keep and join the handle.
- **ENV parse is silent**: a malformed `AGENT_BLOCK_TASK_GRACE_MS` (non-numeric, negative, overflow) falls back to the built-in default without raising — a bad shell env must not break every `with_timeout` in the VM at call time. Same policy as `AGENT_BLOCK_TASK_DRIVER`.

### Changed

- `std.sql` / `std.kv` now observe the enclosing task's `CancelToken` in `race_timeout` and call `sqlite3_interrupt` as soon as the task scope cancels. Before this change, `task.with_timeout` wrapping a long SQL query had to wait for the per-call `AGENT_BLOCK_SQL_QUERY_TIMEOUT_MS` to expire; task-driven cancel did not reach the blocking pool. This integration is the primary rationale for building `std.task` — SQL/KV are now task-API-native. The wall-clock `timeout` remains as a safeguard when called outside any task scope (`effective_token()` returns `None`).

#### Usage note — `task.scope` is cooperative-only

`task.scope` has no deadline and performs no hard abort; on the error path it issues `token.cancel()` and awaits `drain_scope` until every child exits. This follows Trio / Swift `withThrowingTaskGroup` / Kotlin `coroutineScope` / Rust `moro` / `tokio-util::TaskTracker`. Consequence: **a child that never reaches a checkpoint (e.g. `while true do end`, blocking FFI without `task.checkpoint()`) will deadlock `task.scope`.** Mitigation:

- Wrap untrusted / CPU-bound work with `task.with_timeout(ms, fn, { grace_ms = … })` — `with_timeout` is the only primitive that hard-aborts, and only after the grace window.
- Insert `task.checkpoint()` (or `task.yield()` / cancel-aware `task.sleep`) in long-running loops so the child can observe cancellation.

## [0.4.1] - 2026-04-16

### Changed

- `agent-mesh-core` / `agent-mesh-sdk` 0.3.0 → 0.3.1 via `cargo update` (upstream patch release).

## [0.4.0] - 2026-04-15

### Added

- `std.kv` Lua bridge (embedded, agent-private persistent KVS). Async API `std.kv.get/set/delete/list(ns, key?)` backed by SQLite (`__kv` table, `WITHOUT ROWID`). Namespace validated (`^[a-zA-Z0-9_\-]+$`). Shares the bridge's `spawn_blocking` + query-timeout + `sqlite3_interrupt` infrastructure with `std.sql`.
- `std.sql` Lua bridge (embedded, agent-private SQLite with WAL). Async API `std.sql.query(sql, params?) -> rows` / `std.sql.exec(sql, params?) -> { affected, last_id }`. Runs inside `tokio::task::spawn_blocking`; lock acquisition happens inside the blocking task to avoid `await`-holding-lock. Query timeout via `tokio::time::timeout` races against an `InterruptHandle` so runaway queries free the Mutex promptly.
- `std.sql.null` sentinel (`mlua::Value::NULL` = `LightUserData(null_ptr)`) exported for SQL-NULL round-trip on the Lua side. NULL columns arrive as the sentinel instead of being silently skipped, preserving the distinction between "column is NULL" and "column absent". The global `json_to_lua` also emits this sentinel for `serde_json::Value::Null`, so `kv` / `sql` / `mcp` / `llm` bridges all agree.
- `std.kv.register_tools(opts?)` and `std.sql.register_tools(opts?)` — LLM-facing tool registration helpers. Accept `{ allowed, prefix }` and register prefixed tools (`kv_get` / `kv_set` / …, `sql_query` / `sql_exec`) via `tool.register`. Return array of registered tool names.
- `tool.call(name, input)` is now async; handlers declared with `tool.register` may be async functions (Lua 5.4 coroutine boundary handled by mlua-isle). Sequential execution guaranteed via `RefCell` borrow check in the bridge.
- ENV-driven config for bridges:
  - `AGENT_BLOCK_HOME` — base dir for all on-disk state (default `~/.agent-block`).
  - `AGENT_BLOCK_KV_PATH` — override KV SQLite path (default `{base}/kv.sqlite`).
  - `AGENT_BLOCK_SQL_PATH` — override SQL SQLite path (default `{base}/db.sqlite`).
  - `AGENT_BLOCK_SQL_BUSY_TIMEOUT_MS`, `AGENT_BLOCK_SQL_QUERY_TIMEOUT_MS`, `AGENT_BLOCK_SQL_JOURNAL_MODE` — SQLite tuning.
  - `:memory:` paths short-circuit journal/PRAGMA setup for tests.
- E2E fixtures and tests: `tests/fixtures/sql_roundtrip.lua`, `tests/fixtures/sql_null.lua`, `tests/e2e_sql.rs` (NULL-sentinel round-trip), plus `examples/agent_with_sql.lua` demonstrating the LLM agent using `sql_query` / `sql_exec` tools.

### Changed

- `std.kv` internal storage migrated from JSON-file-per-namespace (`{base}/kv/{ns}.json`, whole-namespace rewrite on every mutation, no `fsync(parent_dir)`) to a single SQLite table on a dedicated `kv.sqlite` file. Eliminates the cross-process lost-update window and the full-rewrite cost. Lua API is unchanged; legacy `{base}/kv/*.json` data is **not** migrated and should be deleted.
- SQL param conversion rejects non-finite `f64` (NaN / ±Inf) with an indexed error. `run_query` on `ValueRef::Real` also errors on non-finite instead of silently lowering to NULL — serde_json cannot represent them and the prior path corrupted the round-trip.
- `e2e_agent` tests isolated via per-test `tempdir()` + `AGENT_BLOCK_HOME` env to prevent WAL init races on shared `~/.agent-block` paths under parallel `cargo test`.

### Security

- `rustls-webpki` 0.103.10 → 0.103.12 via `cargo update`. Fixes RUSTSEC-2026-0098 (name constraints wrongly accepted for URI names) and RUSTSEC-2026-0099 (wildcard certificate name constraints wrongly accepted).
- `rand` 0.9.2 → 0.9.4 via `cargo update`. Clears RUSTSEC-2026-0097 (unsound with a custom logger using `rand::rng()`) on that version. `rand` 0.8.5 remains via `agent-mesh-core 0.3` and is tracked as an allowed advisory warning pending an upstream bump.

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
