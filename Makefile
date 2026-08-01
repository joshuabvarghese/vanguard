.PHONY: all build run run-tui run-no-tui logs stop test lint fmt check install-crd install-rbac install cluster-up cluster-down demo bootstrap teardown docker-build docker-kubeconfig docker-run docker-run-tui docker-down crdgen live-demo docker-live-demo

BINARY := ./target/release/vanguard
CLUSTER_NAME := vanguard
NAMESPACE := vanguard-system

## ─── Build ──────────────────────────────────────────────────────────────────

all: build

build:
	@echo "→ Building Vanguard (release)…"
	cargo build --release
	@echo "✓ $(BINARY) ($(shell du -sh $(BINARY) 2>/dev/null | cut -f1))"

run: build
	$(BINARY) --kubeconfig $(HOME)/.kube/config

# Stops any background instance started by bootstrap.sh first, then runs in
# the foreground with the TUI attached to this terminal.
run-tui: stop build
	$(BINARY) --kubeconfig $(HOME)/.kube/config

run-no-tui: build
	VANGUARD_NO_TUI=1 $(BINARY) --kubeconfig $(HOME)/.kube/config

logs:
	tail -f ./vanguard.log

stop:
	@if [ -f ./vanguard.pid ]; then \
	  kill "$$(cat ./vanguard.pid)" 2>/dev/null || true; \
	  rm -f ./vanguard.pid; \
	  echo "✓ Stopped background Vanguard process"; \
	else \
	  echo "No ./vanguard.pid found — nothing to stop"; \
	fi

## ─── Quality ─────────────────────────────────────────────────────────────────

test:
	cargo test --lib

fmt:
	cargo fmt

check:
	cargo fmt -- --check
	cargo build

lint:
	cargo clippy --all-targets -- -D warnings

## ─── CRD regeneration ────────────────────────────────────────────────────────

# Regenerates manifests/crd/tenantpipeline.yaml directly from the Rust
# structs (see src/bin/crdgen.rs) so the checked-in schema can never drift
# from the code.
crdgen:
	cargo run --bin crdgen > manifests/crd/tenantpipeline.yaml
	@echo "✓ manifests/crd/tenantpipeline.yaml regenerated from src/crd.rs"

## ─── Docker ───────────────────────────────────────────────────────────────────

docker-build:
	docker build -t vanguard:latest .

docker-kubeconfig:
	@chmod +x scripts/docker-kubeconfig.sh
	@./scripts/docker-kubeconfig.sh

docker-run: docker-build docker-kubeconfig
	docker compose up --build

docker-run-tui: docker-build docker-kubeconfig
	docker compose run --rm -it -e VANGUARD_NO_TUI=0 vanguard

docker-down:
	docker compose down

## ─── Cluster lifecycle ───────────────────────────────────────────────────────

cluster-up:
	kind create cluster --config manifests/kind/cluster.yaml --name $(CLUSTER_NAME) --wait 120s
	kubectl config use-context kind-$(CLUSTER_NAME)

cluster-down:
	kind delete cluster --name $(CLUSTER_NAME)

install-crd:
	kubectl apply -f manifests/crd/tenantpipeline.yaml
	kubectl wait --for=condition=Established crd/tenantpipelines.infrastructure.vanguard.io --timeout=30s

install-rbac:
	kubectl create namespace $(NAMESPACE) --dry-run=client -o yaml | kubectl apply -f -
	kubectl apply -f manifests/rbac/operator.yaml

install: install-crd install-rbac

## ─── Demo ─────────────────────────────────────────────────────────────────────

bootstrap:
	@chmod +x scripts/bootstrap.sh
	./scripts/bootstrap.sh

demo:
	@chmod +x scripts/demo-run.sh
	./scripts/demo-run.sh

teardown:
	@chmod +x scripts/teardown.sh
	./scripts/teardown.sh

## ─── Live demo (vanguard-demo binary — no cluster, no cloud) ──────────────────
# Not to be confused with `make demo` above (which seeds tenants against a
# real Kind cluster via scripts/demo-run.sh). `vanguard-demo` is a separate
# binary that runs the identical REST API / reconcile / chaos code against
# an in-memory mock backend, so it needs nothing installed at all — see the
# README's "Live demo" section.

live-demo:
	cargo run --bin vanguard-demo

docker-live-demo:
	docker build -f Dockerfile.demo -t vanguard-demo:latest .
	docker run --rm -p 8081:8081 vanguard-demo:latest
