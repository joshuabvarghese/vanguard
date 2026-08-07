//! Validates Entra ID (Azure AD) access tokens presented to the control
//! plane's REST API — i.e. answers "is this a real, unexpired token
//! issued by *our* tenant for *this* API", not "is this user allowed to
//! do X" (that's `CallerIdentity::roles`, checked by `api.rs`).
//!
//! Design choice worth calling out: this validates the JWT locally
//! (fetch JWKS once, cache, verify signature/`iss`/`aud`/`exp` in-process)
//! rather than calling Microsoft Graph or the introspection endpoint on
//! every request. A control plane that round-trips to Entra ID for every
//! `POST /tenants` call has taken a hard external dependency on its own
//! hot path; local JWKS validation means Entra ID being briefly
//! unreachable doesn't take Vanguard's API down with it, and it's the
//! same trade-off `kube-rs`'s own token review flow makes.

use crate::cloud::{CallerIdentity, IdentityVerifier};
use async_trait::async_trait;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{Duration, Instant};

/// Config comes from `VANGUARD_ENTRA_TENANT_ID` / `VANGUARD_ENTRA_AUDIENCE`
/// (the App Registration's Application ID URI or client ID) — see
/// `main.rs::build_identity_verifier`.
pub struct EntraIdVerifier {
    tenant_id: String,
    audience: String,
    http: reqwest::Client,
    jwks_cache: RwLock<Option<(JwkSet, Instant)>>,
}

const JWKS_TTL: Duration = Duration::from_secs(3600);

impl EntraIdVerifier {
    pub fn new(tenant_id: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            audience: audience.into(),
            http: reqwest::Client::new(),
            jwks_cache: RwLock::new(None),
        }
    }

    fn jwks_uri(&self) -> String {
        // v2.0 endpoint — issues tokens with `iss` =
        // "https://login.microsoftonline.com/{tenant}/v2.0".
        format!(
            "https://login.microsoftonline.com/{}/discovery/v2.0/keys",
            self.tenant_id
        )
    }

    fn issuer(&self) -> String {
        format!("https://login.microsoftonline.com/{}/v2.0", self.tenant_id)
    }

    async fn jwks(&self) -> anyhow::Result<JwkSet> {
        {
            let cache = self.jwks_cache.read().await;
            if let Some((jwks, fetched_at)) = cache.as_ref() {
                if fetched_at.elapsed() < JWKS_TTL {
                    return Ok(jwks.clone());
                }
            }
        }
        let jwks: JwkSet = self
            .http
            .get(self.jwks_uri())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        *self.jwks_cache.write().await = Some((jwks.clone(), Instant::now()));
        Ok(jwks)
    }
}

#[async_trait]
impl IdentityVerifier for EntraIdVerifier {
    async fn verify(&self, bearer_token: &str) -> anyhow::Result<CallerIdentity> {
        let header = decode_header(bearer_token)?;
        let kid = header
            .kid
            .ok_or_else(|| anyhow::anyhow!("token header missing 'kid'"))?;

        let jwks = self.jwks().await?;
        let jwk = jwks
            .find(&kid)
            .ok_or_else(|| anyhow::anyhow!("no matching signing key for kid {kid:?}"))?;
        let decoding_key = DecodingKey::from_jwk(jwk)?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.audience]);
        validation.set_issuer(&[self.issuer()]);

        let data = decode::<HashMap<String, serde_json::Value>>(
            bearer_token,
            &decoding_key,
            &validation,
        )?;

        let claims = data.claims;
        let subject = claims
            .get("oid")
            .or_else(|| claims.get("sub"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let roles = claims
            .get("roles")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        Ok(CallerIdentity {
            subject,
            roles,
            claims,
        })
    }
}

/// Convenience constructor reading the standard env vars, returned as a
/// trait object so `main.rs` can pick between this and the mock verifier
/// with one `Arc<dyn IdentityVerifier>` binding.
pub fn from_env() -> Option<Arc<dyn IdentityVerifier>> {
    let tenant_id = std::env::var("VANGUARD_ENTRA_TENANT_ID").ok()?;
    let audience = std::env::var("VANGUARD_ENTRA_AUDIENCE").ok()?;
    Some(Arc::new(EntraIdVerifier::new(tenant_id, audience)))
}
