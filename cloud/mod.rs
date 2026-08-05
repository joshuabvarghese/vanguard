//! Cloud provider abstraction layer.
//!
//! This module is the same architectural move as `k8s_backend.rs`'s
//! `K8sBackend`/`CrdBackend` traits, applied one layer up: the control
//! plane's core code (`api.rs`, `operator.rs`, `main.rs`) depends only on
//! the traits defined here, never on an Azure SDK type directly. Real
//! Azure adapters live in `cloud::azure` behind the `azure` Cargo feature;
//! `cloud::mock` provides no-op / in-memory implementations used for local
//! development (`make demo-run` against Kind) and every existing test.
//!
//! Why a trait seam instead of `#[cfg(feature = "azure")]` scattered
//! through `api.rs`/`operator.rs`: it keeps Vanguard's core reconcile and
//! HTTP logic — the part with 41 unit tests and zero external
//! dependencies — provider-agnostic. Swapping Azure for GCP or a bare-metal
//! deployment later means writing new adapters in `cloud/`, not touching
//! business logic. It also means the `azure` feature can stay off the
//! default build (fast CI, no heavy SDK compile) while still being a real,
//! wired-in capability rather than a demo bolted on the side.
//!
//! Three concerns are split deliberately rather than lumped into one
//! "AzureClient" god-object:
//!
//! - [`SecretsProvider`] — secrets *the control plane itself* needs
//!   (bootstrap admin token, webhook HMAC key). Tenant-workload secrets
//!   (proxy TLS material) deliberately do **not** go through this trait —
//!   see `manifests/azure/secretproviderclass.yaml` for why that's a CSI
//!   driver's job, not application code.
//! - [`IdentityVerifier`] — authenticates *inbound* callers of the REST
//!   API (Entra ID access tokens in production, no-op in local dev).
//! - [`TelemetrySink`] — emits business-level events (tenant created,
//!   chaos triggered) to a metrics/observability backend, distinct from
//!   `tracing`'s logs/spans which are wired separately in
//!   `cloud::azure::monitor`.

use async_trait::async_trait;
use std::collections::HashMap;

pub mod mock;

#[cfg(feature = "azure")]
pub mod azure;

/// Resolves secrets the control-plane process itself needs at runtime.
/// Backed by Azure Key Vault in production (`cloud::azure::keyvault`),
/// an in-memory map in tests (`cloud::mock`).
#[async_trait]
pub trait SecretsProvider: Send + Sync {
    async fn get_secret(&self, name: &str) -> anyhow::Result<String>;
}

/// Verifies a bearer token presented to the control-plane REST API.
/// Returns the caller's identity on success. `NoopIdentityVerifier`
/// (the default, used in tests and local `demo-run`) accepts everything
/// unauthenticated; `azure::identity::EntraIdVerifier` validates a real
/// Entra ID (Azure AD) access token against your tenant's JWKS.
#[async_trait]
pub trait IdentityVerifier: Send + Sync {
    async fn verify(&self, bearer_token: &str) -> anyhow::Result<CallerIdentity>;
}

#[derive(Debug, Clone, Default)]
pub struct CallerIdentity {
    /// The token's `sub`/`oid` claim.
    pub subject: String,
    /// App roles / scopes granted to the caller (Entra ID `roles` claim),
    /// used by `api.rs` to gate mutating routes behind e.g. `Tenant.Write`.
    pub roles: Vec<String>,
    pub claims: HashMap<String, serde_json::Value>,
}

/// Emits a named business event with string properties. Fans out tenant
/// lifecycle / chaos events onto Azure Monitor (as a custom event via the
/// tracing→OpenTelemetry pipeline) in addition to the existing in-process
/// `Store` event bus the TUI already reads from — the two are
/// complementary, not a replacement for each other.
#[async_trait]
pub trait TelemetrySink: Send + Sync {
    async fn emit(&self, event_name: &str, properties: HashMap<String, String>);
}
