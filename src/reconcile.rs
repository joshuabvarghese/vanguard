//! Reconcile business logic, ported from `internal/operator/reconciler.go`.
//!
//! Split from the actual `kube::runtime::Controller` wiring (in
//! `operator.rs`) so it can be unit-tested against `k8s_backend::mock` and
//! an in-memory `Store` without a live Kubernetes API server or a
//! `kube::Client` mocked at the HTTP layer.

use crate::crd::{set_condition, Phase, TenantPipelineSpec, TenantPipelineStatus, CONDITION_READY};
use crate::k8s_backend::{K8sBackend, ProxySpec, RateLimitData};
use crate::store::{Event, EventKind, Store, TenantPhase, TenantRecord};
use std::sync::Arc;

const DEFAULT_PROXY_IMAGE: &str = "envoyproxy/envoy:v1.28-latest";
const DEFAULT_PROXY_PORT: i32 = 10000;
const DEFAULT_CPU_MILLI: i64 = 100;
const DEFAULT_MEM_MIB: i64 = 64;

/// What the caller (the real Controller, or a test) needs in order to patch
/// the CRD's status subresource back onto the API server. Kept separate from
/// `TenantPipelineStatus` so this module has zero dependency on `kube::Api`.
#[derive(Debug, Clone)]
pub struct ReconcileOutcome {
    pub namespace_name: String,
    pub proxy_pod_name: String,
    pub config_map_name: String,
    pub reconcile_count: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("namespace provisioning failed: {0}")]
    Namespace(String),
    #[error("configmap provisioning failed: {0}")]
    ConfigMap(String),
    #[error("proxy deployment provisioning failed: {0}")]
    Proxy(String),
}

impl ReconcileError {
    pub fn reason(&self) -> &'static str {
        match self {
            ReconcileError::Namespace(_) => "NamespaceFailed",
            ReconcileError::ConfigMap(_) => "ConfigMapFailed",
            ReconcileError::Proxy(_) => "ProxyFailed",
        }
    }
}

fn log_line(store: &Store, msg: impl Into<String>) {
    let ts = chrono::Utc::now().format("%H:%M:%S%.3f");
    store.append_log(format!("[{ts}] {}", msg.into()));
}

