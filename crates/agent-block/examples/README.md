# examples/

Runnable Lua scripts demonstrating agent-block features. All scripts run via:

```bash
agent-block -s examples/<file>.lua
```

`.env` is auto-loaded from the project root (see project `CLAUDE.md` — no manual `source` needed). Required environment variables are listed per script below.

## compile_loop

Autonomous compile-and-fix loop. See `blocks/compile_loop/README.md` for the API and the SEARCH/REPLACE format.

### Anthropic — single-file

| Script | Scenario | Env |
|---|---|---|
| `test_anthropic_compile_loop.lua` | Single-file smoke (Crux #2: parent agent provider/model inheritance) | `ANTHROPIC_API_KEY` |
| `test_anthropic_compile_loop_pytest.lua` | Single-file with pytest runner | `ANTHROPIC_API_KEY` |
| `test_compile_loop_parent.lua` | Parent agent + compile_loop tool composition | `ANTHROPIC_API_KEY` |

### Anthropic — multi-file (`target_files` + `edit_mode = "diff"`)

| Script | Scenario | Env |
|---|---|---|
| `test_anthropic_compile_loop_multi.lua` | Add a function to both files (basic additive multi-file diff) | `ANTHROPIC_API_KEY` |
| `test_anthropic_compile_loop_multi_delete.lua` | Remove a function + assertions from both files (REPLACE-empty deletion) | `ANTHROPIC_API_KEY` |
| `test_anthropic_compile_loop_multi_selective.lua` | Edit one file only; verifies untouched file is byte-identical | `ANTHROPIC_API_KEY` |
| `test_anthropic_compile_loop_multi_stagnation.lua` | Forced-fail runner; asserts `max_iters` bound and `ok=false` return | `ANTHROPIC_API_KEY` |

### Qwen / OpenAI-compatible vLLM

Run against a Qwen vLLM endpoint (e.g. RunPod proxy). All require `OPENAI_API_KEY` (often dummy), `QWEN_BASE_URL`, optionally `QWEN_MODEL`.

| Script | Scenario |
|---|---|
| `test_qwen_compile_loop.lua` | Baseline smoke |
| `test_qwen_compile_loop_hard.lua` | Harder spec, exercises stagnation handling |
| `test_qwen_compile_loop_lust.lua` | Lua + lust testing framework |
| `test_qwen_compile_loop_rust.lua` | Rust runner |
| `test_qwen_openai.lua` | Bare Qwen OpenAI-compatible call (no compile_loop) |

## Agent basics

| Script | Purpose |
|---|---|
| `hello.lua` / `hello_stream.lua` | Minimal `agent.run` (non-stream / stream) |
| `test_agent.lua` | Agent + tools smoke |
| `test_agent_log_meta.lua` | Verifies `ab.obs` log metadata fields |
| `test_provider_switch.lua` | Anthropic ↔ OpenAI-compatible switching |
| `test_prompt_cache.lua` | Anthropic prompt cache controls |

## Storage / state

| Script | Purpose |
|---|---|
| `agent_with_kv.lua` / `agent_with_kv_v2.lua` | Agent + KV store |
| `agent_with_sql.lua` | Agent + SQL store |

## Composition / orchestration

| Script | Purpose |
|---|---|
| `fcloop.lua` / `test_fcloop.lua` | Function-call loop primitive |
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
| `verify_echo_harness.lua` | Verification script for the bundled `echo_mcp_server` (see root README §MCP) |

## Exit codes

Most scripts use:

- `0` — PASS
- `1` — FAIL (assertion / verification failed)
- `2` — SKIP (required env var unset, e.g. `ANTHROPIC_API_KEY`)
