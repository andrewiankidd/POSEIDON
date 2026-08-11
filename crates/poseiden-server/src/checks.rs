//! Concrete Doctor checks for POSEIDEN.
//!
//! The [`poseiden_doctor`] crate is the generic engine; the checks that know
//! about POSEIDEN's dependencies live here, where the config + provider are in
//! reach. Today there's one: a per-team provider access check. It's the
//! parameterised-check pattern from crosspose - a *generic* "provider
//! access" check, one instance registered per team (derived from config), so
//! that when a team is added the Doctor starts watching that its access stays
//! open.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use poseiden_core::TeamConfig;
use poseiden_doctor::{Check, CheckResult, FixResult, Severity};

use crate::config_store::ConfigStore;

/// GitHub repo checked for new releases (`owner/name`).
const RELEASES_REPO: &str = "andrewiankidd/POSEIDEN";
/// How long a GitHub release lookup is cached, to stay well under the
/// unauthenticated rate limit (60/hr) even though the Doctor ticks often.
const UPDATE_CACHE_MINS: i64 = 30;

/// Process-wide cache of the last computed update result. Static so it survives
/// the per-tick rebuilding of check instances; keeps GitHub calls to ~2/hr.
static UPDATE_CACHE: Mutex<Option<(DateTime<Utc>, CheckResult)>> = Mutex::new(None);

/// A **non-critical** update check. Handles two release channels:
///
/// - **Stable** (running a `v1.2.3` tag): compares against the latest stable
///   release by semver, so someone on v2 isn't told about v1.9.
/// - **Rolling** (running the `latest-main` channel, whose tag is *moved* to a
///   new commit on every push): compares the running build's commit SHA to the
///   release's current target commit. Same tag name, different commit = update.
///
/// A newer release fails the check as a **Warning** -> the traffic light goes
/// AMBER, never RED (an out-of-date build still works). A `local` build skips.
pub struct UpdateCheck;

impl UpdateCheck {
    pub fn new() -> Self {
        Self
    }

    /// GET a GitHub API URL. `Ok(None)` on 404 (release/repo not found yet).
    async fn github_get(url: &str) -> Result<Option<serde_json::Value>, String> {
        let client = reqwest::Client::builder()
            .user_agent("poseiden")
            // Fail fast if GitHub is slow/unreachable so a network hiccup never
            // stalls the Doctor (which blocks the traffic light + the report).
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("GitHub returned {}", resp.status()));
        }
        Ok(Some(resp.json().await.map_err(|e| e.to_string())?))
    }

    /// Compute the update status (uncached). Branches on the running channel.
    async fn compute(version: &str, commit: &str) -> CheckResult {
        if let Some(running) = parse_version(version) {
            // Stable channel: compare against the latest stable release by semver.
            let url = format!("https://api.github.com/repos/{RELEASES_REPO}/releases/latest");
            match Self::github_get(&url).await {
                Ok(Some(json)) => {
                    let latest = json.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
                    match parse_version(latest) {
                        Some(remote) if remote > running => CheckResult::failed(format!(
                            "update available: {latest} (you have {version})"
                        )),
                        _ => CheckResult::ok(format!("up to date ({version})")),
                    }
                }
                Ok(None) => CheckResult::failed("no published release found yet"),
                Err(e) => CheckResult::failed(format!("could not check for updates: {e}")),
            }
        } else {
            // Rolling channel (e.g. latest-main): compare the target commit SHA.
            let url =
                format!("https://api.github.com/repos/{RELEASES_REPO}/releases/tags/{version}");
            match Self::github_get(&url).await {
                Ok(Some(json)) => {
                    let remote = json
                        .get("target_commitish")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if commit == "unknown" || remote.is_empty() {
                        // Can't compare - don't nag.
                        CheckResult::ok(format!("running {version}"))
                    } else if same_commit(remote, commit) {
                        CheckResult::ok(format!("up to date ({version} @ {})", short(commit)))
                    } else {
                        CheckResult::failed(format!("a newer {version} build is available"))
                    }
                }
                Ok(None) => CheckResult::failed(format!("no {version} release found")),
                Err(e) => CheckResult::failed(format!("could not check for updates: {e}")),
            }
        }
    }
}

impl Default for UpdateCheck {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a `v?MAJOR.MINOR.PATCH` tag into a comparable tuple, or `None` if it
/// isn't semver (e.g. `latest-main`). Trailing suffixes on the patch (`-rc1`)
/// are ignored.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch_raw = parts.next().unwrap_or("0");
    let patch_digits: String = patch_raw
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let patch = patch_digits.parse().unwrap_or(0);
    Some((major, minor, patch))
}

