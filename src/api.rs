//! REST control-plane HTTP API, ported from `internal/api/server.go`.
//! Clients POST tenant specs here; handlers translate requests into
//! TenantPipeline CRD objects via `CrdBackend`, which the operator's
//! reconcile loop (in `operator.rs`) then picks up from the Kubernetes API
//! server's watch stream.

use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;

use crate::cloud::mock::{NoopIdentityVerifier, NoopTelemetrySink};
use crate::cloud::{CallerIdentity, IdentityVerifier, TelemetrySink};
use crate::crd::{ProxyConfig, RateLimitPolicy, TenantPipelineSpec};
use crate::k8s_backend::{CrdBackend, K8sBackend};
use crate::store::{Event, EventKind, Store, TenantRecord};

pub struct ApiState {
    pub crd: Arc<dyn CrdBackend>,
    pub backend: Arc<dyn K8sBackend>,
    pub store: Arc<Store>,
    /// Verifies the `Authorization: Bearer …` header on every route except
    /// `/healthz`. Defaults to `NoopIdentityVerifier` (accept everything)
    /// so local dev / `make demo-run` and every existing test are
    /// unaffected; production sets this to
    /// `cloud::azure::identity::EntraIdVerifier` via `VANGUARD_AUTH_MODE=entra`
    /// in `main.rs`. See `cloud/mod.rs` for why this is a trait rather than
    /// an `if cfg!(feature = "azure")` scattered through the handlers below.
    pub identity: Arc<dyn IdentityVerifier>,
    /// Fans out business-level events (tenant lifecycle, chaos) to Azure
    /// Monitor when configured; a no-op otherwise. See `cloud/mod.rs`.
    pub telemetry: Arc<dyn TelemetrySink>,
}

impl ApiState {
    /// Convenience constructor for the common case (real K8s backends,
    /// no auth/telemetry configured) — used by `main.rs` when
    /// `VANGUARD_AUTH_MODE` isn't set to `entra`.
    pub fn new(crd: Arc<dyn CrdBackend>, backend: Arc<dyn K8sBackend>, store: Arc<Store>) -> Self {
        Self {
            crd,
            backend,
            store,
            identity: Arc::new(NoopIdentityVerifier),
            telemetry: Arc::new(NoopTelemetrySink),
        }
    }
}

/// Extracts and verifies the bearer token via `state.identity`, rejecting
/// with `401` on failure. Skips `/healthz` so load balancers / AKS
/// liveness probes never need a credential. Runs before route handlers,
/// which read the resulting `CallerIdentity` back out of request
/// extensions if they need to authorize a specific role (see
/// `create_tenant`'s `Tenant.Write` check).
async fn auth_middleware(
    State(state): State<Arc<ApiState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    let identity = state
        .identity
        .verify(token)
        .await
        .map_err(|e| err(StatusCode::UNAUTHORIZED, format!("unauthorized: {e}")))?;

    req.extensions_mut().insert(identity);
    Ok(next.run(req).await)
}

fn require_role(identity: &CallerIdentity, role: &str) -> Result<(), ApiError> {
    if identity.roles.iter().any(|r| r == role) {
        Ok(())
    } else {
        Err(err(
            StatusCode::FORBIDDEN,
            format!("caller {:?} missing required role {role:?}", identity.subject),
        ))
    }
}

pub fn router(state: Arc<ApiState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(Any);

    let authenticated = Router::new()
        .route("/api/v1/tenants", post(create_tenant).get(list_tenants))
        .route(
            "/api/v1/tenants/:tenant_id",
            get(get_tenant).delete(delete_tenant),
        )
        .route("/api/v1/tenants/:tenant_id/policy", patch(update_policy))
        .route(
            "/api/v1/tenants/:tenant_id/chaos/kill-proxy",
            post(chaos_kill_proxy),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(authenticated)
        .layer(cors)
        .layer(TimeoutLayer::new(Duration::from_secs(10)))
        .with_state(state)
}

// ─── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateTenantRequest {
    #[serde(rename = "tenantId", default)]
    pub tenant_id: String,
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    #[serde(rename = "rateLimit", default)]
    pub rate_limit: RateLimitPolicy,
    #[serde(default)]
    pub proxy: ProxyConfig,
}

