# examples/

Runnable Lua scripts demonstrating agent-block features. All scripts run via:

```bash
agent-block -s examples/<file>.lua
```

`.env` is auto-loaded from the project root (see project `CLAUDE.md` — no manual `source` needed). Required environment variables are listed per script below.

Every script in this directory is listed here. A new one belongs in a table below on the commit that adds it.

## The kernel and a loop over it

| Script | Purpose | Env |
|---|---|---|
| `knl_beat.lua` | The smallest real shell over `knl.beat`, in three sections: the plain kernel with a caller-written loop; the same run with the `policy` pack in the seams the device already has; the same again split across a `supervisor` tree. This is the reference for writing a loop of your own | `ANTHROPIC_API_KEY` |
| `fcloop.lua` / `test_fcloop.lua` | A function-call loop built directly on `http.request`, below the kernel — what the layers above are saving you from | `ANTHROPIC_API_KEY` |

## Agent basics

| Script | Purpose | Env |
|---|---|---|
| `hello.lua` / `hello_stream.lua` | Minimal `agent.run` (non-stream / stream) | `ANTHROPIC_API_KEY` |
| `test_agent.lua` | Agent + tools smoke | `ANTHROPIC_API_KEY` |
| `test_agent_log_meta.lua` | Verifies `ab.obs` log metadata fields | `ANTHROPIC_API_KEY` |
| `test_provider_switch.lua` | Anthropic ↔ OpenAI-compatible switching | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` |
| `test_prompt_cache.lua` | Anthropic prompt cache controls | `ANTHROPIC_API_KEY` |
| `test_qwen_openai.lua` | A bare OpenAI-compatible call against a Qwen vLLM endpoint | `OPENAI_API_KEY` (often dummy), `QWEN_BASE_URL`, `QWEN_MODEL?` |
| `session_chat.lua` | One CLI invocation that joins a conversation persisted through `std.kv`; run it repeatedly with the same `AGENT_ID` | `ANTHROPIC_API_KEY` |

## Storage / state

| Script | Purpose |
|---|---|
| `agent_with_kv.lua` / `agent_with_kv_v2.lua` | Agent + KV store |
| `agent_with_sql.lua` | Agent + SQL store |
| `test_ts.lua` | `std.ts.*` time-series store smoke — no external service |

## Composition / orchestration

| Script | Purpose |
|---|---|
| `agentify_flow.lua` | Agentify flow demo |
| `test_bus.lua` | Event bus smoke |

## Algocline integration

| Script | Purpose |
|---|---|
| `test_algocline.lua` | Basic algocline call |
| `test_algocline_agent.lua` | Algocline + agent wiring |
| `test_algocline_e2e.lua` | End-to-end |
| `test_algocline_pause.lua` | Pause/resume |

## MCP

| Script | Purpose |
|---|---|
| `test_mcp.lua` | MCP client smoke |
| `test_mcp_ping.lua` | `mcp.ping` keepalive + round-trip latency |
| `test_mcp_complete.lua` | `mcp.complete` over both prompt-ref and resource-ref |
| `test_mcp_resource_templates.lua` | `mcp.list_resource_templates` |
| `test_mcp_roots.lua` | `mcp.set_roots_handler` + `mcp.notify_roots_list_changed` |
| `test_mcp_elicitation.lua` | `mcp.set_elicitation_handler` — server-originated prompts |
| `mcp_resource_subscribe.lua` | All six Resource Subscribe APIs against the bundled subscribe smoke server (see root README §MCP Resource Subscribe Smoke Server) |
| `verify_echo_harness.lua` | Verification script for the bundled `echo_mcp_server` (see root README §MCP Echo Harness) |

## Exit codes

Most scripts use:

- `0` — PASS
- `1` — FAIL (assertion / verification failed)
- `2` — SKIP (required env var unset, e.g. `ANTHROPIC_API_KEY`)
