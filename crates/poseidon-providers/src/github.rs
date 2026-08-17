//! GitHub provider.
//!
//! Talks to the GitHub REST API (api.github.com). Public repos read without a
//! token; a token (Credential::Pat/Bearer with a non-empty value) raises rate
//! limits and reaches private repos. Maps Issues -> work items, Pull Requests ->
//! pull requests, Actions workflows/runs -> pipelines/runs. JSON -> core
//! normalisation lives in free functions at the bottom so it is testable against
//! captured fixtures (src/fixtures/github/) with no network.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poseidon_core::{
    EditableField, FieldChange, FieldKind, Pipeline, PipelineRun, PrStatus, PullRequest, RunStatus,
    TeamConfig, WorkItem, WorkItemUpdate,
};
use serde::Deserialize;

use crate::{Credential, Provider, ProviderError};

const PROVIDER: &str = "github";
const API_BASE: &str = "https://api.github.com";

/// GitHub caps a page at 100 items; we walk pages until a short page ends the
/// walk (mirrors Azure DevOps' `$skip` PR paging - no continuation token games).
const PAGE_SIZE: u32 = 100;

/// Upper bound on pages we walk for a single listing, so a pathological repo
/// (or a bad token that never short-pages) can't spin an unbounded fetch.
const MAX_PAGES: u32 = 20;

/// One GitHub repository POSEIDON polls. `owner`/`repo` come from the team's
/// `organization`/`project` config fields.
pub struct GithubProvider {
    client: reqwest::Client,
    /// The team's display name (stamped onto every entity as `team`).
    team: String,
    /// Repository owner (user or org) - first path segment in API URLs.
    owner: String,
    /// Repository name - second path segment in API URLs.
    repo: String,
}

/// The raw token string behind either credential kind. GitHub authenticates
/// both a PAT and an OAuth token the same way (an `Authorization: Bearer`
/// header), so unlike Azure DevOps we never Basic-encode - we just need the
/// bytes. An empty string means "anonymous" (public repos only).
fn credential_token(credential: &Credential) -> &str {
    match credential {
        Credential::Pat(token) | Credential::Bearer(token) => token,
    }
}

impl GithubProvider {
    pub fn new(cfg: &TeamConfig, credential: Credential) -> Result<Self, ProviderError> {
        // Default headers cover every request: the GitHub media type, and an
        // Authorization header ONLY when a non-empty token is configured (an
        // empty token = anonymous, which works for public repos and must not
        // send a malformed `Bearer ` header that GitHub rejects as 401).
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
        );
        let token = credential_token(&credential).trim();
        if !token.is_empty() {
            let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| ProviderError::Client(e.to_string()))?;
            auth.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, auth);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            // GitHub rejects requests without a User-Agent (403).
            .user_agent("poseidon")
            .build()
            .map_err(|e| ProviderError::Client(e.to_string()))?;

        Ok(Self {
            client,
            team: cfg.name.clone(),
            owner: cfg.organization.clone(),
            repo: cfg.project.clone(),
        })
    }

    /// `{API_BASE}/repos/{owner}/{repo}/{tail}` with an optional query tail.
    fn repo_url(&self, tail: &str, query: &str) -> String {
        let sep = if query.is_empty() { "" } else { "?" };
        format!(
            "{API_BASE}/repos/{}/{}/{tail}{sep}{query}",
            self.owner, self.repo
        )
    }

    /// GET + JSON-decode with a uniform error surface (non-2xx -> `Api`).
    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T, ProviderError> {
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                url: url.to_string(),
                body: body.chars().take(400).collect(),
            });
        }
        resp.json::<T>().await.map_err(ProviderError::Http)
    }

    /// Walk a paginated list endpoint by FOLLOWING GitHub's `Link: rel="next"`
    /// header rather than hand-incrementing `?page=N`. This is required for large
    /// repos: GitHub rejects deep `page` pagination with HTTP 422 and switches to
    /// cursor pagination (`after=`), which the next-link carries transparently -
    /// so following it is correct for both classic and cursor modes. `MAX_PAGES`
    /// bounds a pathological walk. `extra` carries endpoint params (e.g. `state=all`).
    async fn get_paged<T: for<'de> Deserialize<'de>>(
        &self,
        tail: &str,
        extra: &str,
    ) -> Result<Vec<T>, ProviderError> {
        let sep = if extra.is_empty() { "" } else { "&" };
        let mut next = Some(self.repo_url(tail, &format!("per_page={PAGE_SIZE}{sep}{extra}")));
        let mut out = Vec::new();
        let mut pages = 0;
        while let Some(url) = next {
            let resp = self.client.get(&url).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api {
                    status: status.as_u16(),
                    url,
                    body: body.chars().take(400).collect(),
                });
            }
            next = next_link(resp.headers());
            let batch: Vec<T> = resp.json().await.map_err(ProviderError::Http)?;
            out.extend(batch);
            pages += 1;
            if pages >= MAX_PAGES {
                break;
            }
        }
        Ok(out)
    }
}

