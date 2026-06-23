#!/usr/bin/env bash
# verify.sh — fast-handler roundtrip + clean-exit verification.
#
# Complements bus-handler-grace/verify.sh. Here we test the non-pathological
# case: a handler that returns immediately. Success criteria:
#
#   1. The mesh request receives an ack whose JSON body contains the
#      marker returned by the Lua handler. This proves the bytecode
#      transfer (bus.on → Function::dump(true) → handler Isle load) and
#      the ack channel (oneshot → mesh.request) are wired end-to-end.
#   2. After the ack arrives, SIGTERM brings the process down within a
#      short budget (< 2000ms) because there is no handler in flight.
#
# Regression signal: if the ack is missing or the marker is wrong the
# end-to-end dispatch path is broken even for trivial handlers.
#
# Prerequisites — see examples/bus-handler-grace/verify.sh.

set -u

BIN=${AGENT_BLOCK_BIN:-./target/release/agent-block}
RX_SECRET=${AGENT_BLOCK_RX_SECRET:?set AGENT_BLOCK_RX_SECRET}
RX_ID=${AGENT_BLOCK_RX_ID:?set AGENT_BLOCK_RX_ID}
RELAY=${AGENT_BLOCK_RELAY:-wss://agent-mesh.fly.dev/relay/ws}
GRACE_MS=${AGENT_BLOCK_TASK_GRACE_MS:-1000}
MESHCTL=${MESHCTL_BIN:-agent-meshctl}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HANDLER="$SCRIPT_DIR/handler.lua"

# Shutdown budget: no handler is in flight when SIGTERM arrives, so the
# process should exit well below grace_ms (we pick 2s as a generous cap
# that still catches regressions like the Isle-shutdown-blocks-on-thread
# bug observed for CPU-bound handlers).
SHUTDOWN_BUDGET_MS=2000

now_ms() {
    python3 -c 'import time; print(int(time.time()*1000))'
}

cleanup() {
    if [ -n "${RPID:-}" ] && kill -0 "$RPID" 2>/dev/null; then
        kill -KILL "$RPID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "=== bus-handler-fast verify ==="
echo "binary: $BIN"
echo "handler: $HANDLER"
echo "relay: $RELAY"
echo "grace_ms: $GRACE_MS"
echo "shutdown_budget_ms: $SHUTDOWN_BUDGET_MS"
echo "receiver agent_id: $RX_ID"
echo

# 1. Start receiver.
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

# 3. Fire a mesh request (foreground) and capture the ack.
ACK_OUT=$(mktemp)
"$MESHCTL" request --target "$RX_ID" --capability busy \
    --payload '{"ping":"hello"}' > "$ACK_OUT" 2>&1
REQ_EXIT=$?
echo "[step 3] mesh request completed (meshctl exit=$REQ_EXIT)"

# 4. Validate the ack body.
MARKER="bus-handler-fast-ack"
if ! grep -q "$MARKER" "$ACK_OUT"; then
    echo "FAIL: ack did not contain marker '$MARKER'" >&2
    echo "--- meshctl output ---" >&2
    cat "$ACK_OUT" >&2
    echo "--- receiver log tail ---" >&2
    tail -20 "$LOG" >&2
    exit 1
fi
if ! grep -q '"ping"' "$ACK_OUT" || ! grep -q '"hello"' "$ACK_OUT"; then
    echo "FAIL: ack did not echo the request payload (ping=hello)" >&2
    echo "--- meshctl output ---" >&2
    cat "$ACK_OUT" >&2
    exit 1
fi
echo "[step 4] ack roundtrip verified (marker + payload echo)"

# 5. SIGTERM → measure clean-exit time.
T_START=$(now_ms)
kill -TERM "$RPID"
echo "[step 5] SIGTERM sent at t=0ms"

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

echo
echo "=== result ==="
echo "exit_code:          $EXIT"
echo "elapsed_ms:         $ELAPSED_MS"
echo "shutdown_budget_ms: $SHUTDOWN_BUDGET_MS"

# 6. Assertions.
FAIL=0
if [ "$EXIT" -ne 0 ]; then
    echo "FAIL: exit code $EXIT (expected 0)" >&2
    FAIL=1
fi
if [ "$ELAPSED_MS" -ge "$SHUTDOWN_BUDGET_MS" ]; then
    echo "FAIL: elapsed ${ELAPSED_MS}ms >= ${SHUTDOWN_BUDGET_MS}ms budget (no handler was in flight, so this should be cheap)" >&2
    FAIL=1
fi
if grep -q "Lua dispatch failed" "$LOG"; then
    echo "FAIL: 'Lua dispatch failed' in log — handler crashed" >&2
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
