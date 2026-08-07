//! No-op / in-memory adapters — used whenever the `azure` feature is off
//! (default `cargo build`, `cargo test`, `make demo-run` against Kind) and
//! as the default in `ApiState` so nothing about local development or the
//! existing 41-test suite changes just because Azure adapters now exist.

use super::{CallerIdentity, IdentityVerifier, SecretsProvider, TelemetrySink};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;

/// Accepts every request unauthenticated. This is the default
/// `IdentityVerifier` — production deployments opt into
/// `azure::identity::EntraIdVerifier` explicitly via `VANGUARD_AUTH_MODE=entra`
/// (see `main.rs`), rather than auth being silently on or off based on
/// which Cargo features happened to be compiled in.
#[derive(Default)]
pub struct NoopIdentityVerifier;

#[async_trait]
impl IdentityVerifier for NoopIdentityVerifier {
    async fn verify(&self, _bearer_token: &str) -> anyhow::Result<CallerIdentity> {
        Ok(CallerIdentity {
            subject: "anonymous".into(),
            roles: vec!["Tenant.Write".into(), "Tenant.Read".into()],
            claims: HashMap::new(),
        })
    }
}

/// In-memory secret store, seeded at construction. Stands in for Key
/// Vault in tests and in local dev where no Azure credentials exist.
#[derive(Default)]
pub struct InMemorySecretsProvider {
    secrets: RwLock<HashMap<String, String>>,
}

impl InMemorySecretsProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_secret(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets
            .write()
            .unwrap()
            .insert(name.into(), value.into());
        self
    }
}

#[async_trait]
impl SecretsProvider for InMemorySecretsProvider {
    async fn get_secret(&self, name: &str) -> anyhow::Result<String> {
        self.secrets
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("secret {name:?} not found (in-memory provider)"))
    }
}

/// Drops events on the floor. Used whenever Azure Monitor isn't
/// configured, so `TelemetrySink::emit` calls are always safe to make
/// unconditionally from `api.rs`/`chaos.rs` without an `if let Some(..)`
/// at every call site.
#[derive(Default)]
pub struct NoopTelemetrySink;

#[async_trait]
impl TelemetrySink for NoopTelemetrySink {
    async fn emit(&self, _event_name: &str, _properties: HashMap<String, String>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_identity_verifier_accepts_anything() {
        let v = NoopIdentityVerifier;
        let id = v.verify("garbage-token").await.unwrap();
        assert_eq!(id.subject, "anonymous");
    }

    #[tokio::test]
    async fn in_memory_secrets_provider_roundtrips() {
        let p = InMemorySecretsProvider::new().with_secret("admin-token", "s3cr3t");
        assert_eq!(p.get_secret("admin-token").await.unwrap(), "s3cr3t");
        assert!(p.get_secret("missing").await.is_err());
    }
}
