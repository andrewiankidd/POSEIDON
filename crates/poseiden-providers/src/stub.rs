//! A deterministic, offline [`Provider`] - the same curated data every poll,
//! no network and no credential.
//!
//! Two jobs: **demo mode** (try POSEIDEN with a realistic backlog and zero
//! setup - a team with `provider = "stub"`) and the **end-to-end tests** (a
//! stable dataset flowing through the *real* code paths - normalise -> rules ->
//! store -> API -> UI - so a test can assert exact counts). Only the fetch is
//! canned; everything above the provider is production code.
//!
//! The data is intentionally varied so the UI lights up: some clean items, some
//! flagged (untagged, missing required tag, a disallowed tag), a spread of
//! recently-CLOSED items carrying the `area:` / `source:` / internal-external
//! taxonomy (so Recap and the flow reports have real history), a mix of pipeline
//! outcomes (including a never-run one), and a work-item <-> PR link.
//!
//! Every timestamp is RELATIVE to the day the dataset is built, anchored once at
//! construction (see [`StubProvider::new`]). That keeps the output deterministic
//! within a calendar day - so the e2e equality checks hold - while keeping the
//! data perpetually recent, so recency-based views (Recap's 30/60/90-day window,
//! the created/closed-in-7-days reports) always have something to show, no matter
//! when the demo runs. Fixed literal dates would fall out of range as time passed.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use poseiden_core::{
    Pipeline, PipelineRun, PrStatus, PullRequest, RunStatus, TeamConfig, WorkItem, WorkItemUpdate,
};

use crate::{Provider, ProviderError};

/// Provider slug stamped on every stubbed entity.
const PROVIDER: &str = "stub";

/// A few demo assignees, cycled by id so the Work Items list has variety.
const ASSIGNEES: &[&str] = &["Alex Rivera", "Sam Lee", "Jordan Kim"];

/// See the module docs. Constructed per team; all data is a function of the
/// team's display name (so a multi-team demo stays coherent) and a per-instance
/// date anchor.
pub struct StubProvider {
    team: String,
    /// Start of "today" (UTC) - every timestamp is derived from this so the whole
    /// dataset stays recent and self-consistent. Floored to the day so two
    /// providers built moments apart produce byte-identical data.
    anchor: DateTime<Utc>,
}

impl StubProvider {
    pub fn new(cfg: &TeamConfig) -> Self {
        let anchor = Utc::now()
            .date_naive()
            .and_hms_opt(9, 0, 0)
            .expect("09:00 is a valid time")
            .and_utc();
        Self {
            team: cfg.name.clone(),
            anchor,
        }
    }

    /// A timestamp `d` days before the anchor.
    fn days_ago(&self, d: i64) -> DateTime<Utc> {
        self.anchor - Duration::days(d)
    }

    #[allow(clippy::too_many_arguments)]
    fn wi(
        &self,
        id: i64,
        title: &str,
        ty: &str,
        state: &str,
        tags: &[&str],
        changed_days: i64,
        closed_days: Option<i64>,
        linked: &[i64],
    ) -> WorkItem {
        WorkItem {
            id,
            provider: PROVIDER.into(),
            team: self.team.clone(),
            title: title.into(),
            work_item_type: ty.into(),
            state: state.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            assigned_to: Some(ASSIGNEES[(id as usize) % ASSIGNEES.len()].into()),
            created_at: self.days_ago(180),
            changed_at: self.days_ago(changed_days),
            closed_at: closed_days.map(|d| self.days_ago(d)),
            iteration_path: Some(format!("{}\\Sprint 12", self.team)),
            story_points: Some(3.0),
            url: format!("https://stub.example/{}/_workitems/edit/{id}", self.team),
            description: None,
            linked_pr_ids: linked.to_vec(),
            linked_prs: Vec::new(),
            tag_suggestions: Vec::new(),
        }
    }
}

#[async_trait]
impl Provider for StubProvider {
    fn provider_name(&self) -> &str {
        PROVIDER
    }

    fn team_name(&self) -> &str {
        &self.team
    }

