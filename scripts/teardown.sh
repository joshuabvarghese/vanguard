#!/usr/bin/env bash
# teardown.sh — removes the Kind cluster and cleans up local artifacts
set -euo pipefail

CLUSTER_NAME="vanguard"
PID_FILE="./vanguard.pid"

echo "Deleting Kind cluster '${CLUSTER_NAME}'…"
kind delete cluster --name "$CLUSTER_NAME" 2>/dev/null || true

if [[ -f "$PID_FILE" ]]; then
  PID="$(cat "$PID_FILE")"
  if kill -0 "$PID" 2>/dev/null; then
    echo "Stopping Vanguard (pid ${PID})…"
    kill "$PID" 2>/dev/null || true
    sleep 1
    kill -0 "$PID" 2>/dev/null && kill -9 "$PID" 2>/dev/null || true
  fi
  rm -f "$PID_FILE"
fi

echo "Removing binary and logs…"
rm -f ./target/release/vanguard ./vanguard.log

echo "✓ Teardown complete"
