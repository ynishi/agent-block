# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed

- `compile_loop.make()` の `conf.register` opt を削除。姉件 fix (1777461322-92442) で
  `register = false` を渡すと `extra_tools` 経由 tool が `dispatch_tool` の registry
  経路から見えなくなり 'tool not found' を引き起こすため、`tool.register` を常時呼び出す
  元の挙動に戻す (1777469900-71779)。代わりに `build_tools` に first-wins dedup を追加
  して duplicate tool エラーを防止し、`dispatch_tool` に `extra_tools` handler への直接
  fallback 経路を追加して registry 非依存の dispatch wiring を確立する。

### Added

- `blocks/compile_loop` — new Tool factory block (`blocks/compile_loop/init.lua`).
  Primary surface: `compile_loop.make(conf)` returns a `tool_def = {name, schema, handler}`
  that can be passed directly to `agent.run({extra_tools = {tool_def}})`. The compile-and-fix
  loop logic (previously inside `coding_agent`) now lives here.
  - **`compile_loop.make(conf)`** — factory function. `conf.runner` (function) is required;
    `conf.llm` is optional and inherits the parent agent's provider/model/api_key at call
    time when omitted (Crux #2: `_AGENT_LLM_CTX` stack resolution).
  - **Tool input** (`spec`, `target_file`, `lang?`) is supplied by the calling LLM at tool-call
    time; factory `conf` fixes the runner and LLM policy at registration time.
  - **Counter WF-A defence**: handler output JSON never contains `code` or `history` fields
    to prevent caller context contamination.
  - **Stagnation detection**: when `STAGNATION_WINDOW = 3` consecutive iterations produce
    identical runner `stderr`, the loop gives up immediately (`failure_reason = "stagnation"`).
  - **Verdict Gate**: loop exits with `ok=true` on first runner PASS; gives up with
    `failure_reason = "max_iters"` when the iteration ceiling is reached.
  - **Structured result**: `{ ok, iters, summary, failure_reason?, last_error?, artifact_path }`.
  - **Provider support**: `"anthropic"` and `"openai"`-compatible endpoints (vLLM, llama.cpp,
    etc.) are both fully implemented with the same K-96 field set
    (`provider`, `base_url`, `api_key`, `api_key_env`, `model`, `max_tokens`, `temperature`,
    `disable_thinking`, `timeout`).
  - **Side-effect**: `tool.register(name, schema, handler)` is called by `make()` so the
    returned `tool_def.handler` and the registry entry are identity-equal.
- `agent._llm_ctx_top()` — internal function (module-level, not public API). `agent.run()`
  now pushes `{provider, base_url, api_key, api_key_env, model}` onto a module-level
  `_AGENT_LLM_CTX` stack at call entry and pops it on return (both normal and error paths via
  `pcall`). `compile_loop` handlers call `agent._llm_ctx_top()` at tool-dispatch time to
  inherit the parent's LLM credentials when `conf.llm` is omitted.
- 5 examples rewritten to the new `compile_loop.make()` + `agent.run({extra_tools={...}})`
  API. All references to `coding_agent.run()` and `coding_agent.register_tool()` have been
  removed from their call paths:
  - `examples/test_compile_loop_parent.lua` — Anthropic Haiku parent calls the
    `compile_loop` tool with a Qwen child LLM; exercises the full tool-registry →
    child-loop → structured-result round-trip.
  - `examples/test_anthropic_compile_loop.lua` — Anthropic Haiku parent + child; verifies
    Crux #2 (LLM inheritance when `conf.llm` is omitted from `compile_loop.make()`).
  - `examples/test_qwen_compile_loop.lua` — OpenAI-compatible (Qwen) child LLM via
    `agent.run({extra_tools={compile_loop.make(...)}})`.
  - `examples/test_qwen_compile_loop_lust.lua` — Qwen child; lust spec target.
  - `examples/test_qwen_compile_loop_rust.lua` — Qwen child; cargo test runner.
- `examples/test_anthropic_compile_loop_pytest.lua` — new example: Anthropic parent +
  inline `pytest_runner` that wraps `python3 -m pytest <file> --tb=short` via `io.popen`.
  Pass judgement requires exit code 0 **and** at least one `"N passed"` count in stdout
  (`%d+ passed` pattern; exit-code-only is insufficient to reject "no tests collected").
  pytest absence is detected at startup via `python3 -m pytest --version` and exits with
  code 2 (skip signal) rather than propagating an io.popen error.
- README: added **External runner examples** mini-table under the `compile_loop` Provider
  support section, listing all 6 example files with their runner kind and provider.
- `blocks/compile_loop` Anthropic path now reads `opts.base_url` instead of using a
  hardcoded endpoint. When `opts.base_url` is supplied the Anthropic client forwards it
  as the base URL (`(opts.base_url or "https://api.anthropic.com") .. "/v1/messages"`),
  matching the existing OpenAI path behaviour. Existing callers that omit `base_url` are
  unaffected (falls back to `"https://api.anthropic.com"`).
- E2E tests `compile_loop_openai_mock_iterates_until_pass` and
  `compile_loop_anthropic_mock_iterates_until_pass` in `tests/e2e_compile_loop.rs`:
  in-process axum mock servers (OpenAI `/chat/completions` and Anthropic `/v1/messages`)
  return broken code on the first call and fixed code on the second, verifying that the
  compile-and-fix loop iterates exactly twice before passing. Both tests carry no
  `#[ignore]` and pass without `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` present
  (`api_key="dummy"` is injected inline). Supporting infra:
  `tests/common/compile_loop_openai_mock.rs`,
  `tests/common/compile_loop_anthropic_mock.rs`,
  `tests/fixtures/compile_loop_openai_mock.lua`,
  `tests/fixtures/compile_loop_anthropic_mock.lua`.
- `blocks/compile_loop` now emits `ab.obs` structured log events (`iter_start`, `iter_result`,
  `converged`, `stagnation`, `max_iters_reached`) gated by `AGENT_BLOCK_LLM_DUMP=meta|full`.
  Event lines use the fixed-order `prefix=ab.obs event=<name> component=compile_loop key=value`
  format identical to the agent block's obs schema. Existing tests are unaffected (default `off`).
  New e2e test `compile_loop_anthropic_mock_emits_obs_events` in `tests/e2e_compile_loop.rs`
  validates the PASS-path events (`iter_start`, `iter_result`, `converged`) with
  `predicate::str::contains`.

### Changed

- `blocks/coding_agent` reduced to a thin backward-compatible facade (~155 lines) over
  `blocks/compile_loop`. `coding_agent.run(opts)` and `coding_agent.register_tool(opts)`
  remain available but delegate internally to `compile_loop.make()`.
  - **Breaking**: `coding_agent.run()` return shape is now
    `{ ok, iters, summary, failure_reason?, last_error?, artifact_path }`.
    Fields `code` and `history` are no longer returned (accepted breaking change; existing
    5 examples have all been updated to the new API).
  - `runner_kind` string dispatch (`"lua"` / `"cargo"`) remains in the `coding_agent` facade
    only; `compile_loop` itself accepts only a runner function (`conf.runner`).
  - `coding_agent.register_tool()` now returns the registered tool name instead of the
    `tool_def` table.

### Fixed

- `blocks/agent` — `build_tools` now applies first-wins dedup across `lua_tools → mcp_tools →
  extra_tools` in that order, preventing duplicate tool entries when the same tool name appears
  in multiple sources (e.g. registered via `tool.register` and also passed via `extra_tools`).
- `blocks/agent` — `dispatch_tool` now holds a direct `extra_tools_map` fallback path
  (`extra_tools_map[name].handler(input)`) between the MCP path and the registry (`tool.call`)
  path. This means `extra_tools` handlers are dispatched correctly even when the tool is not
  registered in the global registry, making dispatch wiring registry-independent (1777469900-71779).
- `blocks/agent` — `build_tools` now flattens `extra_tools` entries that use the
  `compile_loop.make()` return shape (`{name, schema={description, input_schema}, handler}`)
  into the Anthropic flat form (`{name, description, input_schema}`), preventing
  `unsupported type for JSON conversion` errors when passing `compile_loop` tools
  via `agent.run({extra_tools = {...}})`. Already-flat entries pass through unchanged.

## [0.10.0] - 2026-04-28

### Added

- `agent.run()` now accepts `provider = "openai"` to route LLM calls to any
  OpenAI-compatible endpoint (vLLM, llama.cpp, OpenRouter, RunPod, etc.) while
  keeping `provider = "anthropic"` (or absent) as the unchanged default path.
  New opts: `provider` (`"anthropic" | "openai"`, default `"anthropic"`),
  `base_url` (per-call endpoint override; default for openai is
  `https://api.openai.com/v1`), `api_key` (inline key, bypasses env lookup),
  `api_key_env` (custom env-variable name; defaults: `ANTHROPIC_API_KEY` /
  `OPENAI_API_KEY`).
- OpenAI response normalizer converts `choices[0].message.tool_calls[]` to the
  internal Anthropic-shape `tool_use` content-block format so the ReAct loop
  and all `dispatch_tool` call sites require zero modification.
- `cache_control`, `context_management`, and `context_management_config` are
  Anthropic-only: when `provider = "openai"` any of these opts emit a single
  `warn`-level log line (`agent: <field> is anthropic-only; ignored for
  provider=openai`) and are silently excluded from the request body / headers.
  They remain fully operative for the Anthropic provider.
- `tool_calls[].function.arguments` JSON-parse failures in OpenAI responses are
  surfaced as `is_error = true` tool-result blocks fed back to the model for
  self-correction rather than silently dropped or causing a loop abort.
- E2E test `openai_provider_mock_tool_dispatch` in `tests/e2e_agent.rs`:
  in-process axum mock server returns a well-formed OpenAI completion with a
  single tool call, verifying that the ReAct loop dispatches the tool and
  collects the result correctly end-to-end. Runs in CI without `#[ignore]`.
- `examples/test_qwen_openai.lua` — OpenAI-compat smoke test against a self-hosted
  Qwen vLLM endpoint (`QWEN_BASE_URL` / `OPENAI_API_KEY` from env). Verifies
  ReAct tool dispatch + final content end-to-end on real hardware.
- `examples/test_provider_switch.lua` — single `agent.run()` block that flips
  between Anthropic (Haiku) and OpenAI-compat (Qwen vLLM) via `AGENT_PROVIDER`
  env, demonstrating that `tool.register` handlers and the ReAct loop are
  zero-modified across providers.

- `examples/echo_mcp_server` — standalone MCP reference server (stdio + HTTP) exposing tools
  (`echo`, `slow_echo`), resources, prompts, logging, and sampling for smoke-testing the
  agent-block MCP bridge. Run with `cargo run --example echo_mcp_server -- --help`.
- `mcp.connect_http(name, url, opts)` — connect to an MCP server over HTTP/SSE transport.
  `opts.transport = "sse" | "http"` selects SSE or Streamable HTTP (default `"http"`).
  `opts.headers` table is forwarded as request headers.
- `mcp.list_resources(name)` — list resources exposed by an MCP server.
  Returns `{ ok=true, resources=[{uri, name, description, mimeType, ...}] }`.
- `mcp.read_resource(name, uri)` — read a single resource by URI.
  Returns `{ ok=true, contents=[{uri, mimeType, text|blob}] }`.
- `mcp.list_prompts(name)` — list prompt templates exposed by an MCP server.
  Returns `{ ok=true, prompts=[{name, description, arguments}] }`.
- `mcp.get_prompt(name, prompt_name, args)` — retrieve a rendered prompt.
  Returns `{ ok=true, description, messages=[{role, content}] }`.
- `mcp.on_progress(name, handler)` — register a per-server Lua callback for
  `notifications/progress` events. `handler(token, progress, total, message)` is called
  on each progress notification from the named server. The handler must be a pure Lua
  function (C functions / Rust closures are rejected). Handler execution errors are
  logged at `warn` level and the notification is dropped rather than crashing the runtime.
- `mcp.on_log(name, handler)` — register a per-server Lua callback for
  `notifications/message` (MCP log) events. `handler(level, logger, data)` is called for
  each log notification from the named server. When no handler is registered the
  notification is forwarded to the Rust `tracing` target `"lua"` (same target as `log.*`)
  at the mapped level (debug → DEBUG, info/notice → INFO, warning → WARN,
  error/critical/alert/emergency → ERROR). Handler must be a pure Lua function;
  execution errors are logged at `warn` level and the notification is dropped.
- `mcp.cancel(name, request_id)` — send a `notifications/cancelled` notification to the
  named server for the given `request_id`. This is also fired automatically when
  `mcp.call` times out, so explicit use is only needed for manual cancellation flows.
  Failures are logged at `warn` level (fire-and-forget contract).
- `mcp.set_sampling_handler(server_name, handler)` — register a per-server Lua function
  to respond to `sampling/createMessage` requests from MCP servers. The runtime calls
  `handler(params)` where `params` matches the MCP `CreateMessageRequest` shape; the
  return value must be a table matching `CreateMessageResult`
  (`{ model, stop_reason, role, content }`). When no handler is registered the server
  receives `method_not_found` (existing default behaviour). Handler errors are returned
  to the server as `internal_error`. The signature takes `server_name` so each server
  can use a different LLM policy; a global singleton form is intentionally not provided
  to avoid multi-server dispatch collisions.
- `agent.run()` `mcp_servers` entries now accept an HTTP form:
  `{ name = "myserver", url = "https://…/mcp", transport_opts = { transport = "sse" } }`.
  When `url` is present `mcp.connect_http` is used; when `command` is present the
  existing stdio path is used. Both forms coexist in the same `mcp_servers` list.
- `agent.run({ sampling = fn })` — pass a Lua function as `opts.sampling` to
  automatically register it as the sampling handler for every MCP server connected in
  that `agent.run` call (calls `mcp.set_sampling_handler(srv.name, fn)` per server).
- `mcp.server_info(name)` — return the server's `InitializeResult` as a Lua table
  (`{ ok=true, server_info={...} }`). Exposes `capabilities`, `serverInfo`, and protocol
  version fields from the MCP handshake result.
- `agent.run({ enable_resources = true })` — automatically register
  `{server}__mcp_list_resources` and `{server}__mcp_read_resource` as LLM-callable tools
  for each connected server that declares the `resources` capability. Default `false`;
  capability-absent servers are silently skipped (logged at `info`).
- `agent.run({ enable_prompts = true })` — automatically register
  `{server}__mcp_list_prompts` and `{server}__mcp_get_prompt` as LLM-callable tools for
  each connected server that declares the `prompts` capability. Default `false`; same
  silent-skip behaviour as `enable_resources`.
- `agent.run({ on_progress = fn(ev) })` — register a Lua callback for progress
  notifications from all connected MCP servers. The callback receives an envelope table
  `{ type="progress", server, token, progress, total, message }`. No capability gate.
  Callback errors are swallowed and logged at `warn`.
- `agent.run({ progress_to_log = true })` — bridge progress notifications to `log.info`
  automatically. Ignored when `on_progress` is set (callback takes priority). Default
  `false`.
- `agent.run({ on_log = fn(ev) })` — register a Lua callback for log notifications from
  servers that declare the `logging` capability. Envelope:
  `{ type="log", server, level, logger, data }`. Servers without logging capability are
  silently skipped (logged at `info`). Callback errors are swallowed and logged at `warn`.
- `agent.run({ log_to_stderr = true })` — bridge server log notifications to
  `log.debug|info|warn|error` by severity. Ignored when `on_log` is set. Logging
  capability gate applies. Default `false`.

### Fixed

- `on_log` callback wrapper in `blocks/agent/init.lua`: added `logger = logger or ""`
  and `data_json = data_json or ""` nil-guards before envelope construction to prevent
  nil-concat crashes if argument positions shift in future refactors.
- Nil-concat crash in `__mcp_dispatch_progress` / `__mcp_dispatch_log` glue when
  `opts.on_progress` / `opts.on_log` is passed to `agent.run()`.
  Root cause: the wrapper closures registered by `connect_mcp_servers` captured `user_cb`
  and `sn` as Lua upvalues; after bytecode dump/reload across the handler Isle VM boundary
  all captured upvalues are reset to nil, so `pcall(nil, ev)` always failed and the
  subsequent `.. sn ..` concat in the error path crashed with
  `attempt to concatenate a nil value (upvalue '?')`.
  Fix: two new internal bridge functions (`mcp._store_progress_ucb` /
  `mcp._store_log_ucb`) load the user callback onto handler Isle under dedicated
  `__mcp_user_progress_cbs` / `__mcp_user_log_cbs` globals (no dispatch-registry entry).
  The envelope wrappers now read `user_cb` from those globals using only function
  parameters and `_ENV` globals — no captured upvalues.
- Belt-and-suspenders nil-guards added to the `__mcp_dispatch_progress` and
  `__mcp_dispatch_log` Lua glue strings (handler.rs): `total or "0"`, `message or ""`,
  `logger or ""`, `data_json or ""` are applied before forwarding to the handler so that
  any future regression in the Rust-side normalisation path cannot reach user callbacks
  with nil values.
- `McpManager::call_tool` now enables progress notifications by relying on rmcp's
  `AtomicU32ProgressTokenProvider`, which auto-attaches a counter-based `progressToken`
  when an `on_progress` handler is registered for the target server.

### Changed

- `src/mcp_client.rs` split into `src/mcp_client/mod.rs` + `src/mcp_client/handler.rs` module.
  All existing public API (`connect`, `list_tools`, `call_tool`, `disconnect`, `disconnect_all`,
  `new`, `with_rpc_timeout`) is unchanged. No Lua-visible behaviour change.
- `RunningService<RoleClient, ()>` unit handler replaced with
  `RunningService<RoleClient, AgentBlockClientHandler>` across all server connections.
  For this release all notification methods remain the default no-ops from rmcp;
  progress / log / sampling callback wiring is deferred to subsequent subtasks.
- `rmcp` feature flags expanded: `client-side-sse` and `transport-streamable-http-client`
  added to `Cargo.toml` (no transport code activated yet; enables Subtask 2 HTTP connect).
- Progress notifications now carry the rmcp-assigned counter `progressToken`; the
  `on_progress` callback receives it in `ev.token`. Calls to servers without a
  registered handler are unaffected.

### Security

- `sanitize_url` path: URL redaction now handles edge cases where embedded credentials
  appear in non-standard positions. The existing `[UNPARSEABLE]` fallback on `Url::parse`
  failure (introduced in 0.8.0) is retained.

## [0.8.0] - 2026-04-24

### Added

- Vendored `lshape` package under `blocks/lshape/` so `require("lshape")`
  works out of the box in agent scripts, including `lshape.luacats` codegen.
- New E2E coverage `tests/e2e_lshape.rs` + fixture `tests/fixtures/lshape_require.lua`
  to verify the vendored module loads and basic schema + LuaCATS paths execute.
- Trace context design docs and rollout checklist:
  - `docs/architecture/trace-context.md`
  - `docs/runbooks/trace-rollout-checklist.md`
- Trace context propagation across runtime bridges:
  - `http.request` auto-injects `x-trace-id` / `x-run-id` / `x-agent-id` / `x-agent-name`
    (user-provided headers win; no override).
  - `mcp.call` and `mesh.send` / `mesh.request` inject `__ab_obs` metadata
    (`trace_id`, `run_id`, `agent_id`, `agent_name`) when not already provided.
- Unified structured observability logs (`prefix=ab.obs`) for bridge events:
  - `component=http`: `http_request`, `http_response`
  - `component=mcp`: `mcp_call`, `mcp_result`
  - `component=mesh`: `mesh_send`, `mesh_request`
  - `component=tool`: `tool_register`, `tool_call`, `tool_result`
- New cross-bridge trace correlation E2E:
  - `tests/e2e_obs.rs`
  - `tests/fixtures/obs_trace_e2e.lua`
- Process-scoped auto-generated `agent_id` (UUID v4) fallback in
  `obs_context` when neither `AGENT_BLOCK_AGENT_ID` ENV nor a caller-provided
  fallback is present. Semantic scope "one agent-block execution = one
  agent_id" is documented in RustDoc, conceptually coarser than `run_id`.

### Changed

- LLM structured dump lines now emit unified `ab.obs component=llm` entries.

### Fixed

- `sanitize_url` now returns `"[UNPARSEABLE]"` on `Url::parse` failure
  instead of echoing the raw input, closing a potential secret-leak path
  for malformed URLs with embedded credentials.

### Breaking Changes

- BREAKING(obs): remove `ab.llm` legacy log line; all LLM traces now emit as `ab.obs component=llm`. The legacy line was introduced just before v0.1 and is superseded before external consumers could depend on it.

## [0.7.1] - 2026-04-23

### Added

- Structured LLM dump logging controls in `blocks/agent`: `AGENT_BLOCK_LLM_DUMP=off|meta|full`, `RUST_LOG` fallback to `meta`, production downgrade guard via `AGENT_BLOCK_ENV` (`full` → `meta` unless `AGENT_BLOCK_LLM_DUMP_ALLOW_PROD=true`), and always-redacted auth headers (`x-api-key` / `authorization`) in dump payloads.
- Fixed-order `key=value` LLM dump lines with a unique marker (`prefix=ab.llm`) and per-call events (`request` / `response` / `summary`) including correlation fields (`call`, `turn`, `iter`) and runtime signals (`latency_ms`, `stop_reason`, usage totals, tool-use count, context edit count).
- External log metadata injection for `agent.run()` via `log_meta = { trace_id, agent_id, agent_name, run_id }` with ENV fallbacks (`AGENT_BLOCK_TRACE_ID`, `AGENT_BLOCK_AGENT_ID`, `AGENT_BLOCK_AGENT_NAME`, `AGENT_BLOCK_RUN_ID`). Deprecated compatibility fallback maps `task_id` / `AGENT_BLOCK_TASK_ID` to `trace_id`.
- New runnable example `examples/test_agent_log_meta.lua` and ignored E2E coverage `agent_run_emits_structured_meta_logs` to verify structured meta-log output.
- Maintainer convenience recipes in `justfile`: `demo-llm-meta` and `e2e-llm-meta`.

### Changed

- `llm_call` API error strings no longer include raw response bodies (`API error <status>` only) to reduce accidental sensitive-data propagation through caller logs.

## [0.7.0] - 2026-04-19

### Added

- `blocks/agent` ReAct loop now enables Anthropic server-side context editing by default (`clear_tool_uses_20250919`, beta header `context-management-2025-06-27`). Default config: `trigger = 80_000` input tokens, `keep = 3` most-recent tool uses, `clear_at_least = 10_000` input tokens. Rationale: prior behaviour hit the model input-tokens ceiling and the ReAct loop stopped with a plain API error; with rolling tool-use eviction the loop continues past the trigger threshold. Works on Claude Sonnet 4 / Sonnet 4.5 / Haiku 4.5 / Opus 4 / 4.1 / 4.5 per Anthropic's context-management docs.
- `agent.run()` gains two additive opts for controlling the above:
  - `context_management` (bool, default on) — pass `false` to opt out completely (no beta header and no `body.context_management` is sent).
  - `context_management_config` (table) — replaces the default config entirely (no partial merge). The table is forwarded as `body.context_management`, so its shape matches the Anthropic request body: `{ edits = { { type = "clear_tool_uses_20250919", trigger = { type, value }, keep = { type, value }, clear_at_least = { type, value }, exclude_tools = { ... } }, ... } }`.
- `on_turn` callback payload gains an additive `context_management` key that forwards `response.context_management` from the Anthropic API (shape: `{ applied_edits = { { type = "clear_tool_uses_20250919", cleared_tool_uses = N, cleared_input_tokens = N }, ... } }`). When the server did not fire any edit on a given turn the key is absent; callbacks should nil-guard before indexing `applied_edits`. Existing callbacks that ignore unknown keys are unaffected.

## [0.6.0] - 2026-04-18

### Added

- EventBus infrastructure landing (Subtask 1 of 4) — pure-Rust core types for an upcoming reactive / long-running agent mode. New `src/bus/` module: `Event` struct (kind / id / payload / meta + `oneshot` ack sender), `Source` async trait, `EventBus` struct with a serial `run(shutdown: CancellationToken)` dispatcher loop that fans a single bounded-mpsc input out to kind-specific handlers (with an `on_any` fallback), `Handler` / `HandlerKey` trait placeholders, and `panic::catch_unwind` isolation so a faulting handler does not kill the loop. Added `BlockError::Bus(String)` variant for bus-local errors.
- EventBus handler Isle split (Subtask 2 of 4) — `bus.on(kind, fn)` / `bus.on_any(fn)` dump the Lua handler to bytecode via `Function::dump(true)` and reload it on a dedicated FullAsync Isle worker thread (`handler_isle`). The main VM keeps only a thin dispatcher that forwards `Event` to the Isle via channel. Rationale: CPU-bound Lua handlers (tight loops that do not `yield`) previously occupied the main-thread `LocalSet`, which blocked the `AGENT_BLOCK_TASK_GRACE_MS` shutdown waker. The Isle split unblocks the main thread; see also Subtask 3 below for the end-to-end grace guarantee.
- EventBus handler grace-bounded shutdown (Subtask 3 of 4) — `LuaHandler::call` now spawns via `AsyncIsle::spawn_coroutine_call` and holds a `CancelOnDrop` guard over the returned `AsyncTask`. When the grace timeout drops the dispatcher future, the guard fires `CancelToken::cancel()`; in `mlua-isle` 0.4.1 this races the coroutine future against `cancel.cancelled()` inside a biased `tokio::select!`, dropping the AsyncThread future and cascading to any awaited Rust resource (e.g. `tokio::process::Child` from `sh.exec`). `__bus_dispatch` rewritten as pure Lua (`lua.load(src).eval()`) using `std.json` for payload / meta codec, so user handlers that `coroutine.yield` (through `sh.exec`, `task.sleep`, `mesh.request`, etc.) propagate through Lua frames instead of crashing with "attempt to yield across a C-call boundary". Stress matrix on real mesh (`wss://agent-mesh.fly.dev/relay/ws`) — 3 handler shapes (CPU-bound `bus.on`, CPU-bound `bus.on_any`, `sh.exec sleep 60`) × 4 grace values (500/1000/2000/5000 ms) × 2 iterations = **24/24 PASS**, elapsed scales linearly with grace (e.g. g=1000 → 1249/1160 ms, g=5000 → 5212/5290 ms).
- Handler registration rejects non-Lua closures (C functions / Rust closures wrapped via `create_function`) with a clear `bus.on: handler must be a pure Lua function` error, because `Function::dump` only supports Lua bytecode.
- Upvalue semantics: upvalues captured by the handler closure must be serialisable to Lua bytecode — primitives / tables of primitives work, but `userdata` / `thread` / C-function upvalues fail at `dump` time. Doc comments on `bus.on` / `bus.on_any` / the `src/bridge/bus.rs` module header document the full contract.
- `examples/bus-handler-grace/` runnable example (handler.lua + verify.sh + README) and `docs/runbooks/e2e-bus-handler.md` step-by-step runbook exercise the grace window end-to-end against the public mesh relay. The runbook pattern follows NATS-by-Example (runnable) + etcd/K8s runbook (prose) conventions for AI/Agent-reproducible E2E.
- `examples/bus-handler-fast/` regression example on the non-pathological fast path.

### Changed

- `mlua-isle` dep bumped from `0.4` to `0.4.1` for the `select!`-based cancel in `execute_coroutine_{eval,call}`. Without this upstream change, a handler suspended in `.await` (rather than executing Lua bytecode) could not be preempted via the debug hook alone.

### Fixed

- **Grace window honoured end-to-end.** Subtask 2 alone (Isle split) did not bound process exit under `AGENT_BLOCK_TASK_GRACE_MS`: the main thread was unblocked as intended, but the handler Isle worker thread kept running the Lua handler to completion — `run_with_grace` emitted the `grace window exceeded; forcing exit` warn at +grace, yet the process kept the Isle alive until the Lua handler finished. Pre-fix real-mesh single-shot measurement: grace=1000ms, 60 s CPU-bound handler → elapsed **59 567 ms**. Subtask 3 (cancel-on-drop + upstream `mlua-isle` 0.4.1 `select!`) fixes this; post-fix stress matrix passes 24/24 with elapsed scaling linearly with grace.

### Deferred

- Subtask 4 acceptance tests (formal pass/fail matrix for grace / overwrite / on_any precedence) remain deferred to a follow-up release.

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
