//! Native OAuth 2.0 device authorization grant for Azure DevOps - a pure-HTTP
//! replacement for shelling out to `az login`. Works on every shell (hosted web,
//! Android, desktop) with no Azure CLI, so it drops that dependency entirely.
//!
//! It uses the Azure CLI's well-known PUBLIC client id, exactly as `az` does
//! internally, so there is no app registration and no client secret. Registering
//! a dedicated POSEIDON app is a later hardening step (see BACKLOG); the flow is
//! identical, only the client id changes.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use crate::azure::AZURE_DEVOPS_RESOURCE;
use crate::ProviderError;

/// The Azure CLI's public client id: a first-party public client that supports
/// device code and already carries delegated Azure DevOps permission.
pub const AZURE_CLI_CLIENT_ID: &str = "04b07795-8ddb-461a-bbee-02f9e1bf7b46";

fn tenant_or_default(tenant: Option<&str>) -> String {
    // `organizations` = any work/school (Entra) tenant, which is what Azure
    // DevOps uses. A specific tenant narrows it.
    tenant
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("organizations")
        .to_string()
}

fn devicecode_url(tenant: Option<&str>) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
        tenant_or_default(tenant)
    )
}

fn token_url(tenant: Option<&str>) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
        tenant_or_default(tenant)
    )
}

/// `.default` targets the Azure DevOps resource; `offline_access` asks for a
/// refresh token so later polls renew silently instead of re-prompting.
fn scope() -> String {
    format!("{AZURE_DEVOPS_RESOURCE}/.default offline_access")
}

/// A started device-code flow: show `user_code` at `verification_uri`, then poll
/// with `device_code`.
#[derive(Debug, Clone)]
pub struct DeviceCodeStart {
    pub user_code: String,
    pub verification_uri: String,
    pub device_code: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// Tokens from a successful grant or refresh. `expires_at` is absolute so callers
/// can cheaply decide when to refresh.
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
}

/// Begin a device-code flow against the tenant (or `organizations`).
pub async fn start_device_code(
    http: &reqwest::Client,
    tenant: Option<&str>,
) -> Result<DeviceCodeStart, ProviderError> {
    let sc = scope();
    let resp = http
        .post(devicecode_url(tenant))
        .form(&[("client_id", AZURE_CLI_CLIENT_ID), ("scope", sc.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::NotSignedIn(format!(
            "device-code request failed: {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    let d: DeviceCodeResp = resp.json().await?;
    Ok(DeviceCodeStart {
        user_code: d.user_code,
        verification_uri: d.verification_uri,
        device_code: d.device_code,
        interval: d.interval.max(1),
        expires_in: d.expires_in,
    })
}

/// The result of one token poll during a device-code flow.
pub enum PollOutcome {
    Pending,
    SlowDown,
    Ready(TokenSet),
    Failed(String),
}

/// Poll the token endpoint once for a device-code grant.
pub async fn poll_once(
    http: &reqwest::Client,
    tenant: Option<&str>,
    device_code: &str,
) -> Result<PollOutcome, ProviderError> {
    let resp = http
        .post(token_url(tenant))
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", AZURE_CLI_CLIENT_ID),
            ("device_code", device_code),
        ])
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await?;
    Ok(classify_poll(status, &body))
}

/// Exchange a refresh token for a fresh token set (silent renewal).
pub async fn refresh(
    http: &reqwest::Client,
    tenant: Option<&str>,
    refresh_token: &str,
) -> Result<TokenSet, ProviderError> {
    let sc = scope();
    let resp = http
        .post(token_url(tenant))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", AZURE_CLI_CLIENT_ID),
            ("refresh_token", refresh_token),
            ("scope", sc.as_str()),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::NotSignedIn(format!(
            "token refresh failed - sign in again. ({})",
            body.chars().take(200).collect::<String>()
        )));
    }
    let t: TokenResp = resp.json().await?;
    Ok(into_token_set(t))
}

// ── wire types + pure classification (unit-tested) ───────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResp {
    user_code: String,
    verification_uri: String,
    device_code: String,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default)]
    expires_in: u64,
}
fn default_interval() -> u64 {
    5
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: i64,
}

#[derive(Deserialize)]
struct ErrResp {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

fn into_token_set(t: TokenResp) -> TokenSet {
    TokenSet {
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        expires_at: Utc::now() + Duration::seconds(t.expires_in.max(0)),
    }
}

/// Map a token-endpoint response to a poll outcome. Pure so the pending /
/// slow-down / success / failure branches are testable without a network.
fn classify_poll(status: u16, body: &[u8]) -> PollOutcome {
    if (200..300).contains(&status) {
        return match serde_json::from_slice::<TokenResp>(body) {
            Ok(t) => PollOutcome::Ready(into_token_set(t)),
            Err(e) => PollOutcome::Failed(format!("could not parse token response: {e}")),
        };
    }
    match serde_json::from_slice::<ErrResp>(body) {
        Ok(e) => match e.error.as_str() {
            "authorization_pending" => PollOutcome::Pending,
            "slow_down" => PollOutcome::SlowDown,
            other => PollOutcome::Failed(e.error_description.unwrap_or_else(|| other.to_string())),
        },
        Err(_) => PollOutcome::Failed(format!("token endpoint returned status {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_default_to_organizations_and_narrow_to_a_tenant() {
        assert!(devicecode_url(None).contains("/organizations/"));
        assert!(devicecode_url(Some("")).contains("/organizations/"));
        assert!(token_url(Some("contoso.com")).contains("/contoso.com/"));
        assert!(scope().starts_with(AZURE_DEVOPS_RESOURCE));
        assert!(scope().contains("offline_access"));
    }

    #[test]
    fn pending_and_slow_down_are_transient() {
        assert!(matches!(
            classify_poll(400, br#"{"error":"authorization_pending"}"#),
            PollOutcome::Pending
        ));
        assert!(matches!(
            classify_poll(400, br#"{"error":"slow_down"}"#),
            PollOutcome::SlowDown
        ));
    }

    #[test]
    fn success_yields_a_token_set_with_future_expiry() {
        let body = br#"{"access_token":"abc","refresh_token":"r","expires_in":3600}"#;
        match classify_poll(200, body) {
            PollOutcome::Ready(t) => {
                assert_eq!(t.access_token, "abc");
                assert_eq!(t.refresh_token.as_deref(), Some("r"));
                assert!(t.expires_at > Utc::now());
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn terminal_errors_fail_with_a_message() {
        match classify_poll(
            400,
            br#"{"error":"expired_token","error_description":"code expired"}"#,
        ) {
            PollOutcome::Failed(m) => assert_eq!(m, "code expired"),
            _ => panic!("expected Failed"),
        }
        assert!(matches!(
            classify_poll(400, b"not json"),
            PollOutcome::Failed(_)
        ));
    }
}
