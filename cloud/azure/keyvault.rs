//! Azure Key Vault adapter for [`SecretsProvider`] — the control plane's
//! *own* secrets (bootstrap admin token, inter-service HMAC key), fetched
//! at boot via Workload Identity.
//!
//! Deliberately **not** used for tenant proxy TLS material. That's a
//! different trust boundary — per-tenant secrets that Kubernetes workloads
//! (not the vanguard control-plane process) need mounted as files — and
//! the Azure-native answer for that is the Key Vault Provider for Secrets
//! Store CSI Driver (`manifests/azure/secretproviderclass.yaml`), which
//! syncs a Key Vault secret straight into a pod volume without Vanguard's
//! code ever holding tenant secret material in process memory. Routing
//! everything through one Rust `SecretsProvider::get_secret` call would be
//! simpler to write and meaningfully worse architecture: it would make the
//! control plane a single point of exposure for every tenant's secrets it
//! never actually needs to read.

use crate::cloud::SecretsProvider;
use async_trait::async_trait;
use azure_identity::DefaultAzureCredential;
use azure_security_keyvault::SecretClient;
use std::sync::Arc;

pub struct AzureKeyVaultSecrets {
    client: SecretClient,
}

impl AzureKeyVaultSecrets {
    /// `vault_url` — e.g. `https://vanguard-kv.vault.azure.net`, from
    /// `VANGUARD_KEYVAULT_URL` (see `main.rs`); set by the Bicep deployment
    /// output in `infra/bicep/main.bicep`.
    pub fn new(vault_url: &str, credential: Arc<DefaultAzureCredential>) -> anyhow::Result<Self> {
        let client = SecretClient::new(vault_url, credential)?;
        Ok(Self { client })
    }
}

#[async_trait]
impl SecretsProvider for AzureKeyVaultSecrets {
    async fn get_secret(&self, name: &str) -> anyhow::Result<String> {
        let secret = self.client.get(name).await?;
        Ok(secret.value)
    }
}
