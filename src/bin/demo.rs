//! `vanguard-demo` — the exact same REST API, reconcile loop, and chaos
//! engine as the real `vanguard` binary, wired to
//! `k8s_backend::mock::MockBackend` instead of a live Kubernetes cluster.
//!
//! This exists so the project can be demoed from a browser link with zero
//! setup. Nothing about the control-plane logic is different or simplified
//! — `demo.rs`'s poll loop calls the identical `reconcile_tenant` /
//! `handle_deletion` functions the real `kube::runtime::Controller` in
//! `operator.rs` calls; the only thing swapped out is "is there a real API
//! server to talk to." It also picks up the `cloud::IdentityVerifier` /
//! `cloud::TelemetrySink` seams the Azure-native build added to `api.rs`
//! for free, defaulted to no-ops via `ApiState::new` — no bearer token, no
//! Azure subscription, no `--features azure` needed to run this binary.

use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use vanguard::api::{self, ApiState};
use vanguard::crd::{ProxyConfig, RateLimitPolicy, TenantPipelineSpec};
use vanguard::k8s_backend::mock::MockBackend;
use vanguard::k8s_backend::mock_crd::MockCrdBackend;
use vanguard::k8s_backend::{CrdBackend, K8sBackend};
use vanguard::store::Store;
use vanguard::{chaos, demo};

const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/demo/index.html"));
const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/demo/app.js"));
const APP_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/demo/style.css"));

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let store = Arc::new(Store::new());
    store.append_log("[boot] Vanguard DEMO mode — in-memory mock backend, no real cluster".to_string());

    let crd_backend = Arc::new(MockCrdBackend::new());
    let k8s_backend = Arc::new(MockBackend::new());

    seed_demo_tenants(&crd_backend, &store).await;

    // ── Demo operator poll loop (stands in for operator.rs's Controller) ──
    {
        let crd_backend = crd_backend.clone();
        let k8s_backend = k8s_backend.clone();
        let store = store.clone();
        tokio::spawn(async move { demo::run(crd_backend, k8s_backend, store).await });
    }
    store.append_log("[boot] Demo operator → polling mock TenantPipeline store".to_string());

    // ── Chaos engine (unchanged — already backend-agnostic) ───────────────
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    {
        let engine = chaos::Engine::new(store.clone());
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move { engine.run(shutdown_rx).await });
    }
    store.append_log("[boot] Chaos engine → ready".to_string());

    // ── REST API (identical routes/handlers to the real binary) ───────────
    let crd_dyn: Arc<dyn CrdBackend> = crd_backend.clone();
    let backend_dyn: Arc<dyn K8sBackend> = k8s_backend.clone();
    // `ApiState::new` defaults identity/telemetry to the same no-op
    // implementations the real binary uses when `VANGUARD_AUTH_MODE` isn't
    // `entra` — the demo is meant to be reachable with zero setup, so it
    // never requires a bearer token or an Azure subscription.
    let api_state = Arc::new(ApiState::new(crd_dyn, backend_dyn, store.clone()));

    let demo_routes = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route(
            "/app.js",
            get(|| async { ([(header::CONTENT_TYPE, "application/javascript")], APP_JS) }),
        )
        .route(
            "/style.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css")], APP_CSS) }),
        )
        .route("/api/v1/logs", get(logs_handler))
        .with_state(store.clone());

    let app = Router::new()
        .merge(demo_routes)
        .merge(api::router(api_state));

    let port = std::env::var("PORT").unwrap_or_else(|_| "8081".to_string());
    let addr = format!("0.0.0.0:{port}");
    tracing::info!(%addr, "vanguard-demo listening");
    store.append_log(format!("[boot] Demo API + dashboard → http://{addr}"));

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    axum::serve(listener, app).await.unwrap();
}

async fn logs_handler(
    axum::extract::State(store): axum::extract::State<Arc<Store>>,
) -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "logs": store.logs() })))
}

async fn seed_demo_tenants(crd: &Arc<MockCrdBackend>, store: &Arc<Store>) {
    let seeds = [
        (
            "acme-corp",
            "Acme Corp",
            RateLimitPolicy {
                tier: "pro".into(),
                requests_per_second: 200,
                burst_capacity: 400,
                max_concurrent: 50,
            },
        ),
        (
            "initech",
            "Initech",
            RateLimitPolicy {
                tier: "enterprise".into(),
                requests_per_second: 5000,
                burst_capacity: 10000,
                max_concurrent: 500,
            },
        ),
    ];

    for (id, name, rate_limit) in seeds {
        let _ = crd
            .create(TenantPipelineSpec {
                tenant_id: id.to_string(),
                display_name: name.to_string(),
                rate_limit,
                proxy: ProxyConfig {
                    image: "envoyproxy/envoy:v1.28-latest".into(),
                    port: 10000,
                    resource_limit_milli_cpu: None,
                    resource_limit_memory_mib: None,
                },
                paused: false,
            })
            .await;
        store.append_log(format!("[boot] seeded demo tenant '{id}'"));
    }
}
