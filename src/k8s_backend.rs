//! Namespace / ConfigMap / Deployment lifecycle, behind a `K8sBackend` trait.
//!
//! Ported from `pkg/k8s/resources.go`. One deliberate design change from the
//! Go version: instead of a read-modify-write `client.MergeFrom(base)` diff
//! (the exact pattern that produced three silent no-op-patch bugs in the Go
//! codebase — see that project's POSTMORTEM.md), `ensure_configmap` and
//! `ensure_proxy_deployment` use Kubernetes Server-Side Apply
//! (`Patch::Apply`), which is idempotent by construction: you always send
//! the *complete* desired object and the API server computes the diff
//! itself. There is no "snapshot captured before mutation" step to get
//! wrong, because there's no snapshot at all.
//!
//! The `K8sBackend` trait exists so the reconcile logic in `reconcile.rs` can
//! be unit-tested against an in-memory `MockBackend` without a live cluster
//! or the heavier tower-service mocking `kube::Client` would otherwise
//! require — the same practical trade-off the Go version made by testing
//! against controller-runtime's fake `client.Client`.

use async_trait::async_trait;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace};
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use kube::{Client, Resource};
use serde_json::json;
use thiserror::Error;

pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY_VALUE: &str = "vanguard-operator";
pub const TENANT_LABEL: &str = "vanguard.io/tenant-id";
const FIELD_MANAGER: &str = "vanguard-operator";

#[derive(Debug, Error)]
pub enum K8sError {
    #[error("kube api error: {0}")]
    Kube(#[from] kube::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, K8sError>;

#[derive(Clone, Debug, Default)]
pub struct RateLimitData {
    pub tier: String,
    pub rps: i32,
    pub burst: i32,
    pub max_concurrent: i32,
}

#[derive(Clone, Debug, Default)]
pub struct ProxySpec {
    pub tenant_id: String,
    pub namespace: String,
    pub image: String,
    pub port: i32,
    pub cpu_milli_limit: i64,
    pub memory_mib_limit: i64,
    pub config_map_name: String,
}

pub fn namespace_name(tenant_id: &str) -> String {
    format!("tenant-{tenant_id}")
}
pub fn configmap_name(tenant_id: &str) -> String {
    format!("vanguard-rl-{tenant_id}")
}
pub fn proxy_name(tenant_id: &str) -> String {
    format!("proxy-{tenant_id}")
}

#[async_trait]
pub trait K8sBackend: Send + Sync {
    async fn ensure_namespace(&self, tenant_id: &str) -> Result<String>;
    async fn delete_namespace(&self, tenant_id: &str) -> Result<()>;
    async fn ensure_configmap(
        &self,
        tenant_id: &str,
        namespace: &str,
        data: RateLimitData,
    ) -> Result<String>;
    async fn ensure_proxy_deployment(&self, spec: ProxySpec) -> Result<String>;
    async fn delete_proxy_deployment(&self, tenant_id: &str, namespace: &str) -> Result<()>;
}

// ─── Real backend (talks to a live Kubernetes API server) ─────────────────────

pub struct KubeBackend {
    client: Client,
}

impl KubeBackend {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl K8sBackend for KubeBackend {
    async fn ensure_namespace(&self, tenant_id: &str) -> Result<String> {
        let name = namespace_name(tenant_id);
        let api: Api<Namespace> = Api::all(self.client.clone());

        if api.get_opt(&name).await?.is_some() {
            return Ok(name); // already exists — idempotent no-op
        }

        let desired: Namespace = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {
                "name": name,
                "labels": {
                    MANAGED_BY_LABEL: MANAGED_BY_VALUE,
                    TENANT_LABEL: tenant_id,
                }
            }
        }))
        .map_err(|e| K8sError::Other(e.to_string()))?;

        api.create(&PostParams::default(), &desired).await?;
        Ok(name)
    }