    async fn fetch_work_items(&self) -> Result<Vec<WorkItem>, ProviderError> {
        // 14 items: 6 open (3 tripping hygiene - untagged, missing required
        // tag, a disallowed tag - and 3 clean, one with a linked PR), plus 8
        // recently-closed items tagged with the area:/source:/internal-external
        // taxonomy so Recap has a real month of shipped work to summarise.
        // Closed items are ignored by the hygiene engine, so they never add flags.
        Ok(vec![
            // --- open: the three that trip a rule ---
            self.wi(
                1001,
                "Investigate flaky integration test",
                "Task",
                "New",
                &[],
                2,
                None,
                &[],
            ), // untagged
            self.wi(
                1002,
                "Document the config import format",
                "Task",
                "In Progress",
                &["team:platform", "area:kubernetes"],
                5,
                None,
                &[],
            ), // missing type:*
            self.wi(
                1003,
                "Spike: cache provider responses",
                "User Story",
                "Active",
                &["team:platform", "type:story", "wip"],
                4,
                None,
                &[],
            ), // disallowed 'wip'
            // --- open: clean ---
            self.wi(
                1004,
                "Add retry/backoff to the poller",
                "Bug",
                "Active",
                &["team:platform", "type:bug", "priority:high"],
                1,
                None,
                &[2001],
            ), // linked PR
            self.wi(
                1005,
                "Ship the reports engine",
                "Feature",
                "New",
                &["team:platform", "type:feature", "area:frontend"],
                3,
                None,
                &[],
            ),
            self.wi(
                1006,
                "Wire up the live-reload dev loop",
                "Task",
                "In Progress",
                &["team:platform", "type:task", "area:backend"],
                6,
                None,
                &[],
            ),
            // --- closed in the last ~month: the Recap history ---
            self.wi(
                1007,
                "Migrate config to the database",
                "User Story",
                "Closed",
                &[
                    "team:platform",
                    "type:story",
                    "area:kubernetes",
                    "source:roadmap",
                    "internal",
                ],
                3,
                Some(3),
                &[],
            ),
            self.wi(
                1008,
                "Fix node-pool autoscaling",
                "Bug",
                "Closed",
                &[
                    "team:platform",
                    "type:bug",
                    "area:kubernetes",
                    "source:incident",
                    "internal",
                ],
                6,
                Some(6),
                &[],
            ),
            self.wi(
                1009,
                "Add OpenTelemetry traces to the API",
                "Task",
                "Closed",
                &[
                    "team:platform",
                    "type:task",
                    "area:observability",
                    "source:support",
                    "external",
                ],
                9,
                Some(9),
                &[],
            ),
            self.wi(
                1010,
                "Roll out Argo CD to staging",
                "User Story",
                "Closed",
                &[
                    "team:platform",
                    "type:story",
                    "area:dev-platform",
                    "source:roadmap",
                    "internal",
                ],
                12,
                Some(12),
                &[],
            ),
            self.wi(
                1011,
                "Self-service portal access request",
                "Feature",
                "Closed",
                &[
                    "team:platform",
                    "type:feature",
                    "area:idp",
                    "source:request",
                    "internal",
                ],
                15,
                Some(15),
                &[],
            ),
            self.wi(
                1012,
                "Standardise pipeline YAML templates",
                "Task",
                "Closed",
                &[
                    "team:platform",
                    "type:task",
                    "area:azuredevops",
                    "source:support",
                    "external",
                ],
                20,
                Some(20),
                &[],
            ),
            self.wi(
                1013,
                "Investigate cluster DNS latency",
                "Bug",
                "Closed",
                &[
                    "team:platform",
                    "type:bug",
                    "area:kubernetes",
                    "source:incident",
                    "internal",
                ],
                25,
                Some(25),
                &[],
            ),
            self.wi(
                1014,
                "Dashboards for uptime SLOs",
                "User Story",
                "Closed",
                &[
                    "team:platform",
                    "type:story",
                    "area:observability",
                    "source:roadmap",
                    "external",
                ],
                28,
                Some(28),
                &[],
            ),
        ])
    }

    async fn fetch_pipelines(&self) -> Result<Vec<Pipeline>, ProviderError> {
        let pipe =
            |id: i64, name: &str, status: Option<RunStatus>, at: Option<DateTime<Utc>>| Pipeline {
                id,
                provider: PROVIDER.into(),
                team: self.team.clone(),
                name: name.into(),
                folder: Some("\\platform".into()),
                url: format!(
                    "https://stub.example/{}/_build?definitionId={id}",
                    self.team
                ),
                last_run_status: status,
                last_run_at: at,
                last_run_url: at.map(|_| {
                    format!(
                        "https://stub.example/{}/_build/results?buildId={id}",
                        self.team
                    )
                }),
            };
        Ok(vec![
            pipe(
                10,
                "platform-ci",
                Some(RunStatus::Succeeded),
                Some(self.days_ago(1)),
            ),
            pipe(
                11,
                "platform-nightly",
                Some(RunStatus::Failed),
                Some(self.days_ago(1)),
            ),
            pipe(12, "platform-release", None, None), // never run
        ])
    }