#[derive(Deserialize)]
pub struct UpdatePolicyRequest {
    pub tier: String,
    #[serde(rename = "requestsPerSecond")]
    pub requests_per_second: i32,
    #[serde(rename = "burstCapacity")]
    pub burst_capacity: i32,
    #[serde(rename = "maxConcurrent")]
    pub max_concurrent: i32,
}

#[derive(Serialize)]
pub struct TenantResponse {
    #[serde(rename = "tenantId")]
    pub tenant_id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub phase: String,
    pub namespace: String,
    pub tier: String,
    #[serde(rename = "requestsPerSecond")]
    pub rps: i32,
    #[serde(rename = "burstCapacity")]
    pub burst: i32,
    #[serde(rename = "maxConcurrent")]
    pub max_concurrent: i32,
    #[serde(rename = "reconcileCount")]
    pub reconcile_count: i64,
    #[serde(rename = "lastReconcileAt", skip_serializing_if = "Option::is_none")]
    pub last_reconcile_at: Option<DateTime<Utc>>,
}

impl From<TenantRecord> for TenantResponse {
    fn from(rec: TenantRecord) -> Self {
        Self {
            tenant_id: rec.tenant_id,
            display_name: rec.display_name,
            phase: rec.phase.as_str().to_string(),
            namespace: rec.namespace,
            tier: rec.tier,
            rps: rec.rps,
            burst: rec.burst,
            max_concurrent: rec.max_concurrent,
            reconcile_count: rec.reconcile_count,
            last_reconcile_at: rec.last_reconcile_at,
        }
    }
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(code, msg.into())
}

// ─── Handlers ───────────────────────────────────────────────────────────────────

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "time": Utc::now().to_rfc3339() }))
}

async fn create_tenant(
    State(state): State<Arc<ApiState>>,
    identity: axum::extract::Extension<CallerIdentity>,
    Json(req): Json<CreateTenantRequest>,
) -> Result<Response, ApiError> {
    require_role(&identity, "Tenant.Write")?;
    if req.tenant_id.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "tenantId is required"));
    }

    let spec = TenantPipelineSpec {
        tenant_id: req.tenant_id.clone(),
        display_name: req.display_name,
        rate_limit: req.rate_limit,
        proxy: req.proxy,
        paused: false,
    };

    state.crd.create(spec).await.map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("k8s create: {e}"),
        )
    })?;

    state.store.publish(Event::new(
        EventKind::TenantCreated,
        req.tenant_id.clone(),
        format!("Tenant {:?} created via API", req.tenant_id),
    ));
    state
        .telemetry
        .emit(
            "TenantCreated",
            HashMap::from([
                ("tenantId".to_string(), req.tenant_id.clone()),
                ("callerSubject".to_string(), identity.subject.clone()),
            ]),
        )
        .await;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "tenantId": req.tenant_id,
            "message": "TenantPipeline created — operator is provisioning resources",
        })),
    )
        .into_response())
}

async fn list_tenants(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let resp: Vec<TenantResponse> = state
        .store
        .list_tenants()
        .into_iter()
        .map(Into::into)
        .collect();
    Json(resp)
}

async fn get_tenant(
    State(state): State<Arc<ApiState>>,
    Path(tenant_id): Path<String>,
) -> Result<Response, ApiError> {
    match state.store.get_tenant(&tenant_id) {
        Some(rec) => Ok(Json(TenantResponse::from(rec)).into_response()),
        None => Err(err(StatusCode::NOT_FOUND, "tenant not found")),
    }
}

async fn delete_tenant(
    State(state): State<Arc<ApiState>>,
    identity: axum::extract::Extension<CallerIdentity>,
    Path(tenant_id): Path<String>,
) -> Result<Response, ApiError> {
    require_role(&identity, "Tenant.Write")?;
    let existed = state.crd.delete(&tenant_id).await.map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("k8s delete: {e}"),
        )
    })?;
    if !existed {
        return Err(err(StatusCode::NOT_FOUND, "tenant not found"));
    }
    Ok(Json(serde_json::json!({
        "tenantId": tenant_id,
        "message": "TenantPipeline deletion initiated",
    }))
    .into_response())
}

