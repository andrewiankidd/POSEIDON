//! The report engine: run a [`ReportSpec`] over loaded [`Datasets`] and get a
//! [`ReportResult`]. Pure functions, no IO - the service loads the rows the spec
//! references and hands them here, so this crate is trivially testable and the
//! same engine backs the Reports screen and the home tiles.
//!
//! A spec is a list of [`Series`]; each series picks a [`DataSource`], filters
//! its rows (by time window + conditions), buckets them by an optional
//! [`GroupBy`], and reduces each bucket to a value via a [`Metric`]. Grouped
//! results are ordered chronologically for time buckets, else by value desc.

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use poseidon_core::{
    Condition, DataSource, GroupBy, Metric, Op, PipelineHealth, PipelineRun, Point, PrStatus,
    PullRequest, RenderKind, ReportResult, ReportSpec, ResultSeries, RunStatus, Series, TimeRange,
    WorkItem,
};

/// The rows a report can query, loaded by the caller. Only the sources a spec
/// references need be populated.
#[derive(Debug, Default, Clone)]
pub struct Datasets {
    pub work_items: Vec<WorkItem>,
    pub pull_requests: Vec<PullRequest>,
    pub pipelines: Vec<PipelineHealth>,
    pub runs: Vec<PipelineRun>,
}

/// Run a spec over the loaded data. `now` anchors relative time ranges (injected
/// rather than read from the clock, so results are deterministic + testable).
pub fn run(spec: &ReportSpec, data: &Datasets, now: DateTime<Utc>) -> ReportResult {
    let series = spec
        .series
        .iter()
        .map(|s| run_series(s, &spec.time_range, spec.team.as_deref(), data, now))
        .collect();
    ReportResult {
        name: spec.name.clone(),
        render: spec.render,
        series,
    }
}

fn run_series(
    series: &Series,
    range: &TimeRange,
    team: Option<&str>,
    data: &Datasets,
    now: DateTime<Utc>,
) -> ResultSeries {
    let label = series
        .label
        .clone()
        .unwrap_or_else(|| default_label(series.source));
    let points = match series.source {
        DataSource::WorkItems => reduce(&data.work_items, series, range, team, now),
        DataSource::PullRequests => reduce(&data.pull_requests, series, range, team, now),
        DataSource::Pipelines => reduce(&data.pipelines, series, range, team, now),
        DataSource::PipelineRuns => reduce(&data.runs, series, range, team, now),
    };
    ResultSeries {
        label,
        points,
        percent: matches!(series.metric, Metric::Ratio { .. }),
    }
}

fn default_label(source: DataSource) -> String {
    match source {
        DataSource::WorkItems => "Work items",
        DataSource::PullRequests => "Pull requests",
        DataSource::Pipelines => "Pipelines",
        DataSource::PipelineRuns => "Pipeline runs",
    }
    .to_string()
}

/// The queryable surface of a source row: a stringified field lookup, its tags,
/// and its primary timestamp (for time filtering + Day/Week bucketing).
trait Queryable {
    fn field(&self, name: &str) -> Option<String>;
    fn tags(&self) -> &[String] {
        &[]
    }
    /// The row's timestamp for a named field (`created` / `closed` / `finished`
    /// …); `None` field selects the source's primary timestamp.
    fn timestamp(&self, field: Option<&str>) -> Option<DateTime<Utc>>;
    fn team(&self) -> &str;
}