    async fn delete_namespace(&self, tenant_id: &str) -> Result<()> {
        let name = namespace_name(tenant_id);
        let api: Api<Namespace> = Api::all(self.client.clone());
        if api.get_opt(&name).await?.is_none() {
            return Ok(());
        }
        api.delete(&name, &DeleteParams::default()).await?;
        Ok(())
    }

    async fn ensure_configmap(
        &self,
        tenant_id: &str,
        namespace: &str,
        data: RateLimitData,
    ) -> Result<String> {
        let name = configmap_name(tenant_id);
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);

        let desired = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": name,
                "namespace": namespace,
                "labels": { MANAGED_BY_LABEL: MANAGED_BY_VALUE, TENANT_LABEL: tenant_id }
            },
            "data": {
                "tier": data.tier,
                "requests_per_s": data.rps.to_string(),
                "burst_capacity": data.burst.to_string(),
                "max_concurrent": data.max_concurrent.to_string(),
                "tenant_id": tenant_id,
            }
        });

        // Server-side apply: idempotent create-or-update, no read-modify-write
        // race and no "diff against a stale snapshot" footgun.
        api.patch(
            &name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&desired),
        )
        .await?;
        Ok(name)
    }

    async fn ensure_proxy_deployment(&self, spec: ProxySpec) -> Result<String> {
        let name = proxy_name(&spec.tenant_id);
        let api: Api<Deployment> = Api::namespaced(self.client.clone(), &spec.namespace);

        let desired = build_deployment_manifest(&name, &spec);
        api.patch(
            &name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&desired),
        )
        .await?;
        Ok(name)
    }

    async fn delete_proxy_deployment(&self, tenant_id: &str, namespace: &str) -> Result<()> {
        let name = proxy_name(tenant_id);
        let api: Api<Deployment> = Api::namespaced(self.client.clone(), namespace);
        if api.get_opt(&name).await?.is_none() {
            return Ok(());
        }
        api.delete(&name, &DeleteParams::default()).await?;
        Ok(())
    }
}

fn build_deployment_manifest(name: &str, spec: &ProxySpec) -> serde_json::Value {
    let cpu_limit_m = spec.cpu_milli_limit;
    let cpu_request_m = spec.cpu_milli_limit / 4;
    let mem_limit_bytes = spec.memory_mib_limit * 1024 * 1024;
    let mem_request_bytes = mem_limit_bytes / 4;

    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": name,
            "namespace": spec.namespace,
            "labels": {
                MANAGED_BY_LABEL: MANAGED_BY_VALUE,
                TENANT_LABEL: spec.tenant_id,
                "app": "vanguard-proxy",
            }
        },
        "spec": {
            "replicas": 1,
            "selector": {
                "matchLabels": { TENANT_LABEL: spec.tenant_id, "app": "vanguard-proxy" }
            },
            "template": {
                "metadata": {
                    "labels": { TENANT_LABEL: spec.tenant_id, "app": "vanguard-proxy" }
                },
                "spec": {
                    "containers": [{
                        "name": "proxy",
                        "image": spec.image,
                        "ports": [{ "containerPort": spec.port, "protocol": "TCP" }],
                        "resources": {
                            "limits": {
                                "cpu": format!("{cpu_limit_m}m"),
                                "memory": format!("{mem_limit_bytes}"),
                            },
                            "requests": {
                                "cpu": format!("{cpu_request_m}m"),
                                "memory": format!("{mem_request_bytes}"),
                            }
                        },
                        "volumeMounts": [{
                            "name": "rl-config",
                            "mountPath": "/etc/vanguard/rl",
                            "readOnly": true,
                        }],
                        "readinessProbe": {
                            "tcpSocket": { "port": spec.port },
                            "initialDelaySeconds": 3,
                            "periodSeconds": 5,
                        }
                    }],
                    "volumes": [{
                        "name": "rl-config",
                        "configMap": { "name": spec.config_map_name }
                    }]
                }
            }
        }
    })
}

// ─── Mock backend (in-memory, used by reconcile.rs unit tests) ────────────────

