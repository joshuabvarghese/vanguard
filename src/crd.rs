//! TenantPipeline Custom Resource Definition types.
//!
//! `#[derive(CustomResource)]` generates the `TenantPipeline` struct (with
//! standard `ObjectMeta`, `spec`, and `status` fields), its `Api`-friendly
//! group/version/kind constants, and a JSON schema for the CRD manifest —
//! the equivalent of what `controller-gen` produces for the Go version, but
//! at compile time via a proc-macro instead of a separate codegen step.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const GROUP: &str = "infrastructure.vanguard.io";
pub const VERSION: &str = "v1alpha1";
pub const KIND: &str = "TenantPipeline";
pub const FINALIZER: &str = "vanguard.io/tenant-cleanup";

/// Token-bucket rate-limit parameters for a tenant tier. Hot-reloaded into
/// the tenant's ConfigMap without restarting proxy pods.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
pub struct RateLimitPolicy {
    /// "free" | "pro" | "enterprise"
    pub tier: String,
    #[serde(rename = "requestsPerSecond")]
    pub requests_per_second: i32,
    #[serde(rename = "burstCapacity")]
    pub burst_capacity: i32,
    #[serde(rename = "maxConcurrent")]
    pub max_concurrent: i32,
}

/// Controls the sidecar Envoy / edge-router deployment.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
pub struct ProxyConfig {
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub port: i32,
    #[serde(
        rename = "resourceLimitMilliCPU",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resource_limit_milli_cpu: Option<i64>,
    #[serde(
        rename = "resourceLimitMemoryMiB",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resource_limit_memory_mib: Option<i64>,
}

/// Desired state declared by the operator user / REST API.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "infrastructure.vanguard.io",
    version = "v1alpha1",
    kind = "TenantPipeline",
    plural = "tenantpipelines",
    shortname = "tp",
    status = "TenantPipelineStatus",
    printcolumn = r#"{"name":"Tenant", "type":"string", "jsonPath":".spec.tenantId"}"#,
    printcolumn = r#"{"name":"Tier", "type":"string", "jsonPath":".spec.rateLimit.tier"}"#,
    printcolumn = r#"{"name":"Phase", "type":"string", "jsonPath":".status.phase"}"#
)]
pub struct TenantPipelineSpec {
    #[serde(rename = "tenantId")]
    pub tenant_id: String,
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    #[serde(rename = "rateLimit")]
    pub rate_limit: RateLimitPolicy,
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub paused: bool,
}

/// High-level lifecycle stage of a TenantPipeline.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
pub enum Phase {
    #[default]
    Provisioning,
    Ready,
    Degraded,
    Terminating,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Phase::Provisioning => "Provisioning",
            Phase::Ready => "Ready",
            Phase::Degraded => "Degraded",
            Phase::Terminating => "Terminating",
        };
        write!(f, "{s}")
    }
}

/// Written back by the operator to reflect actual cluster state.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
pub struct TenantPipelineStatus {
    #[serde(default)]
    pub phase: Phase,
    /// `k8s_openapi::Condition` doesn't implement `JsonSchema` (no `schemars`
    /// feature on that crate), so this field gets a permissive
    /// array-of-objects schema instead of a fully-typed one — matching how
    /// most real-world operators leave `status.conditions` schema-loose
    /// anyway, since it's server-managed and not user-authored.
    #[serde(default)]
    #[schemars(with = "Vec<serde_json::Value>")]
    pub conditions: Vec<Condition>,
    #[serde(rename = "namespaceName", default)]
    pub namespace_name: String,
    #[serde(rename = "proxyPodName", default)]
    pub proxy_pod_name: String,
    #[serde(rename = "configMapName", default)]
    pub config_map_name: String,
    #[serde(rename = "observedGeneration", default)]
    pub observed_generation: i64,
    #[serde(
        rename = "lastReconcileTime",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_reconcile_time: Option<String>,
    #[serde(rename = "reconcileCount", default)]
    pub reconcile_count: i64,
}

pub const CONDITION_READY: &str = "Ready";

/// Insert-or-update a condition by type, matching the Go version's
/// `setCondition` (update in place if the type already exists, append
/// otherwise) so `status.conditions` never grows an entry per reconcile.
pub fn set_condition(
    status: &mut TenantPipelineStatus,
    ctype: &str,
    true_: bool,
    reason: &str,
    message: &str,
) {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    let now = Time(chrono::Utc::now());
    let status_str = if true_ { "True" } else { "False" }.to_string();

    if let Some(existing) = status.conditions.iter_mut().find(|c| c.type_ == ctype) {
        existing.status = status_str;
        existing.reason = reason.to_string();
        existing.message = message.to_string();
        existing.last_transition_time = now;
        return;
    }
    status.conditions.push(Condition {
        type_: ctype.to_string(),
        status: status_str,
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: now,
        observed_generation: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_condition_updates_in_place_not_append() {
        let mut status = TenantPipelineStatus::default();
        set_condition(&mut status, CONDITION_READY, true, "Reconciled", "ok");
        set_condition(&mut status, CONDITION_READY, false, "ProxyFailed", "boom");

        assert_eq!(
            status.conditions.len(),
            1,
            "condition must be updated in place, not appended"
        );
        assert_eq!(status.conditions[0].status, "False");
        assert_eq!(status.conditions[0].reason, "ProxyFailed");
    }

    #[test]
    fn spec_roundtrips_through_json() {
        let spec = TenantPipelineSpec {
            tenant_id: "acme".into(),
            display_name: "Acme Corp".into(),
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
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"tenantId\":\"acme\""));
        assert!(json.contains("\"requestsPerSecond\":200"));
        let back: TenantPipelineSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tenant_id, "acme");
        assert_eq!(back.rate_limit.requests_per_second, 200);
    }

    #[test]
    fn proxy_config_defaults_are_empty_not_missing() {
        // A spec with an empty proxy block must deserialize (defaults kick in),
        // matching the Go version's "image=='' -> apply default" pattern used
        // by the reconciler rather than requiring every field up front.
        let json = r#"{"tenantId":"acme","rateLimit":{"tier":"free","requestsPerSecond":10,"burstCapacity":20,"maxConcurrent":5}}"#;
        let spec: TenantPipelineSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.proxy.image, "");
        assert_eq!(spec.proxy.port, 0);
    }
}