async fn update_policy(
    State(state): State<Arc<ApiState>>,
    identity: axum::extract::Extension<CallerIdentity>,
    Path(tenant_id): Path<String>,
    Json(req): Json<UpdatePolicyRequest>,
) -> Result<Response, ApiError> {
    require_role(&identity, "Tenant.Write")?;
    let policy = RateLimitPolicy {
        tier: req.tier.clone(),
        requests_per_second: req.requests_per_second,
        burst_capacity: req.burst_capacity,
        max_concurrent: req.max_concurrent,
    };

    let existed = state
        .crd
        .patch_policy(&tenant_id, policy)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("k8s patch: {e}")))?;
    if !existed {
        return Err(err(StatusCode::NOT_FOUND, "tenant not found"));
    }

    state.store.publish(Event::new(
        EventKind::ConfigReloaded,
        tenant_id.clone(),
        format!(
            "Policy updated → tier={} rps={} burst={}",
            req.tier, req.requests_per_second, req.burst_capacity
        ),
    ));

    Ok(Json(serde_json::json!({
        "tenantId": tenant_id,
        "message": "Policy update sent — operator will hot-reload ConfigMap within one reconcile cycle",
    }))
    .into_response())
}

/// The chaos endpoint genuinely deletes the live proxy Deployment from
/// Kubernetes — it is not a narration. See `chaos.rs` for the narrated
/// self-heal sequence that follows.
async fn chaos_kill_proxy(
    State(state): State<Arc<ApiState>>,
    identity: axum::extract::Extension<CallerIdentity>,
    Path(tenant_id): Path<String>,
) -> Result<Response, ApiError> {
    require_role(&identity, "Tenant.Write")?;
    let rec = state
        .store
        .get_tenant(&tenant_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "tenant not found"))?;

    state
        .backend
        .delete_proxy_deployment(&tenant_id, &rec.namespace)
        .await
        .map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("chaos: delete proxy deployment: {e}"),
            )
        })?;

    state.store.publish(Event::new(
        EventKind::ChaosInjected,
        tenant_id.clone(),
        format!("💥 Chaos: killed proxy deployment {}", rec.proxy_pod_name),
    ));
    state.store.append_log(format!(
        "[{}] 💥 CHAOS → deleted proxy/{} in ns/{}",
        Utc::now().format("%H:%M:%S%.3f"),
        rec.proxy_pod_name,
        rec.namespace
    ));
    state
        .telemetry
        .emit(
            "ChaosInjected",
            HashMap::from([
                ("tenantId".to_string(), tenant_id.clone()),
                ("callerSubject".to_string(), identity.subject.clone()),
            ]),
        )
        .await;

    Ok(Json(serde_json::json!({
        "tenantId": tenant_id,
        "message": "Chaos injected — proxy deployment deleted from the cluster. Watch the TUI for auto-heal.",
        "proxy": rec.proxy_pod_name,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s_backend::mock::MockBackend;
    use crate::k8s_backend::mock_crd::MockCrdBackend;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> (
        Arc<ApiState>,
        Arc<MockCrdBackend>,
        Arc<MockBackend>,
        Arc<Store>,
    ) {
        let crd = Arc::new(MockCrdBackend::new());
        let backend = Arc::new(MockBackend::new());
        let store = Arc::new(Store::new());
        let state = Arc::new(ApiState::new(crd.clone(), backend.clone(), store.clone()));
        (state, crd, backend, store)
    }

    async fn send(app: Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = if bytes.is_empty() {
            serde_json::json!(null)
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(
                |_| serde_json::json!({ "raw": String::from_utf8_lossy(&bytes).to_string() }),
            )
        };
        (status, body)
    }

    #[tokio::test]
    async fn healthz_ok() {
        let (state, ..) = test_state();
        let app = router(state);
        let (status, _) = send(app, Request::get("/healthz").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn create_tenant_rejects_missing_tenant_id() {
        let (state, ..) = test_state();
        let app = router(state);
        let req = Request::post("/api/v1/tenants")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"displayName":"no id"}"#))
            .unwrap();
        let (status, _) = send(app, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_tenant_persists_crd() {
        let (state, crd, ..) = test_state();
        let app = router(state);
        let body = serde_json::json!({
            "tenantId": "acme",
            "displayName": "Acme Corp",
            "rateLimit": {"tier": "pro", "requestsPerSecond": 200, "burstCapacity": 400, "maxConcurrent": 50},
            "proxy": {"image": "envoyproxy/envoy:v1.28-latest", "port": 10000}
        });
        let req = Request::post("/api/v1/tenants")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, _) = send(app, req).await;
        assert_eq!(status, StatusCode::CREATED);

        let spec = crd.get("acme").unwrap();
        assert_eq!(spec.rate_limit.tier, "pro");
    }

    #[tokio::test]
    async fn get_and_list_tenants() {
        let (state, _crd, _backend, store) = test_state();
        store.upsert_tenant(TenantRecord {
            tenant_id: "acme".into(),
            phase: crate::store::TenantPhase::Ready,
            tier: "pro".into(),
            ..Default::default()
        });
        let app = router(state);

        let (status, body) = send(
            app.clone(),
            Request::get("/api/v1/tenants/acme")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tenantId"], "acme");
        assert_eq!(body["phase"], "Ready");

        let (status, _) = send(
            app.clone(),
            Request::get("/api/v1/tenants/ghost")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = send(
            app,
            Request::get("/api/v1/tenants").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_policy_patches_crd() {
        let (state, crd, ..) = test_state();
        crd.create(TenantPipelineSpec {
            tenant_id: "acme".into(),
            display_name: "Acme".into(),
            rate_limit: RateLimitPolicy {
                tier: "free".into(),
                requests_per_second: 10,
                burst_capacity: 20,
                max_concurrent: 5,
            },
            proxy: Default::default(),
            paused: false,
        })
        .await
        .unwrap();
        let app = router(state);

        let body = serde_json::json!({"tier": "enterprise", "requestsPerSecond": 5000, "burstCapacity": 10000, "maxConcurrent": 500});
        let req = Request::patch("/api/v1/tenants/acme/policy")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, _) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(crd.get("acme").unwrap().rate_limit.tier, "enterprise");
    }

    #[tokio::test]
    async fn update_policy_not_found() {
        let (state, ..) = test_state();
        let app = router(state);
        let body = serde_json::json!({"tier": "pro", "requestsPerSecond": 1, "burstCapacity": 1, "maxConcurrent": 1});
        let req = Request::patch("/api/v1/tenants/ghost/policy")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, _) = send(app, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_tenant_removes_crd() {
        let (state, crd, ..) = test_state();
        crd.create(TenantPipelineSpec {
            tenant_id: "acme".into(),
            display_name: "".into(),
            rate_limit: Default::default(),
            proxy: Default::default(),
            paused: false,
        })
        .await
        .unwrap();
        let app = router(state);
        let (status, _) = send(
            app,
            Request::delete("/api/v1/tenants/acme")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(crd.get("acme").is_none());
    }

    /// Regression test for the exact bug the Go version shipped: the chaos
    /// endpoint must actually delete the Deployment, not just log about it.
    #[tokio::test]
    async fn chaos_kill_proxy_actually_deletes_the_deployment() {
        let (state, _crd, backend, store) = test_state();
        store.upsert_tenant(TenantRecord {
            tenant_id: "acme".into(),
            namespace: "tenant-acme".into(),
            proxy_pod_name: "proxy-acme".into(),
            ..Default::default()
        });
        backend
            .ensure_proxy_deployment(crate::k8s_backend::ProxySpec {
                tenant_id: "acme".into(),
                namespace: "tenant-acme".into(),
                image: "envoy".into(),
                port: 10000,
                cpu_milli_limit: 100,
                memory_mib_limit: 64,
                config_map_name: "cm".into(),
            })
            .await
            .unwrap();
        assert!(backend.deployment("tenant-acme", "acme").is_some());

        let app = router(state);
        let (status, _) = send(
            app,
            Request::post("/api/v1/tenants/acme/chaos/kill-proxy")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            backend.deployment("tenant-acme", "acme").is_none(),
            "chaos endpoint must actually delete the deployment"
        );
    }

    #[tokio::test]
    async fn chaos_kill_proxy_unknown_tenant() {
        let (state, ..) = test_state();
        let app = router(state);
        let (status, _) = send(
            app,
            Request::post("/api/v1/tenants/ghost/chaos/kill-proxy")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cors_preflight() {
        let (state, ..) = test_state();
        let app = router(state);
        let req = Request::builder()
            .method(Method::OPTIONS)
            .uri("/api/v1/tenants")
            .header("Origin", "http://example.com")
            .header("Access-Control-Request-Method", "POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_success());
        assert!(resp.headers().get("access-control-allow-origin").is_some());
    }
}