/// Drives one tenant's actual cluster resources to match `spec`. Idempotent:
/// safe to call repeatedly with the same input, exactly like the Go
/// version's `Reconcile()` contract.
///
/// On success, also upserts the `Store` (drives the TUI/REST API) and
/// returns the fields the caller should patch onto `status`.
pub async fn reconcile_tenant(
    backend: &dyn K8sBackend,
    store: &Arc<Store>,
    spec: &TenantPipelineSpec,
    prior_reconcile_count: i64,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<ReconcileOutcome, ReconcileError> {
    let tenant_id = &spec.tenant_id;

    log_line(
        store,
        format!(
            "⟳  [{tenant_id}] reconcile #{} start",
            prior_reconcile_count + 1
        ),
    );
    store.publish(Event::new(
        EventKind::ReconcileStart,
        tenant_id.clone(),
        format!("Reconcile #{}", prior_reconcile_count + 1),
    ));

    // Step A: Namespace
    let ns_name = backend
        .ensure_namespace(tenant_id)
        .await
        .map_err(|e| ReconcileError::Namespace(e.to_string()))?;
    log_line(store, format!("  ✓ namespace/{ns_name} ready"));
    store.publish(Event::new(
        EventKind::NamespaceReady,
        tenant_id.clone(),
        format!("Namespace {ns_name} ready"),
    ));

    // Step B: ConfigMap (create or hot-reload)
    let cm_name = backend
        .ensure_configmap(
            tenant_id,
            &ns_name,
            RateLimitData {
                tier: spec.rate_limit.tier.clone(),
                rps: spec.rate_limit.requests_per_second,
                burst: spec.rate_limit.burst_capacity,
                max_concurrent: spec.rate_limit.max_concurrent,
            },
        )
        .await
        .map_err(|e| ReconcileError::ConfigMap(e.to_string()))?;
    log_line(
        store,
        format!(
            "  ✓ configmap/{cm_name} synced (tier={} rps={} burst={})",
            spec.rate_limit.tier,
            spec.rate_limit.requests_per_second,
            spec.rate_limit.burst_capacity
        ),
    );
    store.publish(Event::new(
        EventKind::ConfigReloaded,
        tenant_id.clone(),
        format!("ConfigMap {cm_name} synced"),
    ));

    // Step C: Proxy Deployment, applying the same fallback defaults as the Go version.
    let image = if spec.proxy.image.is_empty() {
        DEFAULT_PROXY_IMAGE.to_string()
    } else {
        spec.proxy.image.clone()
    };
    let port = if spec.proxy.port == 0 {
        DEFAULT_PROXY_PORT
    } else {
        spec.proxy.port
    };
    let cpu_limit = spec
        .proxy
        .resource_limit_milli_cpu
        .filter(|v| *v != 0)
        .unwrap_or(DEFAULT_CPU_MILLI);
    let mem_limit = spec
        .proxy
        .resource_limit_memory_mib
        .filter(|v| *v != 0)
        .unwrap_or(DEFAULT_MEM_MIB);

    let proxy_name = backend
        .ensure_proxy_deployment(ProxySpec {
            tenant_id: tenant_id.clone(),
            namespace: ns_name.clone(),
            image: image.clone(),
            port,
            cpu_milli_limit: cpu_limit,
            memory_mib_limit: mem_limit,
            config_map_name: cm_name.clone(),
        })
        .await
        .map_err(|e| ReconcileError::Proxy(e.to_string()))?;
    log_line(
        store,
        format!("  ✓ deployment/{proxy_name} up (image={image} port={port})"),
    );
    store.publish(Event::new(
        EventKind::ProxyUp,
        tenant_id.clone(),
        format!("Proxy deployment {proxy_name} running"),
    ));

    // Update the Store (drives the TUI/REST API).
    let now = chrono::Utc::now();
    let reconcile_count = prior_reconcile_count + 1;
    store.upsert_tenant(TenantRecord {
        tenant_id: tenant_id.clone(),
        display_name: spec.display_name.clone(),
        namespace: ns_name.clone(),
        phase: TenantPhase::Ready,
        tier: spec.rate_limit.tier.clone(),
        rps: spec.rate_limit.requests_per_second,
        burst: spec.rate_limit.burst_capacity,
        max_concurrent: spec.rate_limit.max_concurrent,
        proxy_pod_name: proxy_name.clone(),
        proxy_image: image,
        config_map_name: cm_name.clone(),
        reconcile_count,
        last_reconcile_at: Some(now),
        created_at,
    });

    log_line(store, format!("  ✓ [{tenant_id}] reconcile complete"));
    store.publish(Event::new(
        EventKind::ReconcileOk,
        tenant_id.clone(),
        "Converged".to_string(),
    ));

    Ok(ReconcileOutcome {
        namespace_name: ns_name,
        proxy_pod_name: proxy_name,
        config_map_name: cm_name,
        reconcile_count,
    })
}

/// Applies a successful reconcile outcome onto a status object in place,
/// including the Ready condition. Separated out so both the real Controller
/// (which then patches this onto the API server) and tests (which just
/// assert on the struct) share one code path.
pub fn apply_ready_status(
    status: &mut TenantPipelineStatus,
    outcome: &ReconcileOutcome,
    observed_generation: i64,
) {
    status.phase = Phase::Ready;
    status.namespace_name = outcome.namespace_name.clone();
    status.proxy_pod_name = outcome.proxy_pod_name.clone();
    status.config_map_name = outcome.config_map_name.clone();
    status.observed_generation = observed_generation;
    status.reconcile_count = outcome.reconcile_count;
    status.last_reconcile_time = Some(chrono::Utc::now().to_rfc3339());
    set_condition(
        status,
        CONDITION_READY,
        true,
        "Reconciled",
        "All resources healthy",
    );
}

/// Marks a tenant Degraded — both on the Store (TUI/API view) and on the
/// status object the caller will patch back. Mirrors the Go version's
/// `setDegraded`.
pub fn apply_degraded_status(
    store: &Store,
    status: &mut TenantPipelineStatus,
    tenant_id: &str,
    display_name: &str,
    err: &ReconcileError,
) {
    log_line(
        store,
        format!("  ✗ [{tenant_id}] degraded: {}: {err}", err.reason()),
    );
    store.publish(Event::new(
        EventKind::ReconcileError,
        tenant_id.to_string(),
        format!("{}: {err}", err.reason()),
    ));

    status.phase = Phase::Degraded;
    set_condition(
        status,
        CONDITION_READY,
        false,
        err.reason(),
        &err.to_string(),
    );

    store.upsert_tenant(TenantRecord {
        tenant_id: tenant_id.to_string(),
        display_name: display_name.to_string(),
        phase: TenantPhase::Degraded,
        last_reconcile_at: Some(chrono::Utc::now()),
        ..Default::default()
    });
}

/// Deletes all resources owned by a tenant. Mirrors `handleDeletion`.
pub async fn handle_deletion(
    backend: &dyn K8sBackend,
    store: &Store,
    tenant_id: &str,
) -> Result<(), ReconcileError> {
    log_line(
        store,
        format!("  🗑  deleting resources for tenant {tenant_id}"),
    );

    if let Some(rec) = store.get_tenant(tenant_id) {
        backend
            .delete_proxy_deployment(tenant_id, &rec.namespace)
            .await
            .map_err(|e| ReconcileError::Proxy(e.to_string()))?;
        backend
            .delete_namespace(tenant_id)
            .await
            .map_err(|e| ReconcileError::Namespace(e.to_string()))?;
    }

    store.delete_tenant(tenant_id);
    store.publish(Event::new(
        EventKind::TenantDeleted,
        tenant_id.to_string(),
        "Resources removed".to_string(),
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{ProxyConfig, RateLimitPolicy};
    use crate::k8s_backend::mock::MockBackend;

    fn sample_spec(tenant_id: &str) -> TenantPipelineSpec {
        TenantPipelineSpec {
            tenant_id: tenant_id.to_string(),
            display_name: "Acme Corp".to_string(),
            rate_limit: RateLimitPolicy {
                tier: "pro".into(),
                requests_per_second: 200,
                burst_capacity: 400,
                max_concurrent: 50,
            },
            proxy: ProxyConfig {
                image: "envoyproxy/envoy:v1.28-latest".into(),
                port: 10000,
                resource_limit_milli_cpu: Some(100),
                resource_limit_memory_mib: Some(64),
            },
            paused: false,
        }
    }

    #[tokio::test]
    async fn reconcile_provisions_full_stack() {
        let backend = MockBackend::new();
        let store = Arc::new(Store::new());
        let spec = sample_spec("acme");

        let outcome = reconcile_tenant(&backend, &store, &spec, 0, None)
            .await
            .unwrap();

        assert_eq!(outcome.namespace_name, "tenant-acme");
        assert_eq!(outcome.proxy_pod_name, "proxy-acme");
        assert_eq!(outcome.config_map_name, "vanguard-rl-acme");
        assert_eq!(outcome.reconcile_count, 1);

        assert!(backend.has_namespace("acme"));
        assert!(backend.has_configmap("tenant-acme", "acme"));
        assert!(backend.deployment("tenant-acme", "acme").is_some());

        let rec = store.get_tenant("acme").unwrap();
        assert_eq!(rec.phase, TenantPhase::Ready);
        assert_eq!(rec.reconcile_count, 1);
    }

    #[tokio::test]
    async fn reconcile_is_idempotent_across_repeated_calls() {
        let backend = MockBackend::new();
        let store = Arc::new(Store::new());
        let spec = sample_spec("acme");

        let mut count = 0;
        for _ in 0..3 {
            let outcome = reconcile_tenant(&backend, &store, &spec, count, None)
                .await
                .unwrap();
            count = outcome.reconcile_count;
        }
        assert_eq!(count, 3);
        assert_eq!(store.get_tenant("acme").unwrap().reconcile_count, 3);
    }

    #[tokio::test]
    async fn reconcile_defaults_proxy_image_and_port_when_unset() {
        let backend = MockBackend::new();
        let store = Arc::new(Store::new());
        let mut spec = sample_spec("acme");
        spec.proxy = ProxyConfig::default(); // no image/port/resources specified

        reconcile_tenant(&backend, &store, &spec, 0, None)
            .await
            .unwrap();

        let dep = backend.deployment("tenant-acme", "acme").unwrap();
        assert_eq!(dep.image, DEFAULT_PROXY_IMAGE);
        assert_eq!(dep.port, DEFAULT_PROXY_PORT);
        assert_eq!(dep.cpu_milli_limit, DEFAULT_CPU_MILLI);
        assert_eq!(dep.memory_mib_limit, DEFAULT_MEM_MIB);
    }

    #[tokio::test]
    async fn apply_ready_status_sets_single_condition_not_appended_per_call() {
        let backend = MockBackend::new();
        let store = Arc::new(Store::new());
        let spec = sample_spec("acme");
        let mut status = TenantPipelineStatus::default();

        for i in 0..3 {
            let outcome = reconcile_tenant(&backend, &store, &spec, i, None)
                .await
                .unwrap();
            apply_ready_status(&mut status, &outcome, 1);
        }

        assert_eq!(
            status.conditions.len(),
            1,
            "Ready condition must be updated in place, not appended per reconcile"
        );
        assert_eq!(status.phase, Phase::Ready);
        assert_eq!(status.reconcile_count, 3);
    }

    #[tokio::test]
    async fn apply_degraded_status_marks_store_and_status() {
        let store = Store::new();
        let mut status = TenantPipelineStatus::default();
        let err = ReconcileError::Proxy("connection refused".to_string());

        apply_degraded_status(&store, &mut status, "acme", "Acme Corp", &err);

        assert_eq!(status.phase, Phase::Degraded);
        let cond = status
            .conditions
            .iter()
            .find(|c| c.type_ == CONDITION_READY)
            .unwrap();
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, "ProxyFailed");

        let rec = store.get_tenant("acme").unwrap();
        assert_eq!(rec.phase, TenantPhase::Degraded);
    }

    #[tokio::test]
    async fn handle_deletion_removes_owned_resources_and_store_entry() {
        let backend = MockBackend::new();
        let store = Store::new();
        let spec = sample_spec("acme");
        let store_arc = Arc::new(Store::new());
        reconcile_tenant(&backend, &store_arc, &spec, 0, None)
            .await
            .unwrap();
        // Mirror the reconciled record into the plain `store` used for deletion,
        // since Store isn't Clone (by design — it's meant to live in one Arc).
        let rec = store_arc.get_tenant("acme").unwrap();
        store.upsert_tenant(rec);

        handle_deletion(&backend, &store, "acme").await.unwrap();

        assert!(backend.deployment("tenant-acme", "acme").is_none());
        assert!(!backend.has_namespace("acme"));
        assert!(store.get_tenant("acme").is_none());
    }

    #[tokio::test]
    async fn handle_deletion_on_unknown_tenant_is_a_noop_not_an_error() {
        let backend = MockBackend::new();
        let store = Store::new();
        handle_deletion(&backend, &store, "ghost").await.unwrap();
    }
}
