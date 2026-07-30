# =============================================================================
#  Dockerfile — Vanguard control-plane / operator binary (Rust)
#
#  Multi-stage build: compile with the full Rust toolchain, ship only the
#  stripped static-ish binary + CA certs (needed to dial the Kubernetes API
#  server over TLS via rustls) in a minimal Debian-slim runtime, non-root.
#
#  Build:  docker build -t vanguard:latest .
#  Run:    docker run --rm -p 8081:8081 \
#            -v ~/.kube/config:/home/vanguard/.kube/config:ro \
#            -e KUBECONFIG=/home/vanguard/.kube/config \
#            vanguard:latest
#  (see docker-compose.yml for the full demo setup against a Kind cluster)
# =============================================================================

# ── Build stage ───────────────────────────────────────────────────────────────
FROM rust:1.97.0-bookworm AS builder
WORKDIR /src

# Cache dependency compilation separately from source changes: build a dummy
# main.rs against just Cargo.toml/Cargo.lock first.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs \
    && cargo build --release 2>/dev/null || true \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r vanguard && useradd -r -g vanguard -m -d /home/vanguard vanguard

COPY --from=builder /src/target/release/vanguard /usr/local/bin/vanguard

USER vanguard
WORKDIR /home/vanguard

# The TUI needs an interactive TTY (ratatui AltScreen); containers are
# usually run detached, so default to headless mode. Override with
# `-e VANGUARD_NO_TUI=0 -it` to see the Flight Deck inside the container.
ENV VANGUARD_NO_TUI=1
ENV VANGUARD_API_ADDR=:8081
ENV RUST_LOG=info

EXPOSE 8081

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -sf http://127.0.0.1:8081/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/vanguard"]
