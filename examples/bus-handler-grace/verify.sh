#!/usr/bin/env bash
# verify.sh — measure SIGTERM → exit elapsed for CPU-bound bus handler.
#
# Success criteria:
#   - receiver exits with code 0 (graceful shutdown)
#   - elapsed (SIGTERM → exit) < 3000 ms
#
# Regression baseline (before handler Isle split): ~10000 ms
# Expected (after handler Isle split):             ~1000 ms + overhead
#
# Prerequisites (set in env):
#   AGENT_BLOCK_BIN        path to agent-block binary (default: ./target/release/agent-block)
#   AGENT_BLOCK_RX_SECRET  receiver Ed25519 secret (64 hex chars)
#   AGENT_BLOCK_RX_ID      receiver agent_id (for sender-side request targeting)
#   AGENT_BLOCK_RELAY      public relay URL (default: wss://agent-mesh.fly.dev/relay/ws)
#   AGENT_BLOCK_TASK_GRACE_MS  grace window in ms (default: 1000)
#   MESHCTL_BIN            path to agent-meshctl (default: agent-meshctl on PATH)

set -u

BIN=${AGENT_BLOCK_BIN:-./target/release/agent-block}
RX_SECRET=${AGENT_BLOCK_RX_SECRET:?set AGENT_BLOCK_RX_SECRET}
RX_ID=${AGENT_BLOCK_RX_ID:?set AGENT_BLOCK_RX_ID}
RELAY=${AGENT_BLOCK_RELAY:-wss://agent-mesh.fly.dev/relay/ws}
GRACE_MS=${AGENT_BLOCK_TASK_GRACE_MS:-1000}
MESHCTL=${MESHCTL_BIN:-agent-meshctl}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HANDLER="$SCRIPT_DIR/handler.lua"

now_ms() {
    python3 -c 'import time; print(int(time.time()*1000))'
}

cleanup() {
    if [ -n "${RPID:-}" ] && kill -0 "$RPID" 2>/dev/null; then
        kill -KILL "$RPID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "=== bus-handler-grace verify ==="
echo "binary: $BIN"
echo "handler: $HANDLER"
echo "relay: $RELAY"
echo "grace_ms: $GRACE_MS"
echo "receiver agent_id: $RX_ID"
echo

# 1. Start receiver in background.
LOG=$(mktemp)
AGENT_BLOCK_TASK_GRACE_MS="$GRACE_MS" \
    "$BIN" --relay "$RELAY" --secret-key "$RX_SECRET" --script "$HANDLER" \
    > "$LOG" 2>&1 &
RPID=$!
echo "[step 1] receiver started pid=$RPID (log: $LOG)"

# 2. Wait for READY.
for _ in $(seq 1 30); do
    if grep -q "READY" "$LOG"; then
        break
    fi
    sleep 0.2
done
if ! grep -q "READY" "$LOG"; then
    echo "FAIL: receiver did not reach READY within 6s" >&2
    tail -20 "$LOG" >&2
    exit 1
fi
echo "[step 2] receiver READY"

# 3. Fire a mesh request in background (will be blocked by block_sync(10)).
"$MESHCTL" request --target "$RX_ID" --capability busy --payload '{"msg":"grace-test"}' \
    > /dev/null 2>&1 &
SENDER_PID=$!
echo "[step 3] mesh request sent (sender pid=$SENDER_PID)"

# 4. Wait until the handler is in-flight.
for _ in $(seq 1 30); do
    if grep -q "handler START" "$LOG"; then
        break
    fi
    sleep 0.2
done
if ! grep -q "handler START" "$LOG"; then
    echo "FAIL: handler did not start within 6s (mesh routing failed?)" >&2
    tail -20 "$LOG" >&2
    kill "$SENDER_PID" 2>/dev/null || true
    exit 1
fi
echo "[step 4] handler is in-flight"

# 5. SIGTERM — start clock.
T_START=$(now_ms)
kill -TERM "$RPID"
echo "[step 5] SIGTERM sent at t=0ms"

# 6. Wait for receiver exit with hard timeout (10s wall).
HARD_LIMIT_MS=10000
while kill -0 "$RPID" 2>/dev/null; do
    NOW=$(now_ms)
    ELAPSED=$(( NOW - T_START ))
    if [ "$ELAPSED" -ge "$HARD_LIMIT_MS" ]; then
        echo "FAIL: receiver did not exit within ${HARD_LIMIT_MS}ms" >&2
        kill -KILL "$RPID" 2>/dev/null || true
        exit 1
    fi
    sleep 0.05
done
wait "$RPID"
EXIT=$?
T_END=$(now_ms)
ELAPSED_MS=$(( T_END - T_START ))

# 7. sender cleanup (its request will get "handler timeout" after 30s; we
# don't care about its exit code for this test).
kill "$SENDER_PID" 2>/dev/null || true
wait "$SENDER_PID" 2>/dev/null || true

echo
echo "=== result ==="
echo "exit_code:    $EXIT"
echo "elapsed_ms:   $ELAPSED_MS"
echo "grace_ms:     $GRACE_MS"
echo "threshold_ms: 3000"

# 8. Assertions.
#
# A naive check on "handler START" + elapsed < threshold is NOT enough: a
# handler that crashes immediately after logging START (e.g. due to a lost
# upvalue after bytecode transfer to the Isle) would pass in ~100ms and
# hide the real grace-window behaviour. We therefore also require:
#   - no "Lua dispatch failed" line in the log (handler did not crash)
#   - no "handler END" before SIGTERM (handler was genuinely in-flight
#     when the signal arrived, so the grace window is what bounded exit)
#   - elapsed >= grace_ms (if it's lower, the handler never actually held
#     execution for the grace window — another false-positive signature)
FAIL=0
if [ "$EXIT" -ne 0 ]; then
    echo "FAIL: exit code $EXIT (expected 0)" >&2
    FAIL=1
fi
if [ "$ELAPSED_MS" -ge 3000 ]; then
    echo "FAIL: elapsed ${ELAPSED_MS}ms >= 3000ms threshold" >&2
    FAIL=1
fi
if grep -q "Lua dispatch failed" "$LOG"; then
    echo "FAIL: handler crashed (Lua dispatch failed in log) — grace window was never tested" >&2
    FAIL=1
fi
if grep -q "handler END" "$LOG"; then
    echo "FAIL: handler END appeared in log — handler completed before SIGTERM, grace window was never tested" >&2
    FAIL=1
fi
# Lower bound: if elapsed is well below grace_ms, the handler was not
# actually blocking — shutdown returned immediately, which means nothing
# was being bounded.
LOWER_BOUND=$(( GRACE_MS / 2 ))
if [ "$ELAPSED_MS" -lt "$LOWER_BOUND" ]; then
    echo "FAIL: elapsed ${ELAPSED_MS}ms < ${LOWER_BOUND}ms (half of grace_ms=${GRACE_MS}) — handler likely never ran" >&2
    FAIL=1
fi

if [ "$FAIL" -eq 0 ]; then
    echo "PASS"
    exit 0
else
    echo
    echo "--- receiver log tail ---" >&2
    tail -30 "$LOG" >&2
    exit 1
fi