pub mod mock {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    #[derive(Default)]
    struct State {
        namespaces: HashSet<String>,
        configmaps: HashSet<(String, String)>, // (namespace, name)
        deployments: std::collections::HashMap<(String, String), ProxySpec>,
    }

    /// In-memory stand-in for a Kubernetes API server. Lets `reconcile.rs`
    /// be tested for its actual business logic (what gets created, in what
    /// order, with what fallback defaults) without needing a live cluster.
    /// Its inspection methods (`has_namespace` etc.) are used from other
    /// modules' test code (`api.rs`, `reconcile.rs`), which is why they're
    /// `pub` rather than `#[cfg(test)]`-only, and why a plain `cargo build`
    /// sees them as unused — they only light up under `cargo test`.
    #[derive(Default)]
    pub struct MockBackend {
        state: Mutex<State>,
    }

    #[allow(dead_code)]
    impl MockBackend {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn has_namespace(&self, tenant_id: &str) -> bool {
            self.state
                .lock()
                .unwrap()
                .namespaces
                .contains(&namespace_name(tenant_id))
        }

        pub fn has_configmap(&self, namespace: &str, tenant_id: &str) -> bool {
            self.state
                .lock()
                .unwrap()
                .configmaps
                .contains(&(namespace.to_string(), configmap_name(tenant_id)))
        }

        pub fn deployment(&self, namespace: &str, tenant_id: &str) -> Option<ProxySpec> {
            self.state
                .lock()
                .unwrap()
                .deployments
                .get(&(namespace.to_string(), proxy_name(tenant_id)))
                .cloned()
        }
    }

    #[async_trait]
    impl K8sBackend for MockBackend {
        async fn ensure_namespace(&self, tenant_id: &str) -> Result<String> {
            let name = namespace_name(tenant_id);
            self.state.lock().unwrap().namespaces.insert(name.clone());
            Ok(name)
        }

        async fn delete_namespace(&self, tenant_id: &str) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .namespaces
                .remove(&namespace_name(tenant_id));
            Ok(())
        }

        async fn ensure_configmap(
            &self,
            tenant_id: &str,
            namespace: &str,
            _data: RateLimitData,
        ) -> Result<String> {
            let name = configmap_name(tenant_id);
            self.state
                .lock()
                .unwrap()
                .configmaps
                .insert((namespace.to_string(), name.clone()));
            Ok(name)
        }

        async fn ensure_proxy_deployment(&self, spec: ProxySpec) -> Result<String> {
            let name = proxy_name(&spec.tenant_id);
            self.state
                .lock()
                .unwrap()
                .deployments
                .insert((spec.namespace.clone(), name.clone()), spec);
            Ok(name)
        }

        async fn delete_proxy_deployment(&self, tenant_id: &str, namespace: &str) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .deployments
                .remove(&(namespace.to_string(), proxy_name(tenant_id)));
            Ok(())
        }
    }
}

// ─── CRD object backend (used by the REST API layer) ──────────────────────────
//
// Separate concern from `K8sBackend` above: this manages the TenantPipeline
// custom resource *object itself* (create / read / patch-spec / delete),
// the way `internal/api/server.go` used the same `client.Client` for both
// CRDs and built-ins in Go. kube-rs doesn't have as mature an in-memory fake
// for `Api<T>` as controller-runtime's fake client, so this trait exists to
// keep the REST API handlers unit-testable the same way `K8sBackend` does
// for the reconciler.

use crate::crd::{RateLimitPolicy, TenantPipelineSpec};

#[async_trait]
pub trait CrdBackend: Send + Sync {
    async fn create(&self, spec: TenantPipelineSpec) -> Result<()>;
    /// Returns `false` if no TenantPipeline with that name exists.
    async fn patch_policy(&self, tenant_id: &str, policy: RateLimitPolicy) -> Result<bool>;
    /// Returns `false` if no TenantPipeline with that name exists.
    async fn delete(&self, tenant_id: &str) -> Result<bool>;
}