/// Extract the `rel="next"` target from a GitHub `Link` header, if any. The
/// header looks like `<url>; rel="next", <url>; rel="last"` (classic) or carries
/// cursor URLs (`...&after=...`) for large datasets - both surface as `rel="next"`.
fn next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    for part in link.split(',') {
        let is_next = part.split(';').skip(1).any(|s| s.trim() == "rel=\"next\"");
        if is_next {
            if let Some(start) = part.find('<') {
                if let Some(len) = part[start + 1..].find('>') {
                    return Some(part[start + 1..start + 1 + len].to_string());
                }
            }
        }
    }
    None
}

#[async_trait]
impl Provider for GithubProvider {
    fn provider_name(&self) -> &str {
        PROVIDER
    }
    fn team_name(&self) -> &str {
        &self.team
    }

    async fn fetch_work_items(&self) -> Result<Vec<WorkItem>, ProviderError> {
        // `/issues?state=all` returns BOTH issues and pull requests - GitHub
        // models a PR as an issue. Any element carrying a `pull_request` key IS a
        // PR and must be dropped so it doesn't masquerade as a work item.
        let raw: Vec<GhIssue> = self.get_paged("issues", "state=all").await?;
        Ok(raw
            .into_iter()
            .filter(|i| !i.is_pull_request())
            .map(|i| normalise_issue(i, &self.team))
            .collect())
    }

    async fn fetch_pipelines(&self) -> Result<Vec<Pipeline>, ProviderError> {
        // Actions workflows are POSEIDON's pipelines. The workflows endpoint does
        // not fold in the latest run, so `last_run_*` stays None here (best-effort
        // - the runs feed carries recent status separately).
        let url = self.repo_url("actions/workflows", "per_page=100");
        let parsed: GhWorkflowsResponse = self.get_json(&url).await?;
        Ok(parsed
            .workflows
            .into_iter()
            .map(|w| normalise_workflow(w, &self.team))
            .collect())
    }

    async fn fetch_runs(&self, since: DateTime<Utc>) -> Result<Vec<PipelineRun>, ProviderError> {
        // Actions runs, newest first. GitHub supports a server-side `created`
        // filter, but we also filter client-side so the window is exact
        // regardless of the endpoint's date semantics.
        let url = self.repo_url("actions/runs", "per_page=100");
        let parsed: GhRunsResponse = self.get_json(&url).await?;
        Ok(parsed
            .workflow_runs
            .into_iter()
            .filter(|r| r.created_at.map(|c| c >= since).unwrap_or(true))
            .map(|r| normalise_run(r, &self.team))
            .collect())
    }

    async fn fetch_pull_requests(&self) -> Result<Vec<PullRequest>, ProviderError> {
        // The active (open) set - what the PR screen shows. Open PRs normalise to
        // `Active`; the status mapping is honest for closed/merged too, so this
        // stays correct even if the query later widens.
        let raw: Vec<GhPull> = self.get_paged("pulls", "state=open").await?;
        Ok(raw
            .into_iter()
            .map(|p| normalise_pull_request(p, &self.team))
            .collect())
    }

    async fn fetch_pull_request(&self, id: i64) -> Result<PullRequest, ProviderError> {
        let url = self.repo_url(&format!("pulls/{id}"), "");
        let raw: GhPull = self.get_json(&url).await?;
        Ok(normalise_pull_request(raw, &self.team))
    }

    async fn update_work_item(
        &self,
        id: i64,
        _update: &WorkItemUpdate,
    ) -> Result<WorkItem, ProviderError> {
        // GitHub write-back is out of scope: POSEIDON reads GitHub, it does not
        // edit issues here.
        Err(ProviderError::Config(format!(
            "github provider is read-only: update_work_item not supported (#{id})"
        )))
    }
    async fn link_pr(&self, _work_item_id: i64, _pr_id: i64) -> Result<WorkItem, ProviderError> {
        Err(ProviderError::Config(
            "github does not support explicit work-item<->PR links".into(),
        ))
    }
    async fn unlink_pr(&self, _work_item_id: i64, _pr_id: i64) -> Result<WorkItem, ProviderError> {
        Err(ProviderError::Config(
            "github does not support explicit work-item<->PR links".into(),
        ))
    }

