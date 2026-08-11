//! Provider integrations for POSEIDEN.
//!
//! A [`Provider`] fetches raw work items + pipeline data from one upstream and
//! normalises it into `poseiden-core` shapes. Azure DevOps is the first (and
//! only, today) implementation; the trait is the seam that keeps the rest of
//! POSEIDEN provider-agnostic. Adding Jira / Linear / GitHub means adding a new
//! `impl Provider`, nothing else.
//!
//! The normalisation logic (Azure DevOps JSON → core types) is factored into
//! pure functions in [`azure`] so it can be unit-tested against sample
//! payloads without a network round-trip.

pub mod azure;
pub mod github;
pub mod gitlab;
pub mod oauth;
pub mod stub;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poseiden_core::{
    Pipeline, PipelineRun, ProviderKind, PullRequest, TeamConfig, WorkItem, WorkItemUpdate,
};

/// Errors a provider can surface. Deliberately coarse - a poll only reads, so
/// the only thing a caller does with these is log + skip that team's poll and
/// retry on the next tick.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Azure DevOps returned {status} for {url}: {body}")]
    Api {
        status: u16,
        url: String,
        body: String,
    },
    #[error("failed to build HTTP client: {0}")]
    Client(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("not signed in: {0}")]
    NotSignedIn(String),
    #[error("not found: {0}")]
    NotFound(String),
}

/// How a poll authenticates to the provider. Resolved per-poll by the caller so
/// short-lived OAuth tokens stay fresh.
///
/// - `Pat` - a Personal Access Token (HTTP Basic). The classic path.
/// - `Bearer` - an OAuth access token (HTTP Bearer), e.g. one brokered by the
///   Azure CLI (`az account get-access-token`). Lets a user authenticate with
///   their existing `az login` session instead of minting a PAT.
#[derive(Clone)]
pub enum Credential {
    Pat(String),
    Bearer(String),
}

impl Credential {
    /// The `Authorization` header value this credential produces.
    fn header_value(&self) -> String {
        use base64::Engine;
        match self {
            // Azure DevOps PAT auth = HTTP Basic, empty username, PAT as password.
            Credential::Pat(pat) => {
                let token = base64::engine::general_purpose::STANDARD.encode(format!(":{pat}"));
                format!("Basic {token}")
            }
            Credential::Bearer(token) => format!("Bearer {token}"),
        }
    }
}

/// A source POSEIDEN polls. One instance per configured project.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable provider slug stamped onto every normalised entity
    /// (`"azure-devops"`). Distinguishes items when an instance mixes providers.
    fn provider_name(&self) -> &str;

    /// The team's display name from config - the `team` field on every entity
    /// this provider emits, and what the UI scope selector filters on.
    fn team_name(&self) -> &str;

    /// All work items matching the project's query (or the ruleset's default
    /// query), normalised.
    async fn fetch_work_items(&self) -> Result<Vec<WorkItem>, ProviderError>;

    /// The pipelines this provider monitors - either the configured subset or
    /// all pipelines in the project.
    async fn fetch_pipelines(&self) -> Result<Vec<Pipeline>, ProviderError>;

    /// Pipeline runs finished/started at or after `since`. Bounding the window
    /// keeps each poll's cost proportional to recent activity rather than all
    /// history.
    async fn fetch_runs(&self, since: DateTime<Utc>) -> Result<Vec<PipelineRun>, ProviderError>;

    /// The active (open) pull requests across the team's project - the set in
    /// flight right now. Bounded to active PRs so poll cost tracks what's open,
    /// not all history.
    async fn fetch_pull_requests(&self) -> Result<Vec<PullRequest>, ProviderError>;

    /// A single pull request by id, normalised. Used to resolve a linked PR that
    /// fell outside the polled window (so its chip can still open in the browser).
    async fn fetch_pull_request(&self, id: i64) -> Result<PullRequest, ProviderError>;

    /// Apply an update to a work item's editable fields (state / tags) and
    /// return the provider's post-update view of the item (so the caller stores
    /// the canonical result - the provider may normalise tags, stamp a new
    /// changed date, set a closed date, etc.). Requires the credential to have
    /// write permission on the item.
    async fn update_work_item(
        &self,
        id: i64,
        update: &WorkItemUpdate,
    ) -> Result<WorkItem, ProviderError>;

    /// Link a pull request to a work item (add the ADO artifact-link relation),
    /// returning the work item's post-write view so the caller re-derives its
    /// linked PRs. Requires the credential to have write permission on the item.
    async fn link_pr(&self, work_item_id: i64, pr_id: i64) -> Result<WorkItem, ProviderError>;

    /// Remove the pull-request link from a work item, returning its post-write
    /// view. Idempotent-ish: errors if no such link exists.
    async fn unlink_pr(&self, work_item_id: i64, pr_id: i64) -> Result<WorkItem, ProviderError>;
}

/// Construct the right [`Provider`] for a team's configured kind. `credential`
/// is resolved by the caller (PAT from env, or a token brokered by `az`) - this
/// crate never touches the environment, so credential handling stays in one
/// place.
pub fn build_provider(
    cfg: &TeamConfig,
    credential: Credential,
) -> Result<Box<dyn Provider>, ProviderError> {
    match cfg.provider {
        ProviderKind::AzureDevOps => {
            Ok(Box::new(azure::AzureDevOpsProvider::new(cfg, credential)?))
        }
        ProviderKind::GitHub => Ok(Box::new(github::GithubProvider::new(cfg, credential)?)),
        ProviderKind::GitLab => Ok(Box::new(gitlab::GitlabProvider::new(cfg, credential)?)),
        // The stub needs no credential (see `stub`); the `_credential` is ignored.
        ProviderKind::Stub => Ok(Box::new(stub::StubProvider::new(cfg))),
    }
}

/// Azure DevOps' resource id for OAuth - the audience an access token must
/// target. Used by [`azure::acquire_cli_token`]. Re-exported for callers that
/// want to acquire a token themselves.
pub use azure::{DeviceCode, AZURE_DEVOPS_RESOURCE};