pub struct KubeCrdBackend {
    client: Client,
}

impl KubeCrdBackend {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl CrdBackend for KubeCrdBackend {
    async fn create(&self, spec: TenantPipelineSpec) -> Result<()> {
        use crate::crd::TenantPipeline;
        let api: Api<TenantPipeline> = Api::all(self.client.clone());
        let name = spec.tenant_id.clone();
        let mut tp = TenantPipeline::new(&name, spec);
        tp.meta_mut().labels =
            Some([("vanguard.io/created-by".to_string(), "api".to_string())].into());
        api.create(&PostParams::default(), &tp).await?;
        Ok(())
    }

    async fn patch_policy(&self, tenant_id: &str, policy: RateLimitPolicy) -> Result<bool> {
        use crate::crd::TenantPipeline;
        let api: Api<TenantPipeline> = Api::all(self.client.clone());
        if api.get_opt(tenant_id).await?.is_none() {
            return Ok(false);
        }
        // Server-side apply on just the spec.rateLimit field — no
        // read-modify-write, so there's no stale-snapshot patch bug to have.
        let patch = json!({
            "apiVersion": format!("{}/{}", crate::crd::GROUP, crate::crd::VERSION),
            "kind": crate::crd::KIND,
            "spec": { "rateLimit": policy }
        });
        api.patch(
            tenant_id,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&patch),
        )
        .await?;
        Ok(true)
    }

    async fn delete(&self, tenant_id: &str) -> Result<bool> {
        use crate::crd::TenantPipeline;
        let api: Api<TenantPipeline> = Api::all(self.client.clone());
        if api.get_opt(tenant_id).await?.is_none() {
            return Ok(false);
        }
        api.delete(tenant_id, &DeleteParams::default()).await?;
        Ok(true)
    }
}

pub mod mock_crd {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MockCrdBackend {
        objects: Mutex<HashMap<String, TenantPipelineSpec>>,
    }

    #[allow(dead_code)]
    impl MockCrdBackend {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn get(&self, tenant_id: &str) -> Option<TenantPipelineSpec> {
            self.objects.lock().unwrap().get(tenant_id).cloned()
        }

        /// Snapshot of every live TenantPipeline spec. Used by the demo-mode
        /// poller (`demo.rs`) as a stand-in for the watch stream a real
        /// `kube::runtime::Controller` gets from the API server — there's no
        /// live cluster to watch in demo mode, so this is polled instead.
        pub fn list(&self) -> Vec<TenantPipelineSpec> {
            self.objects.lock().unwrap().values().cloned().collect()
        }
    }

    #[async_trait]
    impl CrdBackend for MockCrdBackend {
        async fn create(&self, spec: TenantPipelineSpec) -> Result<()> {
            self.objects
                .lock()
                .unwrap()
                .insert(spec.tenant_id.clone(), spec);
            Ok(())
        }

        async fn patch_policy(&self, tenant_id: &str, policy: RateLimitPolicy) -> Result<bool> {
            let mut objects = self.objects.lock().unwrap();
            match objects.get_mut(tenant_id) {
                Some(spec) => {
                    spec.rate_limit = policy;
                    Ok(true)
                }
                None => Ok(false),
            }
        }