fn reduce<T: Queryable>(
    rows: &[T],
    series: &Series,
    range: &TimeRange,
    team: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<Point> {
    let (from, to) = range_bounds(range, now);
    let time_field = series.time_field.as_deref();
    // Rows passing the team scope, time window, and the series' own filters.
    let kept: Vec<&T> = rows
        .iter()
        .filter(|r| team.is_none_or(|t| r.team() == t))
        .filter(|r| in_window(r.timestamp(time_field), from, to))
        .filter(|r| matches_all(*r, &series.filters))
        .collect();

    // Bucket -> the rows in it (a row with N tags lands in N tag buckets). The
    // index maps a bucket key to its slot so buckets keep first-seen order.
    let mut buckets: Vec<(String, Vec<&T>)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for &row in &kept {
        for key in bucket_keys(row, series.group_by, time_field) {
            match index.get(&key) {
                Some(&i) => buckets[i].1.push(row),
                None => {
                    index.insert(key.clone(), buckets.len());
                    buckets.push((key, vec![row]));
                }
            }
        }
    }

    let mut points: Vec<Point> = buckets
        .into_iter()
        .map(|(label, rows)| Point {
            label,
            value: measure(&rows, &series.metric),
        })
        .collect();

    order_points(&mut points, series.group_by);
    points
}

/// The bucket key(s) a row falls into for the given grouping. `None` grouping is
/// a single unlabelled bucket; Tag fans out (untagged -> "(untagged)").
fn bucket_keys<T: Queryable>(
    row: &T,
    group_by: Option<GroupBy>,
    time_field: Option<&str>,
) -> Vec<String> {
    match group_by {
        None => vec![String::new()],
        Some(GroupBy::Tag) => {
            let tags = row.tags();
            if tags.is_empty() {
                vec!["(untagged)".to_string()]
            } else {
                tags.to_vec()
            }
        }
        Some(GroupBy::State) => vec![row.field("state").unwrap_or_else(none_label)],
        Some(GroupBy::Status) => vec![row.field("status").unwrap_or_else(none_label)],
        Some(GroupBy::Team) => vec![row.field("team").unwrap_or_else(none_label)],
        Some(GroupBy::WorkItemType) => {
            vec![row.field("work_item_type").unwrap_or_else(none_label)]
        }
        Some(GroupBy::Day) => vec![row
            .timestamp(time_field)
            .map(|t| t.format("%Y-%m-%d").to_string())
            .unwrap_or_else(none_label)],
        Some(GroupBy::Week) => vec![row
            .timestamp(time_field)
            .map(|t| {
                let iso = t.iso_week();
                format!("{}-W{:02}", iso.year(), iso.week())
            })
            .unwrap_or_else(none_label)],
    }
}

fn none_label() -> String {
    "(none)".to_string()
}

/// Reduce a bucket's rows to a single number per the metric.
fn measure<T: Queryable>(rows: &[&T], metric: &Metric) -> f64 {
    match metric {
        Metric::Count => rows.len() as f64,
        Metric::Ratio {
            numerator,
            denominator,
        } => {
            let denom = rows
                .iter()
                .filter(|r| matches_all(**r, denominator))
                .count();
            if denom == 0 {
                return 0.0;
            }
            let num = rows.iter().filter(|r| matches_all(**r, numerator)).count();
            num as f64 / denom as f64
        }
    }
}

fn matches_all<T: Queryable>(row: &T, conditions: &[Condition]) -> bool {
    conditions.iter().all(|c| matches_one(row, c))
}

fn matches_one<T: Queryable>(row: &T, c: &Condition) -> bool {
    // `tag` is special: test against the tag list rather than a scalar field.
    if c.field == "tag" {
        let has = |v: &str| row.tags().iter().any(|t| t.eq_ignore_ascii_case(v));
        return match c.op {
            Op::Eq | Op::In => c.value.split(',').map(str::trim).any(has),
            Op::Ne => !has(&c.value),
            Op::Contains => row
                .tags()
                .iter()
                .any(|t| t.to_lowercase().contains(&c.value.to_lowercase())),
        };
    }
    let actual = row.field(&c.field).unwrap_or_default();
    let a = actual.to_lowercase();
    let v = c.value.to_lowercase();
    match c.op {
        Op::Eq => a == v,
        Op::Ne => a != v,
        Op::In => c
            .value
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .any(|x| x == a),
        Op::Contains => a.contains(&v),
    }
}

fn order_points(points: &mut [Point], group_by: Option<GroupBy>) {
    match group_by {
        // Time buckets read chronologically (ISO labels sort lexically).
        Some(GroupBy::Day) | Some(GroupBy::Week) => points.sort_by(|a, b| a.label.cmp(&b.label)),
        // Categorical: biggest first, ties broken by label.
        _ => points.sort_by(|a, b| {
            b.value
                .partial_cmp(&a.value)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.label.cmp(&b.label))
        }),
    }
}

