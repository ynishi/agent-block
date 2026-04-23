# Runbook: Trace Context Rollout Checklist

Use this checklist when implementing and validating runtime-wide trace context
propagation.

## Phase 1 — Context Core

- [ ] Canonicalize keys to `trace_id`, `run_id`, `agent_id`, `agent_name`
- [ ] Keep `task_id` compatibility fallback with deprecation warning
- [ ] Document resolution order (`opts.log_meta > env > runtime fallback`)
- [ ] Add unit tests for fallback + warning behavior

## Phase 2 — Logging Contract

- [ ] Ensure fixed-order `key=value` output for all trace-bearing log lines
- [ ] Ensure escaping rules are deterministic (spaces / `=` / empty string)
- [ ] Keep current `ab.llm` marker stable (or add compatibility alias if renamed)
- [ ] Add snapshot/substring tests for key-order invariants

## Phase 3 — Bridge Propagation

- [ ] `http.*`: outbound header injection + request/response trace logs
- [ ] `mcp.*`: metadata envelope propagation + trace logs
- [ ] `mesh.*`: metadata propagation for send/request + inbound preservation
- [ ] `tool.*`: dispatch logs and optional read-only handler context

## Phase 4 — End-to-End Validation

- [ ] Add e2e script spanning multi-bridge path (`llm -> tool -> http|mcp|mesh`)
- [ ] Assert a single `trace_id` across all observed component logs
- [ ] Verify fallback path (`AGENT_BLOCK_TASK_ID`) still maps to `trace_id`
- [ ] Verify production-safe defaults remain unchanged (`LLM_DUMP=off` by default)

## Operational Readiness

- [ ] Add log-query examples (`trace_id=...`) for common backends
- [ ] Add troubleshooting section for missing/empty trace fields
- [ ] Decide deprecation removal release for `task_id` fallback