    async fn editable_fields(&self, id: i64) -> Result<Vec<EditableField>, ProviderError> {
        // A GitHub issue is title + body (markdown, no conversion needed). Labels
        // (== tags) and state are edited via the row's own controls, not here.
        let issue: GhIssue = self
            .get_json(&self.repo_url(&format!("issues/{id}"), ""))
            .await?;
        Ok(vec![
            EditableField {
                reference: "title".into(),
                label: "Title".into(),
                kind: FieldKind::Text,
                value: issue.title.unwrap_or_default(),
                options: Vec::new(),
                read_only: false,
                required: true,
                help: None,
            },
            EditableField {
                reference: "body".into(),
                label: "Description".into(),
                kind: FieldKind::Markdown,
                value: issue.body.unwrap_or_default(),
                options: Vec::new(),
                read_only: false,
                required: false,
                help: None,
            },
        ])
    }

    async fn update_fields(
        &self,
        id: i64,
        changes: &[FieldChange],
    ) -> Result<WorkItem, ProviderError> {
        if changes.is_empty() {
            return Err(ProviderError::Config("no fields to update".into()));
        }
        // PATCH /issues/{id} with the changed title/body (body is markdown natively).
        let mut patch = serde_json::Map::new();
        for c in changes {
            match c.reference.as_str() {
                "title" | "body" => {
                    patch.insert(
                        c.reference.clone(),
                        serde_json::Value::String(c.value.clone()),
                    );
                }
                other => {
                    return Err(ProviderError::Config(format!(
                        "github: field '{other}' is not editable"
                    )))
                }
            }
        }
        let url = self.repo_url(&format!("issues/{id}"), "");
        let resp = self
            .client
            .patch(&url)
            .json(&serde_json::Value::Object(patch))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                url,
                body: body.chars().take(400).collect(),
            });
        }
        let raw: GhIssue = resp.json().await.map_err(ProviderError::Http)?;
        Ok(normalise_issue(raw, &self.team))
    }

    async fn mark_duplicate(&self, id: i64, duplicate_of: i64) -> Result<WorkItem, ProviderError> {
        // GitHub has no duplicate LINK type. The convention (and what GitHub's own
        // "mark as duplicate" does) is: add the `duplicate` label, leave a "Duplicate of
        // #N" reference comment, and close the issue as not-planned. The label + comment
        // are best-effort; the close is authoritative and returns the post-write item.
        let _ = self
            .client
            .post(self.repo_url(&format!("issues/{id}/labels"), ""))
            .json(&serde_json::json!({ "labels": ["duplicate"] }))
            .send()
            .await;
        let _ = self
            .client
            .post(self.repo_url(&format!("issues/{id}/comments"), ""))
            .json(&serde_json::json!({ "body": format!("Duplicate of #{duplicate_of}") }))
            .send()
            .await;
        let url = self.repo_url(&format!("issues/{id}"), "");
        let resp = self
            .client
            .patch(&url)
            .json(&serde_json::json!({ "state": "closed", "state_reason": "not_planned" }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                url,
                body: body.chars().take(400).collect(),
            });
        }
        let raw: GhIssue = resp.json().await.map_err(ProviderError::Http)?;
        Ok(normalise_issue(raw, &self.team))
    }
}

// ─────────────────────────── GitHub DTOs ───────────────────────────
// Only the fields POSEIDON consumes. `Option`/`default` everywhere the API might
// omit a value so a sparse payload degrades gracefully rather than failing.

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: i64,
    title: Option<String>,
    /// "open" | "closed".
    state: Option<String>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    assignee: Option<GhUser>,
    #[serde(default)]
    assignees: Vec<GhUser>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    closed_at: Option<DateTime<Utc>>,
    body: Option<String>,
    html_url: Option<String>,
    /// Present iff this "issue" is really a pull request (the `/issues` endpoint
    /// returns both). Its shape is irrelevant - only its presence matters.
    pull_request: Option<serde_json::Value>,
}