// ── Time window ────────────────────────────────────────────────────────

fn range_bounds(
    range: &TimeRange,
    now: DateTime<Utc>,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    match range {
        TimeRange::AllTime => (None, None),
        TimeRange::LastDays { days } => (Some(now - chrono::Duration::days(*days)), Some(now)),
        TimeRange::Between { from, to } => (parse_bound(from, false), parse_bound(to, true)),
    }
}

/// Parse an ISO-8601 bound. Accepts RFC3339 or a bare `YYYY-MM-DD`; a date-only
/// `to` bound extends to the end of that day so the range is inclusive.
fn parse_bound(s: &str, end_of_day: bool) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let date = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()?;
    let time = if end_of_day {
        chrono::NaiveTime::from_hms_opt(23, 59, 59)?
    } else {
        chrono::NaiveTime::from_hms_opt(0, 0, 0)?
    };
    Some(DateTime::from_naive_utc_and_offset(
        date.and_time(time),
        Utc,
    ))
}

fn in_window(
    ts: Option<DateTime<Utc>>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> bool {
    if from.is_none() && to.is_none() {
        return true; // no bound -> keep everything, even rows lacking a timestamp
    }
    let Some(ts) = ts else { return false };
    from.is_none_or(|f| ts >= f) && to.is_none_or(|t| ts <= t)
}

// ── Source field mappings ──────────────────────────────────────────────

fn pr_status_str(s: PrStatus) -> &'static str {
    match s {
        PrStatus::Active => "active",
        PrStatus::Completed => "completed",
        PrStatus::Abandoned => "abandoned",
        PrStatus::Unknown => "unknown",
    }
}

fn run_status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Running => "running",
        RunStatus::Canceled => "canceled",
        RunStatus::Unknown => "unknown",
    }
}

impl Queryable for WorkItem {
    fn field(&self, name: &str) -> Option<String> {
        match name {
            "state" => Some(self.state.clone()),
            "type" | "work_item_type" => Some(self.work_item_type.clone()),
            "team" => Some(self.team.clone()),
            "assigned_to" => self.assigned_to.clone(),
            _ => None,
        }
    }
    fn tags(&self) -> &[String] {
        &self.tags
    }
    fn timestamp(&self, field: Option<&str>) -> Option<DateTime<Utc>> {
        match field {
            Some("closed") => self.closed_at,
            Some("changed") => Some(self.changed_at),
            _ => Some(self.created_at),
        }
    }
    fn team(&self) -> &str {
        &self.team
    }
}

impl Queryable for PullRequest {
    fn field(&self, name: &str) -> Option<String> {
        match name {
            "status" => Some(pr_status_str(self.status).to_string()),
            "is_draft" => Some(self.is_draft.to_string()),
            "team" => Some(self.team.clone()),
            "repository" => self.repository.clone(),
            "author" => self.author.clone(),
            _ => None,
        }
    }
    fn timestamp(&self, _field: Option<&str>) -> Option<DateTime<Utc>> {
        self.created_at
    }
    fn team(&self) -> &str {
        &self.team
    }
}

impl Queryable for PipelineHealth {
    fn field(&self, name: &str) -> Option<String> {
        match name {
            "status" => Some(
                self.last_status
                    .map(run_status_str)
                    .unwrap_or("never_run")
                    .to_string(),
            ),
            "team" => Some(self.team.clone()),
            "name" => Some(self.name.clone()),
            _ => None,
        }
    }
    fn timestamp(&self, _field: Option<&str>) -> Option<DateTime<Utc>> {
        self.last_run_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
    }
    fn team(&self) -> &str {
        &self.team
    }
}

