use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A pull request observed across a team's project. POSEIDON fetches the
/// *active* set (open PRs) so the poll cost stays proportional to what's in
/// flight rather than all history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequest {
    /// Provider-native PR id (Azure DevOps `pullRequestId`).
    pub id: i64,
    pub provider: String,
    /// The team (board) this PR was fetched under - the `[[team]]` config `name`.
    pub team: String,
    pub title: String,
    pub status: PrStatus,
    /// Whether the PR is a draft (work-in-progress, not yet ready for review).
    pub is_draft: bool,
    /// Repository the PR targets, if known.
    pub repository: Option<String>,
    /// Display name of the author, if known.
    pub author: Option<String>,
    /// When the PR was created.
    pub created_at: Option<DateTime<Utc>>,
    /// Source / target branch short names (the `refs/heads/` prefix stripped).
    pub source_branch: Option<String>,
    pub target_branch: Option<String>,
    /// Number of reviewers assigned.
    pub reviewer_count: i64,
    /// Web URL to the PR in the provider's UI.
    pub url: String,
    /// Hygiene flags (stale-open / stale-draft), evaluated on read against the
    /// team's effective ruleset. Not stored - a runtime field, so it defaults to
    /// empty when the PR comes from the store and is filled by the read path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<crate::EntityFlag>,
    /// Work-item ids linked to this PR (inverted from work-item relations at read
    /// time). Runtime-only, not stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_work_items: Vec<i64>,
}

/// A pull request linked to a work item, with just enough to render + colour a
/// chip on the Work Items screen. Resolved on read by joining a work item's
/// linked PR ids against the polled PR set; a link we have no PR data for is
/// still shown (status `Unknown`, empty url).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkedPr {
    pub id: i64,
    pub status: PrStatus,
    pub is_draft: bool,
    pub url: String,
}

/// Lifecycle state of a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrStatus {
    Active,
    Completed,
    Abandoned,
    /// Provider reported a state we don't map.
    Unknown,
}

impl PrStatus {
    /// Stable lowercase slug used for storage + the wire (matches the serde form).
    pub fn as_str(&self) -> &'static str {
        match self {
            PrStatus::Active => "active",
            PrStatus::Completed => "completed",
            PrStatus::Abandoned => "abandoned",
            PrStatus::Unknown => "unknown",
        }
    }

    /// Parse the stored / provider slug back into the enum (unknown otherwise).
    pub fn from_slug(s: &str) -> Self {
        match s {
            "active" => PrStatus::Active,
            "completed" => PrStatus::Completed,
            "abandoned" => PrStatus::Abandoned,
            _ => PrStatus::Unknown,
        }
    }
}
