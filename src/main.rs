//! Vanguard — Cloud-Native Control Plane & Kubernetes Operator (Rust port)
//!
//! Startup sequence mirrors the Go version's `cmd/vanguard/main.go`:
//!  1. Build the shared in-memory Store + event bus
//!  2. Build a kube::Client (honours --kubeconfig / KUBECONFIG / in-cluster)
//!  3. Start the operator's Controller watch loop (tokio task)
//!  4. Start the REST API server (tokio task)
//!  5. Start the Chaos Engine (tokio task)
//!  6. Start the TUI Flight Deck (tokio task, unless VANGUARD_NO_TUI=1)
//!  7. Wait for SIGINT/SIGTERM, then shut everything down gracefully

use k8s_backend::{KubeBackend, KubeCrdBackend};
use kube::Client;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use vanguard::cloud::{IdentityVerifier, TelemetrySink};
use vanguard::{api, chaos, cloud, k8s_backend, operator, store, tui};

/// Picks the identity verifier for the REST API. Defaults to
/// `NoopIdentityVerifier` (unauthenticated — the right default for `make
/// demo-run` against Kind). Set `VANGUARD_AUTH_MODE=entra` plus
/// `VANGUARD_ENTRA_TENANT_ID`/`VANGUARD_ENTRA_AUDIENCE` (compiled in only
/// under `--features azure`) to require real Entra ID access tokens — see
/// `cloud::azure::identity` and `infra/bicep/main.bicep` for the App
/// Registration this expects.
fn build_identity_verifier() -> Arc<dyn IdentityVerifier> {
    #[cfg(feature = "azure")]
    {
        if std::env::var("VANGUARD_AUTH_MODE").as_deref() == Ok("entra") {
            match cloud::azure::identity::from_env() {
                Some(verifier) => {
                    tracing::info!("REST API auth: Entra ID (VANGUARD_AUTH_MODE=entra)");
                    return verifier;
                }
                None => {
                    eprintln!(
                        "VANGUARD_AUTH_MODE=entra set but VANGUARD_ENTRA_TENANT_ID / \
                         VANGUARD_ENTRA_AUDIENCE missing — refusing to start unauthenticated"
                    );
                    std::process::exit(1);
                }
            }
        }
    }
    Arc::new(cloud::mock::NoopIdentityVerifier)
}