impl GhIssue {
    fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhPull {
    number: i64,
    title: Option<String>,
    /// "open" | "closed" (a merged PR is `closed` with `merged_at` set).
    state: Option<String>,
    #[serde(default)]
    draft: bool,
    user: Option<GhUser>,
    created_at: Option<DateTime<Utc>>,
    merged_at: Option<DateTime<Utc>>,
    html_url: Option<String>,
    head: Option<GhRef>,
    base: Option<GhRef>,
    #[serde(default)]
    requested_reviewers: Vec<serde_json::Value>,
}

/// A PR branch ref (`head`/`base`), carrying the short branch name and, for the
/// base, the repository it belongs to.
#[derive(Debug, Deserialize)]
struct GhRef {
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    repo: Option<GhRepo>,
}

#[derive(Debug, Deserialize)]
struct GhRepo {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhWorkflowsResponse {
    #[serde(default)]
    workflows: Vec<GhWorkflow>,
}

#[derive(Debug, Deserialize)]
struct GhWorkflow {
    id: i64,
    name: String,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhRunsResponse {
    #[serde(default)]
    workflow_runs: Vec<GhRun>,
}

#[derive(Debug, Deserialize)]
struct GhRun {
    id: i64,
    workflow_id: i64,
    /// "queued" | "in_progress" | "completed" | ...
    status: Option<String>,
    /// When completed: "success" | "failure" | "cancelled" | ... ; None while
    /// the run is still in flight.
    conclusion: Option<String>,
    run_started_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    created_at: Option<DateTime<Utc>>,
    head_branch: Option<String>,
    html_url: Option<String>,
}

// ─────────────────────────── Normalisation ───────────────────────────
// Pure functions: raw DTO -> core type. Unit-tested below.

/// Map a GitHub issue `state` ("open"/"closed") to POSEIDON's title-cased form.
/// "Closed" matches the rules engine's default resolved_states; anything else is
/// passed through as-is (title-cased) so an unexpected state stays visible.
fn map_issue_state(state: Option<&str>) -> String {
    match state.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("open") => "Open".to_string(),
        Some("closed") => "Closed".to_string(),
        Some(other) => title_case(other),
        None => String::new(),
    }
}

/// Upper-case the first character of an ascii word (for an unmapped state).
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn normalise_issue(raw: GhIssue, team_name: &str) -> WorkItem {
    // Prefer `updated_at` for the staleness anchor, fall back to created (and
    // vice versa) so a sparse payload never anchors on the Unix epoch.
    let changed = raw.updated_at.or(raw.created_at).unwrap_or_default();
    let created = raw.created_at.or(raw.updated_at).unwrap_or_default();
    // A single explicit assignee wins; otherwise fall back to the first of the
    // `assignees` list. Either way it's a login handle.
    let assigned_to = raw
        .assignee
        .map(|u| u.login)
        .or_else(|| raw.assignees.into_iter().next().map(|u| u.login));
    let url = raw
        .html_url
        .unwrap_or_else(|| format!("{API_BASE}/repos/_/issues/{}", raw.number));

    WorkItem {
        id: raw.number,
        provider: PROVIDER.to_string(),
        team: team_name.to_string(),
        title: raw.title.unwrap_or_default(),
        work_item_type: "Issue".to_string(),
        state: map_issue_state(raw.state.as_deref()),
        tags: raw.labels.into_iter().map(|l| l.name).collect(),
        assigned_to,
        created_at: created,
        changed_at: changed,
        closed_at: raw.closed_at,
        iteration_path: None,
        story_points: None,
        // GitHub bodies are markdown/plain text (not HTML like Azure DevOps), so
        // keep them verbatim; an empty/whitespace body maps to None.
        description: raw
            .body
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty()),
        url,
        linked_pr_ids: Vec::new(),
        parent_id: None,
        linked_repos: Vec::new(),
        linked_prs: Vec::new(),
        tag_suggestions: Vec::new(),
    }
}

/// Map a GitHub PR's merge/close state into POSEIDON's [`PrStatus`]. Merged wins
/// (merged PRs are `closed` too), then a plain close is `Abandoned`, else it's an
/// in-flight `Active` PR.
fn map_pr_status(merged: bool, state: Option<&str>) -> PrStatus {
    if merged {
        PrStatus::Completed
    } else if state
        .map(|s| s.eq_ignore_ascii_case("closed"))
        .unwrap_or(false)
    {
        PrStatus::Abandoned
    } else {
        PrStatus::Active
    }
}

fn normalise_pull_request(raw: GhPull, team_name: &str) -> PullRequest {
    let status = map_pr_status(raw.merged_at.is_some(), raw.state.as_deref());
    // The target repo (base) names the PR's repository; GitHub branch refs are
    // already short (no `refs/heads/` prefix to strip).
    let repository = raw
        .base
        .as_ref()
        .and_then(|b| b.repo.as_ref())
        .and_then(|r| r.name.clone());
    let source_branch = raw.head.and_then(|h| h.ref_name);
    let target_branch = raw.base.and_then(|b| b.ref_name);
    let url = raw
        .html_url
        .unwrap_or_else(|| format!("{API_BASE}/pull/{}", raw.number));

    PullRequest {
        id: raw.number,
        provider: PROVIDER.to_string(),
        team: team_name.to_string(),
        title: raw.title.unwrap_or_default(),
        status,
        is_draft: raw.draft,
        repository,
        author: raw.user.map(|u| u.login),
        created_at: raw.created_at,
        source_branch,
        target_branch,
        reviewer_count: raw.requested_reviewers.len() as i64,
        url,
        flags: Vec::new(),
        linked_work_items: Vec::new(),
    }
}

fn normalise_workflow(raw: GhWorkflow, team_name: &str) -> Pipeline {
    Pipeline {
        id: raw.id,
        provider: PROVIDER.to_string(),
        team: team_name.to_string(),
        name: raw.name,
        // GitHub Actions has no folder grouping for workflows.
        folder: None,
        url: raw.html_url.unwrap_or_default(),
        // The workflows endpoint carries no last-run info; left None (best-effort).
        last_run_status: None,
        last_run_at: None,
        last_run_url: None,
    }
}

/// Map an Actions run's `status` + `conclusion` into POSEIDON's [`RunStatus`].
/// A run that has not `completed` is `Running`; a completed run buckets by its
/// conclusion, and an unmapped conclusion (skipped / neutral / action_required /
/// stale) is surfaced honestly as `Unknown` rather than faked as pass/fail.
fn map_run_status(status: Option<&str>, conclusion: Option<&str>) -> RunStatus {
    match status.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("completed") => match conclusion.map(|c| c.to_ascii_lowercase()).as_deref() {
            Some("success") => RunStatus::Succeeded,
            Some("failure") | Some("timed_out") | Some("startup_failure") => RunStatus::Failed,
            Some("cancelled") | Some("canceled") => RunStatus::Canceled,
            _ => RunStatus::Unknown,
        },
        // queued / in_progress / waiting / pending / requested -> still running.
        Some(_) => RunStatus::Running,
        None => RunStatus::Unknown,
    }
}

