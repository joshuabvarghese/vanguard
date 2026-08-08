//! Real Azure adapters for the traits in `cloud::mod`. Compiled only with
//! `--features azure` (see the root `Cargo.toml` for why this is opt-in:
//! the Azure SDK crates are heavy, and the default build/test/CI path —
//! Kind, mock backends — has no business paying that compile cost).
//!
//! All three adapters authenticate via `azure_identity::DefaultAzureCredential`,
//! which resolves credentials in this order and Just Works across every
//! environment this project runs in without any code change:
//!
//! 1. **Workload Identity** (`AZURE_CLIENT_ID`, `AZURE_TENANT_ID`,
//!    `AZURE_FEDERATED_TOKEN_FILE`) — set automatically by AKS when the pod's
//!    ServiceAccount is annotated per `manifests/azure/serviceaccount.yaml`.
//!    This is the only path used in production; no secret ever touches the
//!    pod.
//! 2. **Azure CLI** (`az login`) — local development against a real
//!    Key Vault/App Insights instance.
//! 3. **Environment variables** (`AZURE_CLIENT_SECRET`) — CI or non-AKS
//!    hosts, last resort.

pub mod identity;
pub mod keyvault;
pub mod monitor;

use azure_identity::DefaultAzureCredential;
use std::sync::Arc;

/// Builds the shared credential once at startup; every adapter clones the
/// `Arc` rather than each re-resolving the credential chain independently.
pub fn default_credential() -> anyhow::Result<Arc<DefaultAzureCredential>> {
    Ok(Arc::new(DefaultAzureCredential::default()))
}