/// Whether two commit refs denote the same commit, tolerating short vs full SHA.
fn same_commit(a: &str, b: &str) -> bool {
    let a = a.trim().to_ascii_lowercase();
    let b = b.trim().to_ascii_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.starts_with(&b) || b.starts_with(&a)
}

/// First 7 chars of a SHA for display.
fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[async_trait]
impl Check for UpdateCheck {
    fn id(&self) -> String {
        "app-updates".to_string()
    }

    fn label(&self) -> String {
        "Application updates".to_string()
    }

    // A newer release available is amber, not red - the app still works.
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    async fn run(&self) -> CheckResult {
        let version = poseiden_core::version();
        if version == "local" {
            return CheckResult::ok("development build - update check skipped");
        }

        // Serve from cache if fresh (keeps GitHub calls rare).
        if let Some((at, result)) = UPDATE_CACHE.lock().unwrap().clone() {
            if Utc::now() - at < Duration::minutes(UPDATE_CACHE_MINS) {
                return result;
            }
        }

        let result = Self::compute(&version, &poseiden_core::commit()).await;
        *UPDATE_CACHE.lock().unwrap() = Some((Utc::now(), result.clone()));
        result
    }
}

/// The access-check key for a team (crosspose's parameterised `additional-key`).
fn access_key(team_name: &str) -> String {
    format!("ado-access:{team_name}")
}

/// A self-healing meta-check: it ensures every configured team has a registered
/// provider access check, and - as its **auto-fix** - registers the missing
/// ones and prunes orphaned ones (teams that were removed). This is the "Doctor
/// detects teams → ensures they have checks → registers them automatically"
/// mechanism: adding a team never requires hand-registering its check.
///
/// The registered set is persisted per owner in the DB (`doctor.checks`),
/// so registrations survive restarts.
pub struct TeamCheckReconciler {
    store: ConfigStore,
    owner: String,
}

impl TeamCheckReconciler {
    pub fn new(store: ConfigStore, owner: String) -> Self {
        Self { store, owner }
    }

    /// The registered `ado-access:*` keys that exactly match current teams
    /// (missing added, orphans dropped), preserving any non-access keys for
    /// future check types (helm repos, …).
    async fn reconciled_keys(&self) -> Vec<String> {
        let want: HashSet<String> = self
            .store
            .teams(&self.owner)
            .await
            .unwrap_or_default()
            .iter()
            .map(|t| access_key(&t.name))
            .collect();
        let mut keys: Vec<String> = self
            .store
            .registered_checks(&self.owner)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|k| !k.starts_with("ado-access:")) // keep other check kinds
            .collect();
        let mut sorted: Vec<String> = want.into_iter().collect();
        sorted.sort();
        keys.extend(sorted);
        keys
    }
}

#[async_trait]
impl Check for TeamCheckReconciler {
    fn id(&self) -> String {
        "doctor:team-checks".to_string()
    }

    fn label(&self) -> String {
        "Team access checks registered".to_string()
    }

    // A missing registration self-heals on the next tick, so it's a warning,
    // not critical.
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn can_fix(&self) -> bool {
        true
    }

    // The headline auto-fix: register/prune per-team checks with no user action.
    fn auto_fix(&self) -> bool {
        true
    }

    async fn run(&self) -> CheckResult {
        let registered: HashSet<String> = self
            .store
            .registered_checks(&self.owner)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        let teams = self.store.teams(&self.owner).await.unwrap_or_default();
        let want: HashSet<String> = teams.iter().map(|t| access_key(&t.name)).collect();

        let missing: Vec<&String> = teams
            .iter()
            .map(|t| &t.name)
            .filter(|n| !registered.contains(&access_key(n)))
            .collect();
        let orphans: Vec<&String> = registered
            .iter()
            .filter(|k| k.starts_with("ado-access:") && !want.contains(*k))
            .collect();

        if missing.is_empty() && orphans.is_empty() {
            CheckResult::ok(format!("{} team access check(s) registered", teams.len()))
        } else {
            CheckResult::failed(format!(
                "{} team check(s) to register, {} orphaned to remove",
                missing.len(),
                orphans.len()
            ))
        }
    }

    async fn fix(&self) -> FixResult {
        let keys = self.reconciled_keys().await;
        match self.store.set_registered_checks(&self.owner, keys).await {
            Ok(_) => FixResult::ok("registered access checks for all teams"),
            Err(e) => FixResult::failed(format!("could not persist registration: {e}")),
        }
    }
}