fn normalise_run(raw: GhRun, team_name: &str) -> PipelineRun {
    let status = map_run_status(raw.status.as_deref(), raw.conclusion.as_deref());
    let started_at = raw.run_started_at.or(raw.created_at);
    // A finished timestamp only makes sense once the run is terminal; while it's
    // running we have no real finish time, so leave it None.
    let finished_at = if status == RunStatus::Running {
        None
    } else {
        raw.updated_at
    };
    PipelineRun {
        id: raw.id,
        pipeline_id: raw.workflow_id,
        provider: PROVIDER.to_string(),
        team: team_name.to_string(),
        status,
        started_at,
        finished_at,
        source_branch: raw.head_branch,
        url: raw.html_url.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit tests against hand-written payloads ────────────────────────

    #[test]
    fn map_issue_state_maps_open_and_closed() {
        assert_eq!(map_issue_state(Some("open")), "Open");
        assert_eq!(map_issue_state(Some("closed")), "Closed");
        // Case-insensitive on the wire value.
        assert_eq!(map_issue_state(Some("OPEN")), "Open");
        // An unexpected state stays visible (title-cased), never silently dropped.
        assert_eq!(map_issue_state(Some("triaged")), "Triaged");
        assert_eq!(map_issue_state(None), "");
    }

    #[test]
    fn map_pr_status_prefers_merged_then_closed_then_active() {
        // Merged wins even though the PR is also `closed`.
        assert_eq!(map_pr_status(true, Some("closed")), PrStatus::Completed);
        // A plain close (not merged) is abandoned.
        assert_eq!(map_pr_status(false, Some("closed")), PrStatus::Abandoned);
        // Open and un-merged is active.
        assert_eq!(map_pr_status(false, Some("open")), PrStatus::Active);
        assert_eq!(map_pr_status(false, None), PrStatus::Active);
    }

    #[test]
    fn map_run_status_covers_the_matrix() {
        assert_eq!(
            map_run_status(Some("completed"), Some("success")),
            RunStatus::Succeeded
        );
        assert_eq!(
            map_run_status(Some("completed"), Some("failure")),
            RunStatus::Failed
        );
        assert_eq!(
            map_run_status(Some("completed"), Some("timed_out")),
            RunStatus::Failed
        );
        assert_eq!(
            map_run_status(Some("completed"), Some("cancelled")),
            RunStatus::Canceled
        );
        // In-flight runs (no conclusion yet) are Running.
        assert_eq!(
            map_run_status(Some("in_progress"), None),
            RunStatus::Running
        );
        assert_eq!(map_run_status(Some("queued"), None), RunStatus::Running);
        // A completed run with an unmapped conclusion is honestly Unknown.
        assert_eq!(
            map_run_status(Some("completed"), Some("action_required")),
            RunStatus::Unknown
        );
        assert_eq!(
            map_run_status(Some("completed"), Some("skipped")),
            RunStatus::Unknown
        );
        assert_eq!(map_run_status(None, None), RunStatus::Unknown);
    }

    #[test]
    fn normalise_issue_from_sample_payload() {
        let json = r#"{
            "number": 4321,
            "title": "Wire up ingress",
            "state": "open",
            "labels": [ { "name": "enhancement" }, { "name": "area:platform" } ],
            "assignee": { "login": "ada" },
            "assignees": [ { "login": "ada" }, { "login": "grace" } ],
            "created_at": "2026-07-01T09:00:00Z",
            "updated_at": "2026-07-20T15:30:00Z",
            "closed_at": null,
            "body": "  A description with padding  ",
            "html_url": "https://github.com/acme/widgets/issues/4321"
        }"#;
        let raw: GhIssue = serde_json::from_str(json).unwrap();
        assert!(!raw.is_pull_request());
        let wi = normalise_issue(raw, "Platform Engineering");
        assert_eq!(wi.id, 4321);
        assert_eq!(wi.provider, "github");
        assert_eq!(wi.team, "Platform Engineering");
        assert_eq!(wi.title, "Wire up ingress");
        assert_eq!(wi.work_item_type, "Issue");
        assert_eq!(wi.state, "Open");
        assert_eq!(wi.tags, vec!["enhancement", "area:platform"]);
        // Explicit single assignee wins.
        assert_eq!(wi.assigned_to.as_deref(), Some("ada"));
        assert!(wi.story_points.is_none());
        assert!(wi.iteration_path.is_none());
        // Body is trimmed.
        assert_eq!(
            wi.description.as_deref(),
            Some("A description with padding")
        );
        assert_eq!(wi.url, "https://github.com/acme/widgets/issues/4321");
    }

    #[test]
    fn normalise_issue_falls_back_to_first_of_assignees() {
        // No single `assignee`, but the `assignees` array is populated.
        let json = r#"{
            "number": 5,
            "title": "Multi",
            "state": "closed",
            "assignee": null,
            "assignees": [ { "login": "grace" }, { "login": "ada" } ],
            "closed_at": "2026-07-02T00:00:00Z"
        }"#;
        let raw: GhIssue = serde_json::from_str(json).unwrap();
        let wi = normalise_issue(raw, "Platform");
        assert_eq!(wi.assigned_to.as_deref(), Some("grace"));
        assert_eq!(wi.state, "Closed");
        assert!(wi.closed_at.is_some());
    }

    #[test]
    fn normalise_issue_tolerates_sparse_payload() {
        // A bare issue must not panic - defaults + a constructed URL.
        let raw: GhIssue = serde_json::from_str(r#"{ "number": 9 }"#).unwrap();
        let wi = normalise_issue(raw, "Data");
        assert_eq!(wi.id, 9);
        assert!(wi.title.is_empty());
        assert!(wi.tags.is_empty());
        assert!(wi.assigned_to.is_none());
        assert!(wi.description.is_none());
        assert!(wi.url.contains("/issues/9"));
    }

    #[test]
    fn normalise_issue_maps_whitespace_body_to_none() {
        let raw: GhIssue = serde_json::from_str(r#"{ "number": 3, "body": "   \n  " }"#).unwrap();
        let wi = normalise_issue(raw, "Platform");
        assert!(wi.description.is_none());
    }

    #[test]
    fn normalise_pull_request_from_sample() {
        let json = r#"{
            "number": 4321,
            "title": "Add retry to poller",
            "state": "open",
            "draft": true,
            "user": { "login": "ada" },
            "created_at": "2026-07-28T09:00:00Z",
            "merged_at": null,
            "html_url": "https://github.com/acme/widgets/pull/4321",
            "head": { "ref": "feature/retry", "repo": { "name": "widgets-fork" } },
            "base": { "ref": "main", "repo": { "name": "widgets" } },
            "requested_reviewers": [ { "login": "grace" }, { "login": "linus" } ]
        }"#;
        let raw: GhPull = serde_json::from_str(json).unwrap();
        let pr = normalise_pull_request(raw, "Platform");
        assert_eq!(pr.id, 4321);
        assert_eq!(pr.provider, "github");
        assert_eq!(pr.title, "Add retry to poller");
        assert_eq!(pr.status, PrStatus::Active);
        assert!(pr.is_draft);
        assert_eq!(pr.author.as_deref(), Some("ada"));
        // Repository is the base (target) repo.
        assert_eq!(pr.repository.as_deref(), Some("widgets"));
        assert_eq!(pr.source_branch.as_deref(), Some("feature/retry"));
        assert_eq!(pr.target_branch.as_deref(), Some("main"));
        assert_eq!(pr.reviewer_count, 2);
        assert_eq!(pr.url, "https://github.com/acme/widgets/pull/4321");
    }

    #[test]
    fn normalise_pull_request_detects_merged_and_abandoned() {
        let merged: GhPull = serde_json::from_str(
            r#"{ "number": 1, "state": "closed", "merged_at": "2026-07-01T00:00:00Z" }"#,
        )
        .unwrap();
        assert_eq!(
            normalise_pull_request(merged, "T").status,
            PrStatus::Completed
        );
        let abandoned: GhPull =
            serde_json::from_str(r#"{ "number": 2, "state": "closed", "merged_at": null }"#)
                .unwrap();
        assert_eq!(
            normalise_pull_request(abandoned, "T").status,
            PrStatus::Abandoned
        );
    }

    #[test]
    fn normalise_workflow_from_sample() {
        let json = r#"{ "id": 161335, "name": "CI", "path": ".github/workflows/ci.yml",
            "state": "active", "html_url": "https://github.com/acme/widgets/blob/main/.github/workflows/ci.yml" }"#;
        let raw: GhWorkflow = serde_json::from_str(json).unwrap();
        let p = normalise_workflow(raw, "Platform");
        assert_eq!(p.id, 161335);
        assert_eq!(p.name, "CI");
        assert_eq!(p.provider, "github");
        assert!(p.folder.is_none());
        assert!(p.last_run_status.is_none());
        assert!(p.url.contains("ci.yml"));
    }

    #[test]
    fn normalise_run_from_sample() {
        let json = r#"{
            "id": 99001,
            "workflow_id": 161335,
            "status": "completed",
            "conclusion": "failure",
            "run_started_at": "2026-07-29T10:00:00Z",
            "updated_at": "2026-07-29T10:12:00Z",
            "created_at": "2026-07-29T09:59:00Z",
            "head_branch": "main",
            "html_url": "https://github.com/acme/widgets/actions/runs/99001"
        }"#;
        let raw: GhRun = serde_json::from_str(json).unwrap();
        let run = normalise_run(raw, "Platform");
        assert_eq!(run.id, 99001);
        assert_eq!(run.pipeline_id, 161335);
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.source_branch.as_deref(), Some("main"));
        assert!(run.finished_at.is_some());
        assert!(run.url.contains("runs/99001"));
    }

    #[test]
    fn normalise_run_running_has_no_finish_time() {
        let json = r#"{
            "id": 5, "workflow_id": 1, "status": "in_progress", "conclusion": null,
            "run_started_at": "2026-07-29T10:00:00Z", "updated_at": "2026-07-29T10:05:00Z"
        }"#;
        let raw: GhRun = serde_json::from_str(json).unwrap();
        let run = normalise_run(raw, "Platform");
        assert_eq!(run.status, RunStatus::Running);
        // A run still in flight has no real finish time.
        assert!(run.finished_at.is_none());
        assert!(run.started_at.is_some());
    }

    #[test]
    fn empty_credential_sends_no_auth_header_and_builds() {
        // An empty token = anonymous. The provider must still build (and, by
        // construction, omit the Authorization header). Both credential kinds.
        assert_eq!(credential_token(&Credential::Pat(String::new())), "");
        assert_eq!(
            credential_token(&Credential::Bearer("  ".into())).trim(),
            ""
        );
        let cfg = sample_cfg();
        assert!(GithubProvider::new(&cfg, Credential::Pat(String::new())).is_ok());
        assert!(GithubProvider::new(&cfg, Credential::Bearer("tok".into())).is_ok());
    }

    fn sample_cfg() -> TeamConfig {
        TeamConfig {
            name: "OSS".into(),
            provider: poseidon_core::ProviderKind::GitHub,
            organization: "cli".into(),
            project: "cli".into(),
            area_path: None,
            area_path_strict: false,
            tenant: None,
            auth: Default::default(),
            wiql: None,
            pipeline_ids: vec![],
            rules: None,
        }
    }

    // ── Fixture tests ───────────────────────────────────────────────────
    // Real api.github.com responses captured from the cli/cli repo. They pin our
    // deserialisation + normalisation against the REAL response shape - notably
    // that the /issues endpoint interleaves pull requests (a `pull_request` key)
    // which MUST be filtered out of work items.

    #[test]
    fn issues_fixture_filters_out_pull_requests() {
        let json = include_str!("fixtures/github/issues.json");
        let raw: Vec<GhIssue> = serde_json::from_str(json).expect("real issues payload");
        // The captured page interleaves PRs and real issues.
        let total = raw.len();
        let pr_count = raw.iter().filter(|i| i.is_pull_request()).count();
        assert!(pr_count > 0, "fixture should contain PR-as-issue elements");
        let items: Vec<WorkItem> = raw
            .into_iter()
            .filter(|i| !i.is_pull_request())
            .map(|i| normalise_issue(i, "OSS"))
            .collect();
        // Every element that was a PR is gone; the rest are real work items.
        assert_eq!(items.len(), total - pr_count);
        assert!(!items.is_empty(), "fixture should contain real issues too");
        for wi in &items {
            assert_eq!(wi.provider, "github");
            assert_eq!(wi.work_item_type, "Issue");
            assert!(wi.id > 0);
            assert!(wi.url.starts_with("https://github.com/"));
            // Never a PR URL - PRs were filtered before normalisation.
            assert!(!wi.url.contains("/pull/"), "leaked PR url: {}", wi.url);
            // State is one of the mapped forms.
            assert!(
                wi.state == "Open" || wi.state == "Closed",
                "state {}",
                wi.state
            );
        }
        // At least one closed issue exercises the closed-state mapping + closed_at.
        assert!(
            items
                .iter()
                .any(|w| w.state == "Closed" && w.closed_at.is_some()),
            "expected a closed issue with a closed_at"
        );
        // Labels normalise into tags for at least one item.
        assert!(
            items.iter().any(|w| !w.tags.is_empty()),
            "expected labels to become tags"
        );
    }

    #[test]
    fn pulls_fixture_deserialises_and_normalises() {
        let json = include_str!("fixtures/github/pulls.json");
        let raw: Vec<GhPull> = serde_json::from_str(json).expect("real pulls payload");
        assert!(!raw.is_empty());
        for p in raw {
            let pr = normalise_pull_request(p, "OSS");
            assert!(pr.id > 0);
            assert_eq!(pr.provider, "github");
            assert!(pr.url.starts_with("https://github.com/"));
            assert!(pr.url.contains("/pull/"));
            assert!(pr.author.is_some(), "a PR should carry an author");
        }
    }

    #[test]
    fn pull_by_id_fixture_is_completed_when_merged() {
        let json = include_str!("fixtures/github/pull_by_id.json");
        let raw: GhPull = serde_json::from_str(json).expect("real single-PR payload");
        let pr = normalise_pull_request(raw, "OSS");
        // The captured PR is merged (closed + merged_at set) -> Completed.
        assert_eq!(pr.status, PrStatus::Completed);
        assert!(pr.id > 0);
        assert!(pr.target_branch.is_some());
    }

    #[test]
    fn workflows_fixture_deserialises_and_normalises() {
        let json = include_str!("fixtures/github/workflows.json");
        let parsed: GhWorkflowsResponse =
            serde_json::from_str(json).expect("real workflows payload");
        assert!(!parsed.workflows.is_empty());
        for w in parsed.workflows {
            let p = normalise_workflow(w, "OSS");
            assert!(p.id > 0);
            assert_eq!(p.provider, "github");
            assert!(!p.name.is_empty());
        }
    }

    #[test]
    fn runs_fixture_deserialises_normalises_and_windows() {
        let json = include_str!("fixtures/github/runs.json");
        let parsed: GhRunsResponse = serde_json::from_str(json).expect("real runs payload");
        assert!(!parsed.workflow_runs.is_empty());
        for r in &parsed.workflow_runs {
            assert!(r.workflow_id > 0, "a run should reference its workflow");
        }
        let runs: Vec<PipelineRun> = parsed
            .workflow_runs
            .into_iter()
            .map(|r| normalise_run(r, "OSS"))
            .collect();
        for run in &runs {
            assert!(run.id > 0);
            assert!(run.pipeline_id > 0);
            assert_eq!(run.provider, "github");
        }
        // The captured runs are all completed successes/action_required, so the
        // status must be a terminal value (never Running for these).
        assert!(runs.iter().all(|r| r.status.is_terminal()));
    }

    // ── Live smoke test (manual only) ───────────────────────────────────
    // Hits api.github.com anonymously. Excluded from CI (`#[ignore]`); run with
    // `cargo test -p poseidon-providers -- --ignored github_live`.

    #[tokio::test]
    #[ignore = "hits the live GitHub API; run manually"]
    async fn github_live_anonymous_fetch_is_non_empty() {
        let cfg = sample_cfg(); // cli/cli - a large public repo
        let provider =
            GithubProvider::new(&cfg, Credential::Pat(String::new())).expect("build provider");
        let items = provider
            .fetch_work_items()
            .await
            .expect("anonymous work-item fetch");
        assert!(!items.is_empty(), "cli/cli should have open+closed issues");
        let pulls = provider
            .fetch_pull_requests()
            .await
            .expect("anonymous PR fetch");
        // A busy repo virtually always has open PRs; assert the call succeeds and
        // returns a well-formed (possibly empty) set.
        assert!(pulls.iter().all(|p| p.id > 0));
    }
}
