# Runbook: e2e-bus-handler — CPU-bound handler grace window verify

**Goal**: verify that a CPU-bound (non-yielding) Lua handler registered via
`bus.on(kind, fn)` in `agent-block` has its SIGTERM teardown bounded by
`AGENT_BLOCK_TASK_GRACE_MS`, **over the public mesh relay**.

**Scope**: handler Isle split (see `CHANGELOG.md` Unreleased / Subtask 1 + 2 of 4).

**When to run**: as a regression check whenever the handler dispatch path is
touched — `src/bridge/bus.rs`, `src/host.rs::HostContext`, `src/bridge/mesh.rs`.

**Duration**: 5–10 minutes the first time, 1–2 minutes thereafter.

## Prerequisites

### 1. Binaries

```sh
# agent-block (receiver side)
cargo build --release -p agent-block
# → ./target/release/agent-block

# agent-meshctl + agent-meshd (sender side)
# cf. https://github.com/ynishi/agent-mesh/blob/main/README.md
cargo install --path crates/agent-meshctl
cargo install --path crates/agent-meshd
```

### 2. Keypairs & registration

Generate an Ed25519 keypair for the receiver and the sender, and register
each with the hosted registry:

```sh
# Receiver
agent-meshctl keygen
# → Agent ID:   <RX_ID>
# → Secret Key: <RX_SECRET>
agent-meshctl register --name "agent-block-receiver" --capabilities "busy" --secret-key <RX_SECRET>

# Sender (distinct keypair)
agent-meshctl keygen
# → Agent ID:   <TX_ID>
# → Secret Key: <TX_SECRET>
agent-meshctl register --name "agent-block-tester" --capabilities "test" --secret-key <TX_SECRET>
```

### 3. Sender meshd

Start the sender-side meshd in another terminal:

```sh
agent-meshd \
    --relay wss://agent-mesh.fly.dev/relay/ws \
    --cp-url https://agent-mesh.fly.dev \
    --secret-key <TX_SECRET> \
    --local-agent http://127.0.0.1:9999
```

Confirm `State: Connected` (or `Syncing` with peers online) via
`agent-meshctl status`.

## Procedure

### Step 1 — Environment variables

```sh
export AGENT_BLOCK_BIN=./target/release/agent-block
export AGENT_BLOCK_RX_SECRET=<RX_SECRET>
export AGENT_BLOCK_RX_ID=<RX_ID>
# optional:
# export AGENT_BLOCK_RELAY=wss://agent-mesh.fly.dev/relay/ws
# export AGENT_BLOCK_TASK_GRACE_MS=1000
```

### Step 2 — Run verify.sh

```sh
./examples/bus-handler-grace/verify.sh
```

### Step 3 — Read the result

PASS:
```
=== result ===
exit_code:    0
elapsed_ms:   1100    (e.g. 1000ms grace + 100ms overhead)
grace_ms:     1000
threshold_ms: 3000
PASS
```

FAIL:
- stderr contains a reason line plus the tail of the receiver log
- Match against the Troubleshooting sections below

## Troubleshooting

### A. `receiver did not reach READY within 6s`

**Symptom**: `READY` never appears in the receiver log.

**Cause**:
1. `--relay` URL is wrong, or fly.dev is unreachable
2. `RX_SECRET` is invalid
3. `agent-block` release build is missing

**Diagnose**:
```sh
# Confirm the binary exists
ls -la $AGENT_BLOCK_BIN

# Launch manually so the log is visible
AGENT_BLOCK_TASK_GRACE_MS=1000 \
    $AGENT_BLOCK_BIN --relay wss://agent-mesh.fly.dev/relay/ws \
    --secret-key $AGENT_BLOCK_RX_SECRET \
    --script ./examples/bus-handler-grace/handler.lua

# fly.dev health
curl -s https://agent-mesh.fly.dev/status | head
```

### B. `handler did not start within 6s`

**Symptom**: `READY` is logged, but `handler START` is not.

**Cause**: the mesh request never reached the receiver.

**Diagnose**:
```sh
# Both agents should be live in the registry
agent-meshctl discover

# ACL (default-deny requires an explicit sender→receiver allow)
agent-meshctl acl list

# Sender-side meshd status
agent-meshctl status
```

**Fix**: confirm sender and receiver are in the same group via
`agent-meshctl group list`. Otherwise add an explicit ACL rule:
```sh
agent-meshctl acl add --source <TX_ID> --target <RX_ID> --capabilities busy
```

**Common pitfall — stale Noise session on the sender meshd**: if you issue a
request immediately after restarting the receiver (`agent-block`), the sender
meshd may still be holding the previous Noise session and send an encrypted
payload that the receiver cannot decrypt. The receiver logs
`WARN encrypted msg but no session for <sender_id>` and the event is
dropped. Fix: restart the sender meshd to evict the cached session.

```sh
pkill -f "agent-meshd --relay"
agent-meshd --relay wss://agent-mesh.fly.dev/relay/ws \
    --cp-url https://agent-mesh.fly.dev \
    --secret-key $TX_SECRET \
    --local-agent http://127.0.0.1:9999 > /tmp/meshd.log 2>&1 &
```

### C. `elapsed >= 3000 ms`

**Symptom**: `exit_code=0` but elapsed exceeds the threshold.

**Cause (important regression)**: the handler Isle split is broken. The main
Isle `LocalSet` is being occupied by the CPU-bound handler again.

**Diagnose**:
1. Check that `src/host.rs::spawn_handler_isle` actually runs (grep the
   tracing log for `handler_isle.spawn`)
2. Confirm `bus.on` in `src/bridge/bus.rs` still goes through
   `create_async_function` + `handler_isle.exec`
3. Verify `LuaHandler { isle }` points at `host_ctx.handler_isle` rather than
   the main Isle

**Fix**: inspect the subtask 1 / 2 diffs (`git show 442e3a1 a1a09c0`) and
identify the regressing commit.

### D. `exit code != 0`

**Symptom**: elapsed is fine but the exit code is non-zero (killed by signal,
etc.).

**Cause**: the `tokio::signal::ctrl_c` / SIGTERM handler did not take the
graceful path — likely a panic along the way.

**Diagnose**: tail the receiver log and look for a panic backtrace.

## Expected baselines

| | elapsed (SIGTERM → exit) | threshold | verdict |
|---|---|---|---|
| Before subtask 1/2 | ~10000 ms | < 3000 ms | FAIL (expected) |
| After subtask 1/2 | ~1100 ms | < 3000 ms | PASS |

## Related

- `examples/bus-handler-grace/` — runnable reference driven by this runbook
- `src/bus/` — EventBus dispatcher (main thread)
- `src/bridge/bus.rs` — `bus.on` / `bus.on_any` Lua surface (bytecode forwarding)
- `src/host.rs::HostContext` — handler Isle spawn and wiring
- `CHANGELOG.md` — Unreleased section notes the handler Isle split
