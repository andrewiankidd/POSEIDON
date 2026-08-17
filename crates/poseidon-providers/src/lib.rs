//! Provider integrations for POSEIDON.
//!
//! A [`Provider`] fetches raw work items + pipeline data from one upstream and
//! normalises it into `poseidon-core` shapes. Azure DevOps is the first (and
//! only, today) implementation; the trait is the seam that keeps the rest of
//! POSEIDON provider-agnostic. Adding Jira / Linear / GitHub means adding a new
//! `impl Provider`, nothing else.
//!
//! The normalisation logic (Azure DevOps JSON → core types) is factored into
//! pure functions in [`azure`] so it can be unit-tested against sample
//! payloads without a network round-trip.

pub mod azure;
pub mod catalog;
pub mod github;
pub mod gitlab;
pub mod oauth;
pub mod stub;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poseidon_core::{
    EditableField, FieldChange, Pipeline, PipelineRun, ProviderKind, PullRequest, TeamConfig,
    WorkItem, WorkItemUpdate,
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

/// A source POSEIDON polls. One instance per configured project.
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

    /// The area path a work item was CREATED under (its first revision), for the
    /// "moved in from another board" source signal. Immutable, so callers fetch it
    /// once and cache. `None` when the provider can't tell (the default) or the item
    /// has no distinct origin. Only Azure DevOps implements it today.
    async fn fetch_origin_area(&self, _id: i64) -> Result<Option<String>, ProviderError> {
        Ok(None)
    }

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

    /// The set of fields a person may edit for this work item, discovered from the
    /// item's TYPE (a Bug exposes Repro Steps + System Info; a User Story exposes
    /// Acceptance Criteria; every type has a Description). Rich (HTML) fields arrive
    /// as markdown. Provider-normalised into [`EditableField`] so the editor is
    /// provider-agnostic. Default: none - a provider that doesn't support field
    /// editing returns an empty set and the editor shows nothing to edit.
    async fn editable_fields(&self, _id: i64) -> Result<Vec<EditableField>, ProviderError> {
        Ok(Vec::new())
    }

    /// Write changed fields back to the provider and return the item's post-write
    /// view. `changes` carries each field's `reference` + new value (markdown for
    /// rich fields; the provider converts to its native format). Explicit,
    /// user-initiated - never called by a poll. Default: unsupported.
    async fn update_fields(
        &self,
        _id: i64,
        _changes: &[FieldChange],
    ) -> Result<WorkItem, ProviderError> {
        Err(ProviderError::Config(
            "this provider does not support field editing".into(),
        ))
    }

    /// Link a pull request to a work item (add the ADO artifact-link relation),
    /// returning the work item's post-write view so the caller re-derives its
    /// linked PRs. Requires the credential to have write permission on the item.
    async fn link_pr(&self, work_item_id: i64, pr_id: i64) -> Result<WorkItem, ProviderError>;

    /// Remove the pull-request link from a work item, returning its post-write
    /// view. Idempotent-ish: errors if no such link exists.
    async fn unlink_pr(&self, work_item_id: i64, pr_id: i64) -> Result<WorkItem, ProviderError>;

    /// Mark work item `id` as a duplicate of `duplicate_of`, using the provider's
    /// NATIVE mechanism (they differ): Azure DevOps adds a *Duplicate Of* work-item
    /// link; GitLab runs the `/duplicate` quick action (which also closes it); GitHub
    /// applies the `duplicate` label, comments "Duplicate of #N", and closes as
    /// not-planned. Returns the item's post-write view. Explicit + user-initiated,
    /// never called by a poll. Default: unsupported.
    async fn mark_duplicate(
        &self,
        _id: i64,
        _duplicate_of: i64,
    ) -> Result<WorkItem, ProviderError> {
        Err(ProviderError::Config(
            "this provider does not support marking duplicates".into(),
        ))
    }
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
pub use catalog::{
    canonical_product_slug, repo_product_map, BackstageCatalog, CatalogEntity, CatalogError,
    CatalogSource, CsvCatalog, FieldMap, PortCatalog,
};