    async fn fetch_runs(&self, _since: DateTime<Utc>) -> Result<Vec<PipelineRun>, ProviderError> {
        // Returned whole; the store/read layer windows by date. Mixed outcomes so
        // the flow report shows a real success rate, all within the last few days.
        let run = |id: i64, pipeline_id: i64, status: RunStatus, at: DateTime<Utc>| PipelineRun {
            id,
            pipeline_id,
            provider: PROVIDER.into(),
            team: self.team.clone(),
            status,
            started_at: Some(at),
            finished_at: Some(at),
            source_branch: Some("refs/heads/main".into()),
            url: format!(
                "https://stub.example/{}/_build/results?buildId={id}",
                self.team
            ),
        };
        Ok(vec![
            run(9001, 10, RunStatus::Succeeded, self.days_ago(1)),
            run(9002, 10, RunStatus::Succeeded, self.days_ago(2)),
            run(9003, 11, RunStatus::Failed, self.days_ago(1)),
            run(9004, 11, RunStatus::Succeeded, self.days_ago(2)),
            run(9005, 10, RunStatus::Canceled, self.days_ago(3)),
        ])
    }

    async fn fetch_pull_requests(&self) -> Result<Vec<PullRequest>, ProviderError> {
        let pr = |id: i64,
                  title: &str,
                  status: PrStatus,
                  draft: bool,
                  author: &str,
                  created: DateTime<Utc>| PullRequest {
            id,
            provider: PROVIDER.into(),
            team: self.team.clone(),
            title: title.into(),
            status,
            is_draft: draft,
            repository: Some("platform-core".into()),
            author: Some(author.into()),
            created_at: Some(created),
            source_branch: Some("refs/heads/feature/x".into()),
            target_branch: Some("refs/heads/main".into()),
            reviewer_count: 2,
            url: format!(
                "https://stub.example/{}/_git/platform-core/pullrequest/{id}",
                self.team
            ),
            flags: Vec::new(),
            linked_work_items: Vec::new(),
        };
        Ok(vec![
            pr(
                2001,
                "Add retry/backoff to the poller",
                PrStatus::Active,
                false,
                "Alex Rivera",
                self.days_ago(1),
            ),
            pr(
                2002,
                "Bump dependencies",
                PrStatus::Active,
                true,
                "Sam Lee",
                self.days_ago(2),
            ),
            pr(
                2003,
                "Fix flaky integration test",
                PrStatus::Completed,
                false,
                "Sam Lee",
                self.days_ago(4),
            ),
        ])
    }