/// Resolves the control-plane's own secrets. Defaults to an empty
/// in-memory provider (nothing needs it unless `VANGUARD_KEYVAULT_URL` is
/// set). When set (with `--features azure`), fetches from Key Vault via
/// Workload Identity — see `cloud::azure::keyvault`.
fn build_secrets_provider() -> Arc<dyn cloud::SecretsProvider> {
    #[cfg(feature = "azure")]
    {
        if let Ok(vault_url) = std::env::var("VANGUARD_KEYVAULT_URL") {
            match cloud::azure::default_credential()
                .and_then(|cred| cloud::azure::keyvault::AzureKeyVaultSecrets::new(&vault_url, cred))
            {
                Ok(kv) => {
                    tracing::info!(vault_url, "control-plane secrets: Azure Key Vault");
                    return Arc::new(kv);
                }
                Err(e) => {
                    eprintln!("failed to init Key Vault client for {vault_url:?}: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
    Arc::new(cloud::mock::InMemorySecretsProvider::new())
}

/// Picks the telemetry sink. Set `VANGUARD_APPINSIGHTS_CONNECTION_STRING`
/// (with `--features azure`) to export business events to Azure Monitor;
/// otherwise events are dropped, matching plain local dev.
fn build_telemetry_sink() -> Arc<dyn TelemetrySink> {
    #[cfg(feature = "azure")]
    {
        if std::env::var("VANGUARD_APPINSIGHTS_CONNECTION_STRING").is_ok() {
            return Arc::new(cloud::azure::monitor::AzureMonitorTelemetrySink);
        }
    }
    Arc::new(cloud::mock::NoopTelemetrySink)
}

/// Installs tracing → Azure Monitor export when
/// `VANGUARD_APPINSIGHTS_CONNECTION_STRING` is set (and the binary was
/// built with `--features azure`); otherwise falls back to the plain
/// stdout `fmt` subscriber this project always used. Returns the
/// `TracerProvider` when the Azure path was taken, so `main` can flush it
/// on shutdown instead of dropping the last batch of exported spans.
#[cfg(feature = "azure")]
fn init_tracing() -> Option<opentelemetry_sdk::trace::TracerProvider> {
    if let Ok(conn) = std::env::var("VANGUARD_APPINSIGHTS_CONNECTION_STRING") {
        match cloud::azure::monitor::init(&conn) {
            Ok(provider) => return Some(provider),
            Err(e) => {
                eprintln!("failed to init Azure Monitor tracing: {e} — falling back to stdout");
            }
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();
    None
}

#[cfg(not(feature = "azure"))]
fn init_tracing() -> Option<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();
    None
}

/// Reads `CARGO_PKG_VERSION` (from Cargo.toml), overridable via
/// `VANGUARD_VERSION`, so CI can stamp a commit SHA — the same role the Go
/// build's `-ldflags -X main.version=...` played.
fn version() -> String {
    std::env::var("VANGUARD_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

/// Minimal manual flag parsing: just `--kubeconfig <path>`. Anything more
/// than that one flag isn't worth a dependency — see the Cargo.toml comment
/// on why `clap` was dropped from this port.
fn parse_kubeconfig_flag() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    for i in 0..args.len() {
        if args[i] == "--kubeconfig" {
            return args.get(i + 1).cloned();
        }
        if let Some(v) = args[i].strip_prefix("--kubeconfig=") {
            return Some(v.to_string());
        }
    }
    None
}

#[tokio::main]
async fn main() {
    let _tracer_provider = init_tracing();

    // ── kubeconfig resolution: --kubeconfig flag > KUBECONFIG env > default ──
    if let Some(path) = parse_kubeconfig_flag() {
        std::env::set_var("KUBECONFIG", path);
    }

    let client = match Client::try_default().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("unable to load kubeconfig: {e}");
            std::process::exit(1);
        }
    };

    let store = Arc::new(store::Store::new());
    let ver = version();
    store.append_log(format!(
        "[boot] Vanguard control plane starting… (version {ver})"
    ));

    // Control-plane's own secrets (Key Vault when VANGUARD_KEYVAULT_URL is
    // set, in-memory no-op otherwise). Resolved once at boot and probed
    // with a single read so a misconfigured Workload Identity / RBAC grant
    // fails fast here rather than surfacing later as an obscure error the
    // first time something downstream actually needs a secret.
    let secrets = build_secrets_provider();
    if std::env::var("VANGUARD_KEYVAULT_URL").is_ok() {
        match secrets.get_secret("vanguard-admin-bootstrap-token").await {
            Ok(_) => store.append_log(
                "[boot] Key Vault    → reachable, bootstrap secret resolved".to_string(),
            ),
            Err(e) => store.append_log(format!(
                "[boot] Key Vault    → WARNING: {e} (continuing; only the API auth/tracing \
                 paths are hard startup requirements)"
            )),
        }
    }

    // ── Graceful shutdown plumbing ────────────────────────────────────────────
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // ── Operator (Controller watch loop) ─────────────────────────────────────
    {
        let client = client.clone();
        let store = store.clone();
        tokio::spawn(async move {
            operator::run(client, store).await;
        });
    }
    store.append_log("[boot] Operator    → watching TenantPipeline CRDs".to_string());

    // ── REST API ──────────────────────────────────────────────────────────────
    let api_addr = std::env::var("VANGUARD_API_ADDR").unwrap_or_else(|_| ":8081".to_string());
    let bind_addr = if let Some(port) = api_addr.strip_prefix(':') {
        format!("0.0.0.0:{port}")
    } else {
        api_addr.clone()
    };
    {
        let crd_backend: Arc<dyn k8s_backend::CrdBackend> =
            Arc::new(KubeCrdBackend::new(client.clone()));
        let backend: Arc<dyn k8s_backend::K8sBackend> = Arc::new(KubeBackend::new(client.clone()));
        let state = Arc::new(api::ApiState {
            crd: crd_backend,
            backend,
            store: store.clone(),
            identity: build_identity_verifier(),
            telemetry: build_telemetry_sink(),
        });
        let app = api::router(state);
        let bind_addr = bind_addr.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(error = %e, addr = %bind_addr, "failed to bind API listener");
                    return;
                }
            };
            tracing::info!(addr = %bind_addr, "starting REST API");
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await;
        });
    }
    store.append_log(format!("[boot] API server  → {api_addr}"));

    // ── Chaos engine ──────────────────────────────────────────────────────────
    {
        let engine = chaos::Engine::new(store.clone());
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            engine.run(shutdown_rx).await;
        });
    }
    store.append_log("[boot] Chaos engine → ready".to_string());

    // ── TUI Flight Deck ───────────────────────────────────────────────────────
    let no_tui = std::env::var("VANGUARD_NO_TUI").as_deref() == Ok("1");
    let tui_handle = if !no_tui {
        let store = store.clone();
        let ver = ver.clone();
        let shutdown_rx = shutdown_rx.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = tui::run(store, ver, shutdown_rx).await {
                tracing::error!(error = %e, "TUI exited with error");
            }
        }))
    } else {
        None
    };
    store.append_log("[boot] TUI          → flight deck initialised".to_string());

    // ── Wait for shutdown signal ──────────────────────────────────────────────
    wait_for_shutdown_signal().await;
    tracing::info!("shutdown signal received, stopping…");
    let _ = shutdown_tx.send(true);

    if let Some(handle) = tui_handle {
        let _ = handle.await;
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
