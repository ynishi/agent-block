# bus-handler-grace — CPU-bound handler grace window e2e

An example that verifies, **over the public mesh relay**, that a bounded
shutdown holds when a CPU-bound Lua handler (a non-yielding tight loop)
is running through the mesh and the process receives `SIGTERM`. The bound
is `AGENT_BLOCK_TASK_GRACE_MS` (default 1000ms).

## Scope

This directory is a NATS-by-Example style runnable reference. It is not an
automated test driven by `cargo test`; it is a semi-automated e2e that a
human (or an agent) reproduces with `./verify.sh`.

See [`docs/runbooks/e2e-bus-handler.md`](../../docs/runbooks/e2e-bus-handler.md)
for the full step-by-step procedure and troubleshooting.

## Files

| File | Purpose |
|---|---|
| `handler.lua` | Registers a CPU-bound handler (`block_sync(10)`) via `bus.on("mesh", ...)` |
| `verify.sh` | Starts the receiver in the background, issues a mesh request, sends SIGTERM, measures elapsed, and asserts |
| `README.md` | This file |

## Regression baseline

| | elapsed (SIGTERM → exit) |
|---|---|
| Before handler Isle split (pre subtask 1/2) | ~10000 ms |
| After handler Isle split | < 3000 ms (threshold); measured ~1100 ms |

The ~10x gap is the value delivered by subtask 1/2. `verify.sh` is designed
to catch a regression of that gap.

## Prerequisites

- Rust toolchain with `cargo build --release -p agent-block`
- `agent-meshctl` and `agent-meshd` for the hosted instance (see
  [agent-mesh README](https://github.com/ynishi/agent-mesh))
- Receiver-side Ed25519 keypair registered with the hosted registry
- Sender-side meshd already running (Syncing state, registered under a
  different agent_id)
- `python3` (used for millisecond timestamps)

## Quick run

Pass secrets and IDs through environment variables (never inline them):

```bash
export AGENT_BLOCK_BIN=./target/release/agent-block
export AGENT_BLOCK_RX_SECRET=<64-hex>
export AGENT_BLOCK_RX_ID=<receiver-agent-id>
# optional:
# export AGENT_BLOCK_RELAY=wss://agent-mesh.fly.dev/relay/ws
# export AGENT_BLOCK_TASK_GRACE_MS=1000

./examples/bus-handler-grace/verify.sh
```

On PASS, the script prints `elapsed_ms=<~1100> < 3000` and `exit=0`.

On FAIL, the tail of the receiver log is echoed to stderr; inspect it or
look at `/tmp/tmp.*`.

## How it works

```
[verify.sh]
    │
    ├─> agent-block handler.lua (bg)  ── receives a mesh "busy" request
    │       │
    │       └─> bus.on("mesh") handler starts block_sync(10) on the handler Isle
    │
    ├─> agent-meshctl request (bg)   ── in-flight during block_sync
    │
    ├─> wait for "handler START" in the log
    ├─> kill -TERM receiver          ── t=0
    └─> measure exit elapsed         ── assert < 3000ms && exit == 0
```

## Why this does NOT live in `tests/e2e_bus.rs`

- Depends on the public relay (fly.dev); CI should skip by default.
- Sender-side meshd, registry, and ACL setup all live outside the test.
- Handler-start timing is wall-clock dependent, which hurts test determinism.

For those reasons this is run semi-automatically via the procedure in
`docs/runbooks/e2e-bus-handler.md`.

## References

- [`docs/runbooks/e2e-bus-handler.md`](../../docs/runbooks/e2e-bus-handler.md) — full step-by-step runbook plus troubleshooting
- NATS by Example: <https://github.com/ConnectEverything/nats-by-example>