    async fn fetch_pull_request(&self, id: i64) -> Result<PullRequest, ProviderError> {
        self.fetch_pull_requests()
            .await?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| ProviderError::NotFound(format!("pull request {id}")))
    }

    async fn update_work_item(
        &self,
        id: i64,
        update: &WorkItemUpdate,
    ) -> Result<WorkItem, ProviderError> {
        // In-memory edit: apply to the base item and echo it back so the UI edit
        // round-trips. (It reverts on the next poll - stub data is regenerated.)
        let mut item = self
            .fetch_work_items()
            .await?
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| ProviderError::NotFound(format!("work item {id}")))?;
        if let Some(state) = &update.state {
            item.state = state.clone();
        }
        if let Some(tags) = &update.tags {
            item.tags = tags.clone();
        }
        item.changed_at = item.created_at.max(item.changed_at);
        Ok(item)
    }

    async fn link_pr(&self, work_item_id: i64, pr_id: i64) -> Result<WorkItem, ProviderError> {
        let mut item = self
            .fetch_work_items()
            .await?
            .into_iter()
            .find(|w| w.id == work_item_id)
            .ok_or_else(|| ProviderError::NotFound(format!("work item {work_item_id}")))?;
        if !item.linked_pr_ids.contains(&pr_id) {
            item.linked_pr_ids.push(pr_id);
        }
        Ok(item)
    }

    async fn unlink_pr(&self, work_item_id: i64, pr_id: i64) -> Result<WorkItem, ProviderError> {
        let mut item = self
            .fetch_work_items()
            .await?
            .into_iter()
            .find(|w| w.id == work_item_id)
            .ok_or_else(|| ProviderError::NotFound(format!("work item {work_item_id}")))?;
        item.linked_pr_ids.retain(|p| *p != pr_id);
        Ok(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poseiden_core::ProviderKind;

    fn cfg(name: &str) -> TeamConfig {
        TeamConfig {
            name: name.into(),
            provider: ProviderKind::Stub,
            organization: "https://stub.example".into(),
            project: name.into(),
            area_path: None,
            area_path_strict: false,
            tenant: None,
            auth: Default::default(),
            wiql: None,
            pipeline_ids: vec![],
            rules: None,
        }
    }

    // The e2e tests assert exact counts against this dataset - pin them so a
    // careless edit to the demo data is caught here, not in a distant e2e failure.
    #[tokio::test]
    async fn stub_dataset_has_the_expected_shape() {
        let p = StubProvider::new(&cfg("Platform"));
        let items = p.fetch_work_items().await.unwrap();
        assert_eq!(items.len(), 14);
        // 8 of them are closed (the Recap history) and every closed item carries
        // an area: and a source: tag.
        let closed: Vec<_> = items.iter().filter(|w| w.state == "Closed").collect();
        assert_eq!(closed.len(), 8);
        assert!(closed
            .iter()
            .all(|w| w.tags.iter().any(|t| t.starts_with("area:"))
                && w.tags.iter().any(|t| t.starts_with("source:"))));
        assert_eq!(p.fetch_pipelines().await.unwrap().len(), 3);
        assert_eq!(p.fetch_runs(Default::default()).await.unwrap().len(), 5);
        assert_eq!(p.fetch_pull_requests().await.unwrap().len(), 3);
    }

    // Every closed item is dated within the last 30 days (relative to the build),
    // so Recap's default window always has content no matter when the demo runs.
    #[tokio::test]
    async fn stub_closed_items_are_recent() {
        let p = StubProvider::new(&cfg("Platform"));
        let now = Utc::now();
        for w in p
            .fetch_work_items()
            .await
            .unwrap()
            .iter()
            .filter(|w| w.state == "Closed")
        {
            let closed = w.closed_at.expect("closed item has a closed_at");
            let age = (now - closed).num_days();
            assert!(
                (0..=30).contains(&age),
                "closed item {} is {age} days old",
                w.id
            );
        }
    }

    // The deterministic-output contract the e2e relies on: identical input yields
    // byte-for-byte identical entities, both across repeated calls on one provider
    // and across freshly-built providers from the same config (same calendar day).
    #[tokio::test]
    async fn stub_output_is_deterministic_for_the_same_input() {
        let a = StubProvider::new(&cfg("Platform"));
        let b = StubProvider::new(&cfg("Platform"));
        assert_eq!(
            a.fetch_work_items().await.unwrap(),
            a.fetch_work_items().await.unwrap()
        );
        assert_eq!(
            a.fetch_work_items().await.unwrap(),
            b.fetch_work_items().await.unwrap()
        );
        assert_eq!(
            a.fetch_pipelines().await.unwrap(),
            b.fetch_pipelines().await.unwrap()
        );
        assert_eq!(
            a.fetch_pull_requests().await.unwrap(),
            b.fetch_pull_requests().await.unwrap()
        );
    }

    // The data is a function of the team name (so a multi-team demo stays coherent):
    // a different input deterministically produces a different, self-consistent set.
    #[tokio::test]
    async fn stub_stamps_the_configured_team_onto_every_item() {
        let items = StubProvider::new(&cfg("Data Platform"))
            .fetch_work_items()
            .await
            .unwrap();
        assert!(items.iter().all(|w| w.team == "Data Platform"));
        assert!(items.iter().all(|w| w.provider == "stub"));
    }

    // The in-memory edit round-trips through the base dataset (what the UI edit
    // path asserts): the returned item reflects the update but ids are stable.
    #[tokio::test]
    async fn stub_update_work_item_echoes_the_applied_change() {
        let p = StubProvider::new(&cfg("Platform"));
        let update = WorkItemUpdate {
            state: Some("Closed".into()),
            tags: Some(vec!["team:platform".into(), "type:bug".into()]),
        };
        let updated = p.update_work_item(1004, &update).await.unwrap();
        assert_eq!(updated.id, 1004);
        assert_eq!(updated.state, "Closed");
        assert_eq!(updated.tags, vec!["team:platform", "type:bug"]);
        // An unknown id is a NotFound, not a panic.
        assert!(matches!(
            p.update_work_item(999999, &update).await,
            Err(ProviderError::NotFound(_))
        ));
    }
}
