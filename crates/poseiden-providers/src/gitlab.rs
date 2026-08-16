//! GitLab provider.
//!
//! Talks to the GitLab REST API v4 (gitlab.com, or a self-hosted base URL).
//! Public projects read without a token; a token raises limits and reaches
//! private projects. Maps Issues -> work items, Merge Requests -> pull requests,
//! Pipelines -> pipelines/runs. JSON -> core normalisation lives in free
//! functions at the bottom so it is testable against captured fixtures
//! (src/fixtures/gitlab/) with no network.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use poseiden_core::{
    EditableField, FieldChange, FieldKind, Pipeline, PipelineRun, PrStatus, PullRequest, RunStatus,
    TeamConfig, WorkItem, WorkItemUpdate,
};
use serde::Deserialize;

use crate::{Credential, Provider, ProviderError};

const PROVIDER: &str = "gitlab";
const API_BASE: &str = "https://gitlab.com";

/// GitLab addresses a project by its url-encoded path (`namespace/project`),
/// which means the `/` separator itself must be percent-encoded to `%2F` in the
/// URL - `gitlab-org/gitlab-runner` -> `gitlab-org%2Fgitlab-runner`. This set
/// encodes the reserved characters a project (or sub-group) path can contain,
/// slash included; unreserved chars pass through.
const PROJECT_PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'/')
    .add(b'%')
    .add(b'#')
    .add(b'?')
    .add(b'&')
    .add(b'+');

/// Percent-encode a project path so its `/` separators become `%2F`.
fn enc_project(path: &str) -> String {
    utf8_percent_encode(path, PROJECT_PATH).to_string()
}

/// One GitLab project POSEIDEN polls. The project path (`namespace/project`)
/// comes from the team's `organization`/`project` config fields; a self-hosted
/// base URL can live in `organization` when it starts with http.
pub struct GitlabProvider {
    team: String,
    project_path: String,
    base_url: String,
    client: reqwest::Client,
}

impl GitlabProvider {
    pub fn new(cfg: &TeamConfig, credential: Credential) -> Result<Self, ProviderError> {
        // GitLab's token scheme is the `PRIVATE-TOKEN` header (NOT Authorization),
        // and it accepts a Personal/Project Access Token or an OAuth token there.
        // An EMPTY token means "anonymous" - send no auth header at all, which is
        // exactly what public projects want (a blank header would 401). So the
        // default headers carry PRIVATE-TOKEN only when a real token is present.
        let mut builder = reqwest::Client::builder().user_agent("poseiden");
        let token = credential_token(&credential);
        if !token.is_empty() {
            let mut headers = reqwest::header::HeaderMap::new();
            let mut value = reqwest::header::HeaderValue::from_str(token)
                .map_err(|e| ProviderError::Client(e.to_string()))?;
            value.set_sensitive(true);
            headers.insert("PRIVATE-TOKEN", value);
            builder = builder.default_headers(headers);
        }
        let client = builder
            .build()
            .map_err(|e| ProviderError::Client(e.to_string()))?;

        // `organization` = namespace (or a self-hosted base URL); `project` =
        // project name. When `organization` is a base URL, the project path is
        // just the project field (already `namespace/project`); otherwise the
        // path is `{organization}/{project}` against gitlab.com.
        let (base_url, project_path) = if cfg.organization.starts_with("http") {
            (
                cfg.organization.trim_end_matches('/').to_string(),
                cfg.project.clone(),
            )
        } else {
            (
                API_BASE.to_string(),
                format!("{}/{}", cfg.organization, cfg.project),
            )
        };
        Ok(Self {
            team: cfg.name.clone(),
            project_path,
            base_url,
            client,
        })
    }