        async fn delete(&self, tenant_id: &str) -> Result<bool> {
            Ok(self.objects.lock().unwrap().remove(tenant_id).is_some())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockBackend;
    use super::*;

    #[tokio::test]
    async fn mock_ensure_namespace_is_idempotent() {
        let backend = MockBackend::new();
        let name1 = backend.ensure_namespace("acme").await.unwrap();
        let name2 = backend.ensure_namespace("acme").await.unwrap();
        assert_eq!(name1, "tenant-acme");
        assert_eq!(name1, name2);
        assert!(backend.has_namespace("acme"));
    }

    #[tokio::test]
    async fn mock_delete_namespace_is_idempotent() {
        let backend = MockBackend::new();
        backend.delete_namespace("ghost").await.unwrap(); // never existed
        backend.ensure_namespace("acme").await.unwrap();
        backend.delete_namespace("acme").await.unwrap();
        assert!(!backend.has_namespace("acme"));
    }

    #[tokio::test]
    async fn mock_configmap_and_deployment_roundtrip() {
        let backend = MockBackend::new();
        let ns = backend.ensure_namespace("acme").await.unwrap();
        backend
            .ensure_configmap(
                "acme",
                &ns,
                RateLimitData {
                    tier: "pro".into(),
                    rps: 200,
                    burst: 400,
                    max_concurrent: 50,
                },
            )
            .await
            .unwrap();
        assert!(backend.has_configmap(&ns, "acme"));

        backend
            .ensure_proxy_deployment(ProxySpec {
                tenant_id: "acme".into(),
                namespace: ns.clone(),
                image: "envoy".into(),
                port: 10000,
                cpu_milli_limit: 100,
                memory_mib_limit: 64,
                config_map_name: "vanguard-rl-acme".into(),
            })
            .await
            .unwrap();
        let dep = backend.deployment(&ns, "acme").unwrap();
        assert_eq!(dep.image, "envoy");

        backend.delete_proxy_deployment("acme", &ns).await.unwrap();
        assert!(backend.deployment(&ns, "acme").is_none());
    }

    #[test]
    fn deployment_manifest_resource_requests_are_quarter_of_limits() {
        let spec = ProxySpec {
            tenant_id: "acme".into(),
            namespace: "tenant-acme".into(),
            image: "envoy".into(),
            port: 10000,
            cpu_milli_limit: 400,
            memory_mib_limit: 128,
            config_map_name: "cm".into(),
        };
        let manifest = build_deployment_manifest("proxy-acme", &spec);
        let resources = &manifest["spec"]["template"]["spec"]["containers"][0]["resources"];
        assert_eq!(resources["limits"]["cpu"], "400m");
        assert_eq!(resources["requests"]["cpu"], "100m");
        assert_eq!(
            resources["limits"]["memory"],
            (128 * 1024 * 1024).to_string()
        );
        assert_eq!(
            resources["requests"]["memory"],
            (128 * 1024 * 1024 / 4).to_string()
        );
    }

    #[tokio::test]
    async fn mock_crd_backend_create_patch_delete() {
        use super::mock_crd::MockCrdBackend;
        let crd_backend = MockCrdBackend::new();

        let spec = TenantPipelineSpec {
            tenant_id: "acme".into(),
            display_name: "Acme Corp".into(),
            rate_limit: crate::crd::RateLimitPolicy {
                tier: "free".into(),
                requests_per_second: 10,
                burst_capacity: 20,
                max_concurrent: 5,
            },
            proxy: Default::default(),
            paused: false,
        };
        crd_backend.create(spec.clone()).await.unwrap();
        assert_eq!(crd_backend.get("acme").unwrap().rate_limit.tier, "free");

        let patched = crd_backend
            .patch_policy(
                "acme",
                crate::crd::RateLimitPolicy {
                    tier: "enterprise".into(),
                    requests_per_second: 5000,
                    burst_capacity: 10000,
                    max_concurrent: 500,
                },
            )
            .await
            .unwrap();
        assert!(patched);
        assert_eq!(
            crd_backend.get("acme").unwrap().rate_limit.tier,
            "enterprise"
        );

        // Patching a tenant that doesn't exist must report "not found", not silently succeed.
        let patched_missing = crd_backend
            .patch_policy("ghost", crate::crd::RateLimitPolicy::default())
            .await
            .unwrap();
        assert!(!patched_missing);

        let deleted = crd_backend.delete("acme").await.unwrap();
        assert!(deleted);
        assert!(crd_backend.get("acme").is_none());
        assert!(!crd_backend.delete("acme").await.unwrap()); // already gone
    }
}
