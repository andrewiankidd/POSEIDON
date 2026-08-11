//! Optional defence-in-depth: verify the identity token oauth2-proxy forwards,
//! instead of blindly trusting the `X-Auth-Request-Email` header.
//!
//! The header model is only as strong as the network boundary (see the chart's
//! `networkPolicy`): anything that can reach the app directly can set the header
//! and impersonate a tenant. When a [`TokenVerifier`] is configured, the `Scoped`
//! extractor instead reads the forwarded **access token**
//! (`X-Auth-Request-Access-Token`), verifies its signature against the IdP's JWKS
//! and its `iss`/`aud`/`exp`, and takes the owner from the verified `email` claim.
//! A missing or invalid token then fails closed - a spoofed header alone buys
//! nothing without a validly signed token.
//!
//! Opt-in and instance-level, from the environment (there is no config file):
//!   POSEIDEN_AUTH_JWKS_URL     the IdP's JWKS endpoint (enables verification)
//!   POSEIDEN_AUTH_ISSUER       expected `iss` (optional but recommended)
//!   POSEIDEN_AUTH_AUDIENCE     expected `aud` (optional but recommended)
//!   POSEIDEN_AUTH_EMAIL_CLAIM  claim to read as the owner (default: `email`)

use std::collections::HashMap;
use std::sync::Arc;

use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde_json::Value;
use tokio::sync::RwLock;

pub const ENV_JWKS_URL: &str = "POSEIDEN_AUTH_JWKS_URL";
pub const ENV_ISSUER: &str = "POSEIDEN_AUTH_ISSUER";
pub const ENV_AUDIENCE: &str = "POSEIDEN_AUTH_AUDIENCE";
pub const ENV_EMAIL_CLAIM: &str = "POSEIDEN_AUTH_EMAIL_CLAIM";

/// Verifies IdP-signed JWTs against a cached JWKS. Cheap to clone (shared cache).
#[derive(Clone)]
pub struct TokenVerifier {
    jwks_url: String,
    issuer: Option<String>,
    audience: Option<String>,
    email_claim: String,
    http: reqwest::Client,
    keys: Arc<RwLock<HashMap<String, Jwk>>>, // kid -> key
}

impl TokenVerifier {
    /// Build from the environment. `None` when `POSEIDEN_AUTH_JWKS_URL` is unset,
    /// which leaves the plain header-trust behaviour in place (verification off).
    pub fn from_env() -> Option<Self> {
        let jwks_url = env_nonempty(ENV_JWKS_URL)?;
        Some(Self {
            jwks_url,
            issuer: env_nonempty(ENV_ISSUER),
            audience: env_nonempty(ENV_AUDIENCE),
            email_claim: env_nonempty(ENV_EMAIL_CLAIM).unwrap_or_else(|| "email".to_string()),
            http: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Verify `token` and return its email claim, or an error describing why it was
    /// rejected. Fails closed: any problem (bad signature, wrong iss/aud, expired,
    /// unknown key, missing claim) is an `Err`.
    pub async fn verify_email(&self, token: &str) -> Result<String, String> {
        let header = decode_header(token).map_err(|e| format!("unreadable token header: {e}"))?;

        // Take the algorithm from the token header, but refuse symmetric / MAC
        // algorithms up front: a JWKS holds asymmetric public keys, and allowing
        // HS* would open the classic "sign with the public key as an HMAC secret"
        // confusion. (jsonwebtoken's typed keys also reject this, but be explicit.)
        let alg = header.alg;
        if matches!(alg, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512) {
            return Err("symmetric token algorithm is not allowed".to_string());
        }

        let kid = header
            .kid
            .ok_or_else(|| "token has no key id (kid)".to_string())?;
        let jwk = self.jwk_for(&kid).await?;
        let key = DecodingKey::from_jwk(&jwk).map_err(|e| format!("unusable JWK: {e}"))?;

        let mut validation = Validation::new(alg);
        if let Some(iss) = &self.issuer {
            validation.set_issuer(&[iss])
        }
        match &self.audience {
            Some(aud) => validation.set_audience(&[aud]),
            None => validation.validate_aud = false, // no expected aud -> don't require one
        }

        let data = decode::<Value>(token, &key, &validation)
            .map_err(|e| format!("token verification failed: {e}"))?;
        extract_email(&data.claims, &self.email_claim)
    }

    /// The JWK for `kid`, refetching the JWKS once if it isn't cached (key rotation).
    async fn jwk_for(&self, kid: &str) -> Result<Jwk, String> {
        if let Some(jwk) = self.keys.read().await.get(kid).cloned() {
            return Ok(jwk);
        }
        self.refresh().await?;
        self.keys
            .read()
            .await
            .get(kid)
            .cloned()
            .ok_or_else(|| format!("no signing key for kid '{kid}' in the JWKS"))
    }

    /// Fetch the JWKS and replace the cache.
    async fn refresh(&self) -> Result<(), String> {
        let set: JwkSet = self
            .http
            .get(&self.jwks_url)
            .send()
            .await
            .map_err(|e| format!("JWKS fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("JWKS endpoint returned an error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JWKS parse failed: {e}"))?;
        let mut map = self.keys.write().await;
        map.clear();
        for jwk in set.keys {
            if let Some(kid) = jwk.common.key_id.clone() {
                map.insert(kid, jwk);
            }
        }
        Ok(())
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Pull the configured email claim out of the verified payload.
fn extract_email(claims: &Value, claim: &str) -> Result<String, String> {
    claims
        .get(claim)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("token is missing a non-empty '{claim}' claim"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verifier() -> TokenVerifier {
        TokenVerifier {
            jwks_url: "http://127.0.0.1:0/unused".to_string(),
            issuer: None,
            audience: None,
            email_claim: "email".to_string(),
            http: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    #[test]
    fn extract_email_reads_the_claim() {
        let c = serde_json::json!({ "email": "a@b.com", "sub": "1" });
        assert_eq!(extract_email(&c, "email").unwrap(), "a@b.com");
    }

    #[test]
    fn extract_email_rejects_missing_empty_or_nonstring() {
        assert!(extract_email(&serde_json::json!({ "sub": "1" }), "email").is_err());
        assert!(extract_email(&serde_json::json!({ "email": "" }), "email").is_err());
        assert!(extract_email(&serde_json::json!({ "email": 42 }), "email").is_err());
    }

    #[test]
    fn extract_email_honours_a_custom_claim() {
        let c = serde_json::json!({ "preferred_username": "u@x.io" });
        assert_eq!(extract_email(&c, "preferred_username").unwrap(), "u@x.io");
    }

    // A symmetric-algorithm token must be rejected BEFORE any JWKS fetch, so this
    // needs no network - it proves the alg-confusion guard fires first.
    #[tokio::test]
    async fn rejects_symmetric_algorithm_tokens() {
        use jsonwebtoken::{encode, EncodingKey, Header};
        let claims = serde_json::json!({ "email": "a@b.com", "exp": 9_999_999_999u64 });
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"attacker-known-public-key"),
        )
        .unwrap();
        let err = verifier().verify_email(&token).await.unwrap_err();
        assert!(err.contains("symmetric"), "unexpected error: {err}");
    }
}