    /// `{base}/api/v4/projects/{enc path}/{tail}` with an optional query string.
    fn project_url(&self, tail: &str, query: &str) -> String {
        let sep = if query.is_empty() { "" } else { "?" };
        format!(
            "{}/api/v4/projects/{}/{}{}{}",
            self.base_url,
            enc_project(&self.project_path),
            tail,
            sep,
            query
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
}

#[async_trait]
impl Provider for GitlabProvider {
    fn provider_name(&self) -> &str {
        PROVIDER
    }
    fn team_name(&self) -> &str {
        &self.team
    }

    async fn fetch_work_items(&self) -> Result<Vec<WorkItem>, ProviderError> {
        // Issues, all states, paged (GitLab caps a page at 100 and has no total
        // in the body, so walk until a short page ends the run).
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let query = format!("scope=all&per_page=100&page={page}");
            let url = self.project_url("issues", &query);
            let raw: Vec<GlIssue> = self.get_json(&url).await?;
            let full = raw.len() == 100;
            out.extend(raw.into_iter().map(|i| normalise_issue(i, &self.team)));
            if !full {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    async fn fetch_pipelines(&self) -> Result<Vec<Pipeline>, ProviderError> {
        // GitLab has no "pipeline definition" like Azure DevOps - each entry in
        // /pipelines is one execution. We surface each as a Pipeline (its own last
        // status folded in) so the pipelines screen lists recent activity.
        let url = self.project_url("pipelines", "per_page=100");
        let raw: Vec<GlPipeline> = self.get_json(&url).await?;
        Ok(raw
            .into_iter()
            .map(|p| normalise_pipeline(p, &self.team))
            .collect())
    }

    async fn fetch_runs(&self, since: DateTime<Utc>) -> Result<Vec<PipelineRun>, ProviderError> {
        let url = self.project_url("pipelines", "per_page=100");
        let raw: Vec<GlPipeline> = self.get_json(&url).await?;
        Ok(raw
            .into_iter()
            .map(|p| normalise_run(p, &self.team))
            // Window by creation time so the poll cost tracks recent activity.
            .filter(|r| r.started_at.map(|t| t >= since).unwrap_or(true))
            .collect())
    }

    async fn fetch_pull_requests(&self) -> Result<Vec<PullRequest>, ProviderError> {
        // Active (open) MRs = the in-flight set the PR screen shows.
        let url = self.project_url("merge_requests", "state=opened&per_page=100");
        let raw: Vec<GlMergeRequest> = self.get_json(&url).await?;
        Ok(raw
            .into_iter()
            .map(|m| normalise_merge_request(m, &self.team))
            .collect())
    }

    async fn fetch_pull_request(&self, id: i64) -> Result<PullRequest, ProviderError> {
        let url = self.project_url(&format!("merge_requests/{id}"), "");
        let raw: GlMergeRequest = self.get_json(&url).await?;
        Ok(normalise_merge_request(raw, &self.team))
    }

    async fn update_work_item(
        &self,
        id: i64,
        _update: &WorkItemUpdate,
    ) -> Result<WorkItem, ProviderError> {
        Err(ProviderError::Config(format!(
            "gitlab update_work_item not yet implemented (#{id})"
        )))
    }
    async fn link_pr(&self, _work_item_id: i64, _pr_id: i64) -> Result<WorkItem, ProviderError> {
        Err(ProviderError::Config(
            "gitlab does not support explicit work-item<->MR links".into(),
        ))
    }
    async fn unlink_pr(&self, _work_item_id: i64, _pr_id: i64) -> Result<WorkItem, ProviderError> {
        Err(ProviderError::Config(
            "gitlab does not support explicit work-item<->MR links".into(),
        ))
    }

    async fn editable_fields(&self, id: i64) -> Result<Vec<EditableField>, ProviderError> {
        // A GitLab issue is title + description (markdown). Labels/state have their
        // own row controls. `id` is the issue iid (what WorkItem.id carries).
        let issue: GlIssue = self
            .get_json(&self.project_url(&format!("issues/{id}"), ""))
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
                reference: "description".into(),
                label: "Description".into(),
                kind: FieldKind::Markdown,
                value: issue.description.unwrap_or_default(),
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
        // PUT /issues/{iid} with the changed title/description (markdown natively).
        let mut patch = serde_json::Map::new();
        for c in changes {
            match c.reference.as_str() {
                "title" | "description" => {
                    patch.insert(
                        c.reference.clone(),
                        serde_json::Value::String(c.value.clone()),
                    );
                }
                other => {
                    return Err(ProviderError::Config(format!(
                        "gitlab: field '{other}' is not editable"
                    )))
                }
            }
        }
        let url = self.project_url(&format!("issues/{id}"), "");
        let resp = self
            .client
            .put(&url)
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
        let raw: GlIssue = resp.json().await.map_err(ProviderError::Http)?;
        Ok(normalise_issue(raw, &self.team))
    }
}

/// The token string behind either credential kind. GitLab puts it in the
/// `PRIVATE-TOKEN` header verbatim (PAT, project token, or OAuth token all work),
/// so both variants collapse to the same string; an empty one means anonymous.
fn credential_token(credential: &Credential) -> &str {
    match credential {
        Credential::Pat(s) | Credential::Bearer(s) => s.as_str(),
    }
}

// ─────────────────────────── GitLab DTOs ───────────────────────────
// Only the fields POSEIDEN consumes. `Option` everywhere the API might omit a
// value so a sparse payload degrades gracefully rather than failing the poll.

#[derive(Debug, Deserialize)]
struct GlUser {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GlIssue {
    /// Project-relative id - matches the web URL and what a human quotes.
    iid: i64,
    title: Option<String>,
    description: Option<String>,
    state: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    assignee: Option<GlUser>,
    #[serde(default)]
    assignees: Vec<GlUser>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    closed_at: Option<DateTime<Utc>>,
    web_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GlMergeRequest {
    iid: i64,
    title: Option<String>,
    state: Option<String>,
    draft: Option<bool>,
    author: Option<GlUser>,
    created_at: Option<DateTime<Utc>>,
    source_branch: Option<String>,
    target_branch: Option<String>,
    #[serde(default)]
    reviewers: Vec<GlUser>,
    web_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GlPipeline {
    id: i64,
    /// The project the pipeline ran in - used as the run's grouping id, since
    /// GitLab has no separate pipeline-definition id.
    project_id: Option<i64>,
    status: Option<String>,
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    /// Present on a single-pipeline fetch; absent on the list endpoint (where we
    /// fall back to created_at/updated_at).
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    web_url: Option<String>,
    /// Optional pipeline name (usually null on the runner project).
    name: Option<String>,
}

// ─────────────────────────── Normalisation ───────────────────────────
// Pure functions: raw DTO -> core type. Unit-tested below.

/// Map a GitLab issue state to POSEIDEN's state string. "opened" -> "Open",
/// "closed" -> "Closed" (the rules engine's default resolved_states includes
/// "Closed"); anything else passes through title-cased-ish, honest rather than
/// silently bucketed.
fn map_issue_state(state: Option<&str>) -> String {
    match state.unwrap_or("") {
        "opened" => "Open".to_string(),
        "closed" => "Closed".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => String::new(),
    }
}

fn normalise_issue(raw: GlIssue, team: &str) -> WorkItem {
    // Prefer the single `assignee`, else the first of `assignees`.
    let assigned_to = raw
        .assignee
        .and_then(|a| a.name)
        .or_else(|| raw.assignees.into_iter().find_map(|a| a.name));
    // Timestamps: fall back each way so a sparse payload never anchors staleness
    // on the Unix epoch.
    let created = raw.created_at.or(raw.updated_at).unwrap_or_default();
    let changed = raw.updated_at.or(raw.created_at).unwrap_or_default();
    WorkItem {
        id: raw.iid,
        provider: PROVIDER.to_string(),
        team: team.to_string(),
        title: raw.title.unwrap_or_default(),
        work_item_type: "Issue".to_string(),
        state: map_issue_state(raw.state.as_deref()),
        tags: raw.labels,
        assigned_to,
        created_at: created,
        changed_at: changed,
        closed_at: raw.closed_at,
        iteration_path: None,
        story_points: None,
        description: raw.description.filter(|s| !s.is_empty()),
        url: raw.web_url.unwrap_or_default(),
        linked_pr_ids: Vec::new(),
        parent_id: None,
        linked_repos: Vec::new(),
        linked_prs: Vec::new(),
        tag_suggestions: Vec::new(),
    }
}

/// Map a GitLab merge-request state to POSEIDEN's [`PrStatus`]: "opened" (or the
/// rare "locked") is Active, "merged" is Completed, "closed" is Abandoned.
fn map_mr_status(state: Option<&str>) -> PrStatus {
    match state.unwrap_or("") {
        "opened" | "locked" => PrStatus::Active,
        "merged" => PrStatus::Completed,
        "closed" => PrStatus::Abandoned,
        _ => PrStatus::Unknown,
    }
}

fn normalise_merge_request(raw: GlMergeRequest, team: &str) -> PullRequest {
    PullRequest {
        id: raw.iid,
        provider: PROVIDER.to_string(),
        team: team.to_string(),
        title: raw.title.unwrap_or_default(),
        status: map_mr_status(raw.state.as_deref()),
        is_draft: raw.draft.unwrap_or(false),
        repository: None,
        author: raw.author.and_then(|a| a.name),
        created_at: raw.created_at,
        source_branch: raw.source_branch,
        target_branch: raw.target_branch,
        reviewer_count: raw.reviewers.len() as i64,
        url: raw.web_url.unwrap_or_default(),
        flags: Vec::new(),
        linked_work_items: Vec::new(),
    }
}

/// Map a GitLab pipeline status into POSEIDEN's [`RunStatus`]. GitLab uses a
/// single status field (unlike Azure DevOps' split status/result), so the map is
/// direct; unmapped states (skipped, manual, ...) surface as Unknown rather than
/// being silently treated as success or failure.
fn map_pipeline_status(status: Option<&str>) -> RunStatus {
    match status.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("success") => RunStatus::Succeeded,
        Some("failed") => RunStatus::Failed,
        Some("canceled") | Some("cancelled") => RunStatus::Canceled,
        Some("running")
        | Some("pending")
        | Some("created")
        | Some("preparing")
        | Some("waiting_for_resource")
        | Some("scheduled") => RunStatus::Running,
        _ => RunStatus::Unknown,
    }
}

fn normalise_pipeline(raw: GlPipeline, team: &str) -> Pipeline {
    let status = map_pipeline_status(raw.status.as_deref());
    // Name the pipeline by its explicit name, else by the ref it ran against.
    let name = raw
        .name
        .filter(|s| !s.is_empty())
        .or_else(|| raw.ref_name.clone())
        .unwrap_or_default();
    Pipeline {
        id: raw.id,
        provider: PROVIDER.to_string(),
        team: team.to_string(),
        name,
        folder: None,
        url: raw.web_url.clone().unwrap_or_default(),
        last_run_status: Some(status),
        // The list endpoint has no finished time for a still-running pipeline, so
        // fall back to updated_at as the best "last activity" anchor.
        last_run_at: raw.finished_at.or(raw.updated_at),
        last_run_url: raw.web_url,
    }
}

fn normalise_run(raw: GlPipeline, team: &str) -> PipelineRun {
    let status = map_pipeline_status(raw.status.as_deref());
    // A terminal run's finish is finished_at (detail) or updated_at (list); a
    // still-running one has no finish time yet.
    let finished_at = if status.is_terminal() {
        raw.finished_at.or(raw.updated_at)
    } else {
        raw.finished_at
    };
    PipelineRun {
        id: raw.id,
        // No definition id in GitLab - group runs by their project.
        pipeline_id: raw.project_id.unwrap_or(0),
        provider: PROVIDER.to_string(),
        team: team.to_string(),
        status,
        started_at: raw.started_at.or(raw.created_at),
        finished_at,
        source_branch: raw.ref_name,
        url: raw.web_url.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enc_project_encodes_the_path_separator() {
        assert_eq!(
            enc_project("gitlab-org/gitlab-runner"),
            "gitlab-org%2Fgitlab-runner"
        );
        // Sub-groups keep every slash encoded; unreserved chars pass through.
        assert_eq!(enc_project("group/sub/proj_1.0"), "group%2Fsub%2Fproj_1.0");
    }

    #[test]
    fn credential_token_unwraps_either_kind_and_detects_empty() {
        assert_eq!(
            credential_token(&Credential::Pat("glpat-x".into())),
            "glpat-x"
        );
        assert_eq!(
            credential_token(&Credential::Bearer("oauth".into())),
            "oauth"
        );
        assert!(credential_token(&Credential::Pat(String::new())).is_empty());
    }

    #[test]
    fn map_issue_state_maps_open_and_closed() {
        assert_eq!(map_issue_state(Some("opened")), "Open");
        assert_eq!(map_issue_state(Some("closed")), "Closed");
        // An unmapped state is surfaced verbatim, not silently blanked.
        assert_eq!(map_issue_state(Some("reopened")), "reopened");
        assert_eq!(map_issue_state(None), "");
    }

    #[test]
    fn map_mr_status_covers_the_states() {
        assert_eq!(map_mr_status(Some("opened")), PrStatus::Active);
        assert_eq!(map_mr_status(Some("locked")), PrStatus::Active);
        assert_eq!(map_mr_status(Some("merged")), PrStatus::Completed);
        assert_eq!(map_mr_status(Some("closed")), PrStatus::Abandoned);
        assert_eq!(map_mr_status(Some("weird")), PrStatus::Unknown);
        assert_eq!(map_mr_status(None), PrStatus::Unknown);
    }

    #[test]
    fn map_pipeline_status_covers_the_states() {
        assert_eq!(map_pipeline_status(Some("success")), RunStatus::Succeeded);
        assert_eq!(map_pipeline_status(Some("failed")), RunStatus::Failed);
        assert_eq!(map_pipeline_status(Some("canceled")), RunStatus::Canceled);
        assert_eq!(map_pipeline_status(Some("cancelled")), RunStatus::Canceled);
        assert_eq!(map_pipeline_status(Some("running")), RunStatus::Running);
        assert_eq!(map_pipeline_status(Some("pending")), RunStatus::Running);
        // Skipped/manual aren't mapped -> honestly Unknown.
        assert_eq!(map_pipeline_status(Some("skipped")), RunStatus::Unknown);
        assert_eq!(map_pipeline_status(None), RunStatus::Unknown);
    }

    #[test]
    fn normalise_issue_from_hand_written_open_and_closed() {
        // Open issue with a single assignee.
        let open: GlIssue = serde_json::from_str(
            r#"{
                "iid": 42,
                "title": "Fix the flaky test",
                "description": "It flakes on CI",
                "state": "opened",
                "labels": ["type::bug", "backend"],
                "assignee": { "name": "Ada Lovelace" },
                "assignees": [ { "name": "Ada Lovelace" } ],
                "created_at": "2026-07-01T09:00:00Z",
                "updated_at": "2026-07-20T15:30:00Z",
                "web_url": "https://gitlab.com/acme/app/-/issues/42"
            }"#,
        )
        .unwrap();
        let wi = normalise_issue(open, "Platform");
        assert_eq!(wi.id, 42);
        assert_eq!(wi.provider, "gitlab");
        assert_eq!(wi.team, "Platform");
        assert_eq!(wi.title, "Fix the flaky test");
        assert_eq!(wi.work_item_type, "Issue");
        assert_eq!(wi.state, "Open");
        assert_eq!(wi.tags, vec!["type::bug", "backend"]);
        assert_eq!(wi.assigned_to.as_deref(), Some("Ada Lovelace"));
        assert_eq!(wi.story_points, None);
        assert_eq!(wi.url, "https://gitlab.com/acme/app/-/issues/42");
        assert_eq!(wi.description.as_deref(), Some("It flakes on CI"));

        // Closed issue, unassigned, falls back to the `assignees` array being empty.
        let closed: GlIssue = serde_json::from_str(
            r#"{ "iid": 7, "state": "closed", "closed_at": "2026-07-22T00:00:00Z" }"#,
        )
        .unwrap();
        let wi = normalise_issue(closed, "Platform");
        assert_eq!(wi.state, "Closed");
        assert!(wi.assigned_to.is_none());
        assert!(wi.tags.is_empty());
        assert!(wi.closed_at.is_some());
        // A wholly absent description is None (not an empty string).
        assert!(wi.description.is_none());
    }

    #[test]
    fn normalise_issue_prefers_assignees_when_single_assignee_absent() {
        let raw: GlIssue = serde_json::from_str(
            r#"{ "iid": 9, "state": "opened", "assignee": null,
                 "assignees": [ { "name": "Grace Hopper" } ] }"#,
        )
        .unwrap();
        let wi = normalise_issue(raw, "Team");
        assert_eq!(wi.assigned_to.as_deref(), Some("Grace Hopper"));
    }

    #[test]
    fn normalise_merge_request_from_hand_written() {
        let raw: GlMergeRequest = serde_json::from_str(
            r#"{
                "iid": 100,
                "title": "Add retry to poller",
                "state": "opened",
                "draft": true,
                "author": { "name": "Ada Lovelace" },
                "created_at": "2026-07-28T09:00:00Z",
                "source_branch": "feature/retry",
                "target_branch": "main",
                "reviewers": [ { "name": "A" }, { "name": "B" } ],
                "web_url": "https://gitlab.com/acme/app/-/merge_requests/100"
            }"#,
        )
        .unwrap();
        let pr = normalise_merge_request(raw, "Platform");
        assert_eq!(pr.id, 100);
        assert_eq!(pr.provider, "gitlab");
        assert_eq!(pr.title, "Add retry to poller");
        assert_eq!(pr.status, PrStatus::Active);
        assert!(pr.is_draft);
        assert_eq!(pr.author.as_deref(), Some("Ada Lovelace"));
        assert_eq!(pr.source_branch.as_deref(), Some("feature/retry"));
        assert_eq!(pr.target_branch.as_deref(), Some("main"));
        assert_eq!(pr.reviewer_count, 2);
        assert_eq!(pr.url, "https://gitlab.com/acme/app/-/merge_requests/100");
    }

    #[test]
    fn normalise_pipeline_and_run_from_hand_written() {
        let raw: GlPipeline = serde_json::from_str(
            r#"{
                "id": 555,
                "project_id": 250833,
                "status": "success",
                "ref": "main",
                "started_at": "2026-08-01T10:00:00Z",
                "finished_at": "2026-08-01T10:12:00Z",
                "created_at": "2026-08-01T09:59:00Z",
                "updated_at": "2026-08-01T10:12:30Z",
                "web_url": "https://gitlab.com/acme/app/-/pipelines/555",
                "name": null
            }"#,
        )
        .unwrap();
        let p = normalise_pipeline(
            serde_json::from_str(
                r#"{ "id": 555, "status": "success", "ref": "main",
                     "updated_at": "2026-08-01T10:12:30Z",
                     "web_url": "https://gitlab.com/acme/app/-/pipelines/555" }"#,
            )
            .unwrap(),
            "Platform",
        );
        assert_eq!(p.id, 555);
        assert_eq!(p.name, "main");
        assert_eq!(p.last_run_status, Some(RunStatus::Succeeded));
        assert!(p.last_run_at.is_some());
        assert_eq!(p.url, "https://gitlab.com/acme/app/-/pipelines/555");

        let run = normalise_run(raw, "Platform");
        assert_eq!(run.id, 555);
        assert_eq!(run.pipeline_id, 250833);
        assert_eq!(run.status, RunStatus::Succeeded);
        assert_eq!(run.source_branch.as_deref(), Some("main"));
        assert!(run.started_at.is_some());
        assert!(run.finished_at.is_some());
    }

    #[test]
    fn normalise_run_leaves_running_pipeline_unfinished() {
        // A still-running pipeline (the state of every fixture pipeline) has no
        // finish time even though updated_at is present.
        let raw: GlPipeline = serde_json::from_str(
            r#"{ "id": 1, "status": "running", "ref": "main",
                 "created_at": "2026-08-11T12:00:00Z",
                 "updated_at": "2026-08-11T12:05:00Z" }"#,
        )
        .unwrap();
        let run = normalise_run(raw, "T");
        assert_eq!(run.status, RunStatus::Running);
        assert!(run.started_at.is_some());
        assert!(run.finished_at.is_none());
    }

    // ── Fixture tests ───────────────────────────────────────────────────
    // Captured live from gitlab.com's public `gitlab-org/gitlab-runner` project
    // (v4 API). They pin our deserialisation + normalisation against the REAL
    // response shape, which hand-written JSON can't.

    #[test]
    fn issues_fixture_deserialises_and_normalises() {
        let json = include_str!("fixtures/gitlab/issues.json");
        let raw: Vec<GlIssue> = serde_json::from_str(json).expect("real issues payload");
        assert!(!raw.is_empty());
        let items: Vec<WorkItem> = raw
            .into_iter()
            .map(|i| normalise_issue(i, "Runner"))
            .collect();
        for wi in &items {
            assert!(wi.id > 0);
            assert_eq!(wi.work_item_type, "Issue");
            assert_eq!(wi.provider, "gitlab");
            // Every fixture issue is open and carries labels -> tags.
            assert_eq!(wi.state, "Open");
            assert!(wi.url.starts_with("https://gitlab.com/"));
        }
        // The first captured issue maps its iid (matches the web URL), labels, and
        // has no story points (GitLab weight is intentionally not mapped).
        let first = &items[0];
        assert_eq!(first.id, 39683);
        assert!(first.tags.contains(&"type::bug".to_string()));
        assert_eq!(first.story_points, None);
    }

    #[test]
    fn merge_requests_fixture_maps_all_states() {
        let json = include_str!("fixtures/gitlab/merge_requests.json");
        let raw: Vec<GlMergeRequest> =
            serde_json::from_str(json).expect("real merge_requests payload");
        assert!(!raw.is_empty());
        let prs: Vec<PullRequest> = raw
            .into_iter()
            .map(|m| normalise_merge_request(m, "Runner"))
            .collect();
        for pr in &prs {
            assert!(pr.id > 0);
            assert_eq!(pr.provider, "gitlab");
            assert!(pr.url.starts_with("https://gitlab.com/"));
            assert_ne!(pr.status, PrStatus::Unknown);
        }
        // The fixture captured one MR in each lifecycle state - assert the map.
        let by_id = |id: i64| prs.iter().find(|p| p.id == id).unwrap();
        assert_eq!(by_id(7164).status, PrStatus::Completed); // merged
        assert_eq!(by_id(7163).status, PrStatus::Abandoned); // closed
        assert_eq!(by_id(7162).status, PrStatus::Active); // opened
    }

    #[test]
    fn pipelines_fixture_deserialises_and_normalises() {
        let json = include_str!("fixtures/gitlab/pipelines.json");
        let raw: Vec<GlPipeline> = serde_json::from_str(json).expect("real pipelines payload");
        assert!(!raw.is_empty());
        // As pipelines: each surfaces its own last status + a web URL.
        for p in raw
            .iter()
            .map(|p| {
                normalise_pipeline(
                    serde_json::from_value(serde_json::to_value(p_to_json(p)).unwrap()).unwrap(),
                    "Runner",
                )
            })
            .collect::<Vec<_>>()
        {
            assert!(p.id > 0);
            assert!(p.last_run_status.is_some());
            assert!(p.url.starts_with("https://gitlab.com/"));
        }
        // As runs: every fixture pipeline is "running" -> Running, so none carry a
        // finish time, and the source branch comes from `ref`.
        let runs: Vec<PipelineRun> = raw
            .into_iter()
            .map(|p| normalise_run(p, "Runner"))
            .collect();
        assert!(runs.iter().all(|r| r.status == RunStatus::Running));
        assert!(runs.iter().all(|r| r.finished_at.is_none()));
        assert!(runs.iter().all(|r| r.source_branch.is_some()));
        assert_eq!(runs[0].pipeline_id, 250833); // project_id groups the runs
    }

    // Re-serialise a borrowed fixture pipeline so the pipelines test can build a
    // Pipeline without consuming the run vector.
    fn p_to_json(p: &GlPipeline) -> serde_json::Value {
        serde_json::json!({
            "id": p.id,
            "status": p.status,
            "ref": p.ref_name,
            "updated_at": p.updated_at,
            "web_url": p.web_url,
            "name": p.name,
        })
    }

    #[test]
    fn fetch_runs_filters_by_since() {
        // A run created before `since` is dropped; one at/after is kept.
        let old: GlPipeline = serde_json::from_str(
            r#"{ "id": 1, "status": "success", "ref": "main",
                 "created_at": "2026-01-01T00:00:00Z" }"#,
        )
        .unwrap();
        let new: GlPipeline = serde_json::from_str(
            r#"{ "id": 2, "status": "success", "ref": "main",
                 "created_at": "2026-08-01T00:00:00Z" }"#,
        )
        .unwrap();
        let since: DateTime<Utc> = "2026-06-01T00:00:00Z".parse().unwrap();
        let kept: Vec<PipelineRun> = [old, new]
            .into_iter()
            .map(|p| normalise_run(p, "T"))
            .filter(|r| r.started_at.map(|t| t >= since).unwrap_or(true))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, 2);
    }

    // ── Live smoke test (network, off by default) ───────────────────────
    // Hits gitlab.com anonymously against the public runner project. Ignored so
    // CI stays deterministic + offline; run with `--ignored` to exercise it.
    #[tokio::test]
    #[ignore]
    async fn live_anonymous_fetch_is_non_empty() {
        let cfg = TeamConfig {
            name: "Runner".into(),
            provider: poseiden_core::ProviderKind::GitLab,
            organization: "gitlab-org".into(),
            project: "gitlab-runner".into(),
            area_path: None,
            area_path_strict: false,
            tenant: None,
            auth: Default::default(),
            wiql: None,
            pipeline_ids: vec![],
            rules: None,
        };
        // Empty token -> anonymous (public project reads without auth).
        let provider = GitlabProvider::new(&cfg, Credential::Pat(String::new())).unwrap();
        let issues = provider.fetch_work_items().await.expect("issues");
        assert!(!issues.is_empty(), "expected public issues");
        let pipelines = provider.fetch_pipelines().await.expect("pipelines");
        assert!(!pipelines.is_empty(), "expected public pipelines");
    }
}