impl Queryable for PipelineRun {
    fn field(&self, name: &str) -> Option<String> {
        match name {
            "status" => Some(run_status_str(self.status).to_string()),
            "team" => Some(self.team.clone()),
            _ => None,
        }
    }
    fn timestamp(&self, field: Option<&str>) -> Option<DateTime<Utc>> {
        match field {
            Some("started") => self.started_at,
            _ => self.finished_at.or(self.started_at),
        }
    }
    fn team(&self) -> &str {
        &self.team
    }
}

// ── Built-in report templates ──────────────────────────────────────────
// Code-defined, read-only. Editing one in the UI is a "save as" into a new user
// report (persisted separately); these templates are never overwritten.

pub const BUILTIN_WORK_ITEMS_BY_TAG: &str = "work-items-by-tag";
pub const BUILTIN_PIPELINE_SUCCESS_RATE: &str = "pipeline-success-rate";
pub const BUILTIN_WORK_ITEMS_CREATED_7D: &str = "work-items-created-7d";
pub const BUILTIN_WORK_ITEMS_CLOSED_7D: &str = "work-items-closed-7d";
pub const BUILTIN_PR_MERGE_RATE: &str = "pr-merge-rate";

/// The built-in report templates shipped with POSEIDON. The last three are the
/// Stat-render specs that back the home screen's velocity tiles.
pub fn builtins() -> Vec<ReportSpec> {
    vec![
        work_items_by_tag(),
        pipeline_success_rate(),
        work_items_created_7d(),
        work_items_closed_7d(),
        pr_merge_rate(),
    ]
}

fn stat_count(
    name: &str,
    desc: &str,
    source: DataSource,
    days: i64,
    filters: Vec<Condition>,
    time_field: Option<&str>,
) -> ReportSpec {
    ReportSpec {
        name: name.into(),
        description: Some(desc.into()),
        builtin: true,
        team: None,
        time_range: TimeRange::LastDays { days },
        series: vec![Series {
            label: None,
            source,
            metric: Metric::Count,
            group_by: None,
            filters,
            time_field: time_field.map(str::to_string),
        }],
        render: RenderKind::Stat,
    }
}

fn work_items_created_7d() -> ReportSpec {
    stat_count(
        BUILTIN_WORK_ITEMS_CREATED_7D,
        "Work items created in the last 7 days.",
        DataSource::WorkItems,
        7,
        vec![],
        None,
    )
}

fn work_items_closed_7d() -> ReportSpec {
    stat_count(
        BUILTIN_WORK_ITEMS_CLOSED_7D,
        "Work items closed in the last 7 days.",
        DataSource::WorkItems,
        7,
        vec![cond("state", Op::In, "Closed,Resolved,Done,Completed")],
        Some("closed"),
    )
}

fn pr_merge_rate() -> ReportSpec {
    ReportSpec {
        name: BUILTIN_PR_MERGE_RATE.into(),
        description: Some("Merged / (merged + abandoned) pull requests.".into()),
        builtin: true,
        team: None,
        time_range: TimeRange::AllTime,
        series: vec![Series {
            label: Some("Merge rate".into()),
            source: DataSource::PullRequests,
            metric: Metric::Ratio {
                numerator: vec![cond("status", Op::Eq, "completed")],
                denominator: vec![cond("status", Op::In, "completed,abandoned")],
            },
            group_by: None,
            filters: vec![],
            time_field: None,
        }],
        render: RenderKind::Stat,
    }
}

fn cond(field: &str, op: Op, value: &str) -> Condition {
    Condition {
        field: field.into(),
        op,
        value: value.into(),
    }
}

fn work_items_by_tag() -> ReportSpec {
    ReportSpec {
        name: BUILTIN_WORK_ITEMS_BY_TAG.into(),
        description: Some("Work items grouped by tag.".into()),
        builtin: true,
        team: None,
        time_range: TimeRange::AllTime,
        series: vec![Series {
            label: Some("Work items".into()),
            source: DataSource::WorkItems,
            metric: Metric::Count,
            group_by: Some(GroupBy::Tag),
            filters: vec![],
            time_field: None,
        }],
        render: RenderKind::Bar,
    }
}

