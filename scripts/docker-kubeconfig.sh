#!/usr/bin/env bash
# =============================================================================
#  docker-kubeconfig.sh
#  Generates a kubeconfig that resolves the Kind control-plane by its
#  Docker-network hostname (not localhost:PORT), so the Vanguard container
#  can reach it over the shared `kind` Docker network. Run this once after
#  `make cluster-up install`, and again any time you recreate the cluster.
# =============================================================================
set -euo pipefail

CLUSTER_NAME="vanguard"
OUT_DIR=".kube"
OUT_FILE="${OUT_DIR}/config.internal"

command -v kind &>/dev/null || { echo "kind is not installed" >&2; exit 1; }

if ! kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
  echo "Kind cluster '${CLUSTER_NAME}' not found — run 'make cluster-up install' first." >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
kind get kubeconfig --internal --name "$CLUSTER_NAME" > "$OUT_FILE"

echo "✓ Wrote ${OUT_FILE} (resolves ${CLUSTER_NAME}-control-plane on the 'kind' Docker network)"
echo "  Now run:  docker compose up --build"
