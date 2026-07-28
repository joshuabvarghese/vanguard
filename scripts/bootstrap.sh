#!/usr/bin/env bash
# =============================================================================
#  vanguard-bootstrap.sh
#  One-shot setup: creates a Kind cluster, installs CRDs/RBAC, builds the
#  Rust binary, and starts the operator in the background (headless).
#
#  Prerequisites (all installable via Homebrew):
#    brew install kind kubectl rustup-init && rustup-init -y
#
#  Runs Vanguard as a plain background process with a pidfile and a log
#  file rather than a terminal multiplexer: if the build step fails,
#  `set -e` exits immediately with the error visible, instead of leaving you
#  to attach to a tmux session that was never created.
# =============================================================================
set -euo pipefail

CLUSTER_NAME="vanguard"
NAMESPACE="vanguard-system"
API_PORT=8081
BINARY="./target/release/vanguard"
LOG_FILE="./vanguard.log"
PID_FILE="./vanguard.pid"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${CYAN}[vanguard]${NC} $*"; }
ok()   { echo -e "${GREEN}[✓]${NC} $*"; }
warn() { echo -e "${YELLOW}[!]${NC} $*"; }
die()  { echo -e "${RED}[✗] $*${NC}"; exit 1; }

# ── 1. Verify tools ──────────────────────────────────────────────────────────
for tool in kind kubectl cargo curl; do
  command -v "$tool" &>/dev/null || die "Missing required tool: $tool"
done
ok "All required tools found"

# ── 2. Create Kind cluster ────────────────────────────────────────────────────
if kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
  warn "Kind cluster '${CLUSTER_NAME}' already exists — skipping creation"
else
  log "Creating Kind cluster '${CLUSTER_NAME}'…"
  kind create cluster \
    --config manifests/kind/cluster.yaml \
    --name "$CLUSTER_NAME" \
    --wait 120s
  ok "Cluster ready"
fi

kubectl config use-context "kind-${CLUSTER_NAME}"

# ── 3. Install CRD ───────────────────────────────────────────────────────────
log "Installing TenantPipeline CRD…"
kubectl apply -f manifests/crd/tenantpipeline.yaml
kubectl wait --for=condition=Established \
  crd/tenantpipelines.infrastructure.vanguard.io \
  --timeout=30s
ok "CRD installed and established"

# ── 4. Install RBAC ──────────────────────────────────────────────────────────
log "Installing RBAC…"
kubectl create namespace "$NAMESPACE" --dry-run=client -o yaml | kubectl apply -f -
kubectl apply -f manifests/rbac/operator.yaml
ok "RBAC configured"

# ── 5. Build binary ──────────────────────────────────────────────────────────
log "Building Vanguard binary (cargo build --release)…"
VANGUARD_VERSION=bootstrap cargo build --release
ok "Binary built: $BINARY ($(du -sh "$BINARY" | cut -f1))"

# ── 6. Start in the background ───────────────────────────────────────────────
if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  warn "Vanguard already running (pid $(cat "$PID_FILE")) — stopping it first"
  kill "$(cat "$PID_FILE")" 2>/dev/null || true
  sleep 1
fi

log "Starting Vanguard in the background…"
VANGUARD_NO_TUI=1 VANGUARD_VERSION=bootstrap nohup "$BINARY" --kubeconfig "$HOME/.kube/config" \
  > "$LOG_FILE" 2>&1 &
echo $! > "$PID_FILE"

# ── 7. Wait for the API to actually come up before declaring success ────────
log "Waiting for the API on :${API_PORT}…"
for _ in $(seq 1 30); do
  if curl -sf "http://localhost:${API_PORT}/healthz" &>/dev/null; then
    ok "API is up"
    break
  fi
  if ! kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    die "Vanguard exited immediately — check ${LOG_FILE}:\n$(tail -n 20 "$LOG_FILE")"
  fi
  sleep 1
done

if ! curl -sf "http://localhost:${API_PORT}/healthz" &>/dev/null; then
  die "API never came up after 30s — check ${LOG_FILE}:\n$(tail -n 20 "$LOG_FILE")"
fi

echo ""
ok "════════════════════════════════════════════════════════"
ok "  Vanguard is running (pid $(cat "$PID_FILE"))"
ok "  API:        http://localhost:${API_PORT}"
ok "  Logs:       tail -f ${LOG_FILE}"
ok "  TUI:        make run-tui   (kills this background instance, runs in-terminal)"
ok "  Demo:       ./scripts/demo-run.sh"
ok "  Stop:       ./scripts/teardown.sh"
ok "════════════════════════════════════════════════════════"