fn pipeline_success_rate() -> ReportSpec {
    ReportSpec {
        name: BUILTIN_PIPELINE_SUCCESS_RATE.into(),
        description: Some("Succeeded / (succeeded + failed) runs over the last 30 days.".into()),
        builtin: true,
        team: None,
        time_range: TimeRange::LastDays { days: 30 },
        series: vec![Series {
            label: Some("Success rate".into()),
            source: DataSource::PipelineRuns,
            metric: Metric::Ratio {
                numerator: vec![cond("status", Op::Eq, "succeeded")],
                denominator: vec![cond("status", Op::In, "succeeded,failed")],
            },
            group_by: None,
            filters: vec![],
            time_field: None,
        }],
        render: RenderKind::Stat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 31, 12, 0, 0).unwrap()
    }

    fn wi(id: i64, team: &str, tags: &[&str], created_days_ago: i64) -> WorkItem {
        WorkItem {
            id,
            provider: "azure-devops".into(),
            team: team.into(),
            title: "t".into(),
            work_item_type: "User Story".into(),
            state: "Active".into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            assigned_to: None,
            created_at: now() - chrono::Duration::days(created_days_ago),
            changed_at: now(),
            closed_at: None,
            iteration_path: None,
            story_points: None,
            url: "u".into(),
            description: None,
            linked_pr_ids: vec![],
            parent_id: None,
            linked_repos: Vec::new(),
            linked_prs: vec![],
            tag_suggestions: vec![],
        }
    }

    fn a_run(id: i64, team: &str, status: RunStatus, finished_days_ago: i64) -> PipelineRun {
        PipelineRun {
            id,
            pipeline_id: 1,
            provider: "azure-devops".into(),
            team: team.into(),
            status,
            started_at: Some(now() - chrono::Duration::days(finished_days_ago)),
            finished_at: Some(now() - chrono::Duration::days(finished_days_ago)),
            source_branch: None,
            url: "u".into(),
        }
    }

    fn point<'p>(series: &'p ResultSeries, label: &str) -> Option<&'p Point> {
        series.points.iter().find(|p| p.label == label)
    }

    #[test]
    fn count_group_by_tag_fans_out_and_orders_desc() {
        let data = Datasets {
            work_items: vec![
                wi(1, "A", &["blocked", "type:bug"], 1),
                wi(2, "A", &["blocked"], 1),
                wi(3, "A", &[], 1), // untagged
            ],
            ..Default::default()
        };
        let result = run(&work_items_by_tag(), &data, now());
        let s = &result.series[0];
        // A row with two tags lands in both buckets; untagged gets its own.
        assert_eq!(point(s, "blocked").unwrap().value, 2.0);
        assert_eq!(point(s, "type:bug").unwrap().value, 1.0);
        assert_eq!(point(s, "(untagged)").unwrap().value, 1.0);
        // Ordered by count desc -> "blocked" leads.
        assert_eq!(s.points[0].label, "blocked");
    }

    #[test]
    fn ratio_metric_computes_success_rate_ignoring_non_terminal() {
        let data = Datasets {
            runs: vec![
                a_run(1, "A", RunStatus::Succeeded, 1),
                a_run(2, "A", RunStatus::Succeeded, 1),
                a_run(3, "A", RunStatus::Succeeded, 1),
                a_run(4, "A", RunStatus::Failed, 1),
                a_run(5, "A", RunStatus::Running, 1), // excluded from num + denom
            ],
            ..Default::default()
        };
        let result = run(&pipeline_success_rate(), &data, now());
        // 3 succeeded / (3 + 1 failed) = 0.75; the Running run is ignored.
        assert!((result.series[0].points[0].value - 0.75).abs() < 1e-9);
    }

    #[test]
    fn time_window_excludes_rows_outside_last_days() {
        let data = Datasets {
            runs: vec![
                a_run(1, "A", RunStatus::Succeeded, 1),
                a_run(2, "A", RunStatus::Failed, 40), // outside the 30-day window
            ],
            ..Default::default()
        };
        let result = run(&pipeline_success_rate(), &data, now());
        assert!((result.series[0].points[0].value - 1.0).abs() < 1e-9);
    }

    #[test]
    fn team_scope_filters_rows() {
        let mut spec = work_items_by_tag();
        spec.team = Some("A".into());
        let data = Datasets {
            work_items: vec![wi(1, "A", &["x"], 1), wi(2, "B", &["x"], 1)],
            ..Default::default()
        };
        let s = &run(&spec, &data, now()).series[0];
        assert_eq!(point(s, "x").unwrap().value, 1.0); // only team A's item
    }

    #[test]
    fn time_field_closed_windows_on_closed_date_not_created() {
        // Created long ago but closed yesterday -> counts for "closed last 7 days".
        let mut item = wi(1, "A", &["x"], 200);
        item.closed_at = Some(now() - chrono::Duration::days(1));
        let spec = ReportSpec {
            name: "closed-7d".into(),
            description: None,
            builtin: false,
            team: None,
            time_range: TimeRange::LastDays { days: 7 },
            series: vec![Series {
                label: None,
                source: DataSource::WorkItems,
                metric: Metric::Count,
                group_by: None,
                filters: vec![],
                time_field: Some("closed".into()),
            }],
            render: RenderKind::Stat,
        };
        let data = Datasets {
            work_items: vec![item],
            ..Default::default()
        };
        assert_eq!(run(&spec, &data, now()).series[0].points[0].value, 1.0);
    }

    #[test]
    fn filter_condition_narrows_rows() {
        // Count only Failed runs via an explicit filter.
        let spec = ReportSpec {
            name: "failed".into(),
            description: None,
            builtin: false,
            team: None,
            time_range: TimeRange::AllTime,
            series: vec![Series {
                label: None,
                source: DataSource::PipelineRuns,
                metric: Metric::Count,
                group_by: None,
                filters: vec![cond("status", Op::Eq, "failed")],
                time_field: None,
            }],
            render: RenderKind::Stat,
        };
        let data = Datasets {
            runs: vec![
                a_run(1, "A", RunStatus::Succeeded, 1),
                a_run(2, "A", RunStatus::Failed, 1),
                a_run(3, "A", RunStatus::Failed, 1),
            ],
            ..Default::default()
        };
        assert_eq!(run(&spec, &data, now()).series[0].points[0].value, 2.0);
    }

    fn pr(id: i64, team: &str, status: PrStatus, created_days_ago: i64) -> PullRequest {
        PullRequest {
            id,
            provider: "azure-devops".into(),
            team: team.into(),
            title: "t".into(),
            status,
            is_draft: false,
            repository: Some("repo".into()),
            author: Some("a".into()),
            created_at: Some(now() - chrono::Duration::days(created_days_ago)),
            source_branch: None,
            target_branch: None,
            reviewer_count: 0,
            url: "u".into(),
            flags: vec![],
            linked_work_items: vec![],
        }
    }

    #[test]
    fn builtin_pr_merge_rate_ignores_active_prs() {
        let data = Datasets {
            pull_requests: vec![
                pr(1, "A", PrStatus::Completed, 1),
                pr(2, "A", PrStatus::Completed, 1),
                pr(3, "A", PrStatus::Completed, 1),
                pr(4, "A", PrStatus::Abandoned, 1),
                pr(5, "A", PrStatus::Active, 1), // excluded from num + denom
            ],
            ..Default::default()
        };
        let s = &run(&pr_merge_rate(), &data, now()).series[0];
        assert!(s.percent, "a ratio metric flags the series as a percentage");
        // 3 completed / (3 completed + 1 abandoned) = 0.75; active is ignored.
        assert!((s.points[0].value - 0.75).abs() < 1e-9);
    }

    #[test]
    fn pr_merge_rate_all_active_yields_zero_not_empty() {
        // Rows exist (a bucket forms) but none are completed/abandoned -> denom 0 -> 0.0.
        let data = Datasets {
            pull_requests: vec![
                pr(1, "A", PrStatus::Active, 1),
                pr(2, "A", PrStatus::Active, 1),
            ],
            ..Default::default()
        };
        let s = &run(&pr_merge_rate(), &data, now()).series[0];
        assert_eq!(s.points.len(), 1);
        assert_eq!(s.points[0].value, 0.0);
    }

    #[test]
    fn builtin_work_items_created_7d_counts_recent_only() {
        let data = Datasets {
            work_items: vec![
                wi(1, "A", &[], 1),
                wi(2, "A", &[], 3),
                wi(3, "A", &[], 40), // outside the 7-day window
            ],
            ..Default::default()
        };
        assert_eq!(
            run(&work_items_created_7d(), &data, now()).series[0].points[0].value,
            2.0
        );
    }

    #[test]
    fn builtin_work_items_closed_7d_filters_state_and_windows_on_closed() {
        // Closed yesterday with a Closed state -> counts.
        let mut closed_recent = wi(1, "A", &[], 100);
        closed_recent.state = "Closed".into();
        closed_recent.closed_at = Some(now() - chrono::Duration::days(1));
        // Closed yesterday but still Active -> excluded by the state filter.
        let mut wrong_state = wi(2, "A", &[], 100);
        wrong_state.closed_at = Some(now() - chrono::Duration::days(1));
        // Closed long ago -> outside the 7-day closed window.
        let mut closed_old = wi(3, "A", &[], 100);
        closed_old.state = "Done".into();
        closed_old.closed_at = Some(now() - chrono::Duration::days(30));
        let data = Datasets {
            work_items: vec![closed_recent, wrong_state, closed_old],
            ..Default::default()
        };
        assert_eq!(
            run(&work_items_closed_7d(), &data, now()).series[0].points[0].value,
            1.0,
            "only the recently-closed item in a Closed-family state"
        );
    }

    #[test]
    fn group_by_tag_all_untagged_single_bucket() {
        let data = Datasets {
            work_items: vec![wi(1, "A", &[], 1), wi(2, "A", &[], 1)],
            ..Default::default()
        };
        let s = &run(&work_items_by_tag(), &data, now()).series[0];
        assert_eq!(s.points.len(), 1);
        assert_eq!(s.points[0].label, "(untagged)");
        assert_eq!(s.points[0].value, 2.0);
    }

    #[test]
    fn empty_dataset_yields_no_points_without_panicking() {
        let data = Datasets::default();
        // A Stat (group_by None) over zero rows produces an empty points vec, not a 0.
        assert!(run(&work_items_created_7d(), &data, now()).series[0]
            .points
            .is_empty());
        // A grouped Bar report likewise has no buckets.
        assert!(run(&work_items_by_tag(), &data, now()).series[0]
            .points
            .is_empty());
        // A ratio over no PRs -> no bucket either.
        assert!(run(&pr_merge_rate(), &data, now()).series[0]
            .points
            .is_empty());
    }

    #[test]
    fn filter_excluding_all_rows_yields_empty_points() {
        let data = Datasets {
            runs: vec![
                a_run(1, "A", RunStatus::Succeeded, 1),
                a_run(2, "A", RunStatus::Succeeded, 1),
            ],
            ..Default::default()
        };
        let spec = ReportSpec {
            name: "none-match".into(),
            description: None,
            builtin: false,
            team: None,
            time_range: TimeRange::AllTime,
            series: vec![Series {
                label: None,
                source: DataSource::PipelineRuns,
                metric: Metric::Count,
                group_by: None,
                filters: vec![cond("status", Op::Eq, "failed")],
                time_field: None,
            }],
            render: RenderKind::Stat,
        };
        // No failed runs -> the filter removes every row -> no bucket, empty points.
        assert!(run(&spec, &data, now()).series[0].points.is_empty());
    }
}

#[cfg(test)]
mod ser_probe {
    #[test]
    fn builtins_serialise_to_json_value() {
        // to_value on the internally-tagged Metric/TimeRange enums must succeed
        // (the Tauri command's to_value fallback returns Null on failure, which
        // would crash the frontend's `specs.forEach`).
        let v = serde_json::to_value(super::builtins());
        assert!(v.is_ok(), "to_value failed: {:?}", v.err());
        assert!(v.unwrap().is_array());
    }
}
