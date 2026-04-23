# Trace Context Architecture

This document defines the runtime-wide observability context contract for
`agent-block`.

## Goal

Standardize correlation metadata so every bridge (`llm`, `http`, `mcp`,
`mesh`, `tool`) can be joined by the same identifiers in log backends.

## Non-goals

- `agent-block` does not own workflow/task trees.
- `agent-block` does not model external orchestration lifecycles.
- `agent-block` does not carry arbitrary user payload in trace metadata.

## Canonical Keys

The canonical context is:

- `trace_id` (required by policy; generated when absent if allowed)
- `run_id` (recommended)
- `agent_id` (recommended)
- `agent_name` (optional label)

`task_id` is deprecated and accepted only as a compatibility fallback mapped to
`trace_id`.

## Resolution Order

For each key:

1. `agent.run({ log_meta = { ... } })`
2. Environment variable (`AGENT_BLOCK_TRACE_ID`, etc.)
3. Runtime fallback (for `agent_id`, `std.env.agent_id()`; for others, nil unless policy requires generation)

## Runtime Contract

Each runtime event MUST emit fixed-order key/value logs:

1. `prefix=ab.llm` (current family marker; planned generalization to `ab.obs`)
2. `event=<request|response|summary|...>`
3. Correlation fields in fixed order: `trace_id`, `agent_id`, `agent_name`, `run_id`
4. Component-specific fields

Values containing spaces or `=` must be escaped as JSON strings.

## Bridge Propagation Rules (Target State)

- `llm`:
  - emit context on request/response/summary logs
- `http`:
  - add outbound headers: `x-trace-id`, `x-run-id`, `x-agent-id`, `x-agent-name`
  - emit request/response logs with the same context
- `mcp`:
  - pass context via metadata envelope (or reserved argument field if metadata channel is unavailable)
  - emit call/result logs
- `mesh`:
  - include context under message metadata
  - preserve incoming context when relaying
- `tool`:
  - inject context into tool-dispatch log events
  - expose read-only context to handlers in a consistent Lua API

## Safety Constraints

- Never include secrets in trace context.
- Keep context values short and printable (`[A-Za-z0-9._:-]` preferred).
- If validation fails, log warning and either sanitize or drop the value.

## Migration Policy

- `task_id` fallback remains supported temporarily with warning:
  - `log_meta.task_id -> trace_id`
  - `AGENT_BLOCK_TASK_ID -> AGENT_BLOCK_TRACE_ID`
- Remove fallback in a future minor release after deprecation window.