/// Verifies POSEIDEN can obtain a provider credential for one team -
/// either a configured PAT, or (for Azure DevOps) a token brokered by the Azure
/// CLI for the team's tenant. Failure is Critical: without it, that team's poll
/// can't run.
///
/// The fix is interactive (device-code sign-in), so it's surfaced to the UI via
/// `fix_action = "sign-in"` rather than a server-side auto-fix.
pub struct AzureDevOpsAccessCheck {
    team_name: String,
    tenant: Option<String>,
    pat_env: String,
    /// The owner's device-code token cache file, so the probe reads the same
    /// session the owner signs in to.
    token_path: std::path::PathBuf,
    /// A stub/demo provider needs no credential, so the check is a no-op pass.
    stub: bool,
    /// GitHub / GitLab read public repos anonymously - no sign-in required, so
    /// the check passes green when no token is configured.
    anon_ok: bool,
    /// Provider display name for the check label (this check is registered for
    /// every provider, not just Azure DevOps).
    provider_label: &'static str,
}

impl AzureDevOpsAccessCheck {
    pub fn from_team(team: &TeamConfig, token_path: std::path::PathBuf) -> Self {
        use poseiden_core::ProviderKind;
        let (anon_ok, provider_label) = match team.provider {
            ProviderKind::Stub => (false, "Demo"),
            ProviderKind::GitHub => (true, "GitHub"),
            ProviderKind::GitLab => (true, "GitLab"),
            ProviderKind::AzureDevOps => (false, "Azure DevOps"),
        };
        Self {
            team_name: team.name.clone(),
            tenant: team.tenant.clone(),
            pat_env: team.auth.pat_env.clone(),
            token_path,
            stub: matches!(team.provider, ProviderKind::Stub),
            anon_ok,
            provider_label,
        }
    }
}

#[async_trait]
impl Check for AzureDevOpsAccessCheck {
    fn id(&self) -> String {
        // Stable, parameterised id - the dedupe key + fix address.
        format!("ado-access:{}", self.team_name)
    }

    fn label(&self) -> String {
        format!("{} access - {}", self.provider_label, self.team_name)
    }

    fn severity(&self) -> Severity {
        Severity::Critical
    }

    fn can_fix(&self) -> bool {
        true
    }

    fn fix_action(&self) -> Option<String> {
        // Interactive: the frontend runs the device-code sign-in flow.
        Some("sign-in".to_string())
    }

    async fn run(&self) -> CheckResult {
        // The stub/demo provider is offline - no credential to check.
        if self.stub {
            return CheckResult::ok("Demo (stub) provider - no credential needed");
        }
        // A configured PAT is taken as authoritative (verifying it would cost an
        // API call every tick).
        if std::env::var(&self.pat_env)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return CheckResult::ok(format!(
                "Personal Access Token configured (${})",
                self.pat_env
            ));
        }
        // GitHub / GitLab read public repos without any credential - no sign-in
        // needed. (A private repo would need a token in `pat_env`, handled above.)
        if self.anon_ok {
            return CheckResult::ok("Public repo - anonymous read, no sign-in needed");
        }
        // Otherwise the credential comes from the device-code session - actually
        // acquiring a token (refreshing if needed) is the real check.
        match crate::service::acquire_cached_token(&self.token_path, self.tenant.as_deref()).await {
            Ok(_) => CheckResult::ok("Signed in with Azure (device code)"),
            Err(e) => CheckResult::failed(format!("no valid credential - {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_reads_semver_and_rejects_channels() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v2.0.0-rc1"), Some((2, 0, 0)));
        assert_eq!(parse_version("v1.4"), Some((1, 4, 0)));
        // Rolling channels + dev are NOT semver -> compared by commit instead.
        assert_eq!(parse_version("latest-main"), None);
        assert_eq!(parse_version("local"), None);
    }

    #[test]
    fn semver_ordering_drives_stable_updates() {
        assert!(parse_version("v1.3.0") > parse_version("v1.2.9"));
        assert!(parse_version("v2.0.0") > parse_version("v1.9.9"));
        assert!(parse_version("v1.2.3") == parse_version("1.2.3"));
    }

    #[test]
    fn same_commit_tolerates_short_vs_full_sha() {
        assert!(same_commit("abc123def456789", "abc123d"));
        assert!(same_commit("ABC123D", "abc123def456789"));
        assert!(same_commit("deadbeef", "deadbeef"));
        assert!(!same_commit("abc1234", "def5678"));
        assert!(!same_commit("", "abc1234")); // unknown commit never matches
    }
}
