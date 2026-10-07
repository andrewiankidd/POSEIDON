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
    PullRequest, RenderKind, ReportResult, ReportSpec, ReportTable, ResultSeries, RunStatus,
    Series, TableRow, TimeRange, WorkItem,
};

/// The rows a report can query, loaded by the caller. Only the sources a spec
/// references need be populated.
#[derive(Debug, Default, Clone)]
pub struct Datasets {
    pub work_items: Vec<WorkItem>,
    pub pull_requests: Vec<PullRequest>,
    pub pipelines: Vec<PipelineHealth>,
    pub runs: Vec<PipelineRun>,
    /// Team name (case-insensitive) -> identity tokens of the people on that team.
    /// Per-person breakdowns (group by author / reviewer) keep only a PR's people who
    /// are on the roster of the team the PR belongs to; a team with no entry is
    /// unrestricted.
    pub rosters: std::collections::HashMap<String, Vec<String>>,
}

/// Run a spec over the loaded data. `now` anchors relative time ranges (injected
/// rather than read from the clock, so results are deterministic + testable).
pub fn run(spec: &ReportSpec, data: &Datasets, now: DateTime<Utc>) -> ReportResult {
    if spec.render == RenderKind::Items {
        return run_items(spec, data, now);
    }
    let series = spec
        .series
        .iter()
        .map(|s| run_series(s, &spec.time_range, spec.team.as_deref(), data, now))
        .collect();
    ReportResult {
        name: spec.name.clone(),
        render: spec.render,
        series,
        table: None,
    }
}

/// Columns of an item listing (work items).
const ITEM_COLUMNS: [&str; 7] = [
    "ID",
    "Type",
    "State",
    "Created",
    "Created by",
    "Assigned to",
    "Title",
];

/// Upper bound on listed rows, so a pathological window can't produce a payload that
/// stalls the UI.
const MAX_ITEM_ROWS: usize = 10_000;

/// An item-list report: one row per work item in the first series' window and filters,
/// newest first. Honours team rosters like the per-person charts do, so a project-wide
/// feed only lists people who belong to the team, and leaves out build bots.
fn run_items(spec: &ReportSpec, data: &Datasets, now: DateTime<Utc>) -> ReportResult {
    let mut rows: Vec<(DateTime<Utc>, i64, TableRow)> = Vec::new();
    if let Some(series) = spec
        .series
        .first()
        .filter(|s| s.source == DataSource::WorkItems)
    {
        let (from, to) = range_bounds(&spec.time_range, now);
        let time_field = series.time_field.as_deref();
        let rosters = roster_sets(&data.rosters);
        for wi in &data.work_items {
            let in_team = spec.team.as_deref().is_none_or(|t| wi.team == t);
            let in_time = time_field == Some(ANY_TIME)
                || in_window(Queryable::timestamp(wi, time_field), from, to);
            let creator_ok = wi
                .created_by
                .as_deref()
                .is_none_or(|c| !is_service_identity(c))
                && is_member(&rosters, &wi.team, &wi.author_tokens());
            if in_team && in_time && creator_ok && matches_all(wi, &series.filters) {
                rows.push((
                    wi.created_at,
                    wi.id,
                    TableRow {
                        cells: vec![
                            wi.id.to_string(),
                            wi.work_item_type.clone(),
                            wi.state.clone(),
                            wi.created_at.format("%Y-%m-%d").to_string(),
                            wi.created_by.clone().unwrap_or_default(),
                            wi.assigned_to.clone().unwrap_or_default(),
                            wi.title.clone(),
                        ],
                        url: Some(wi.url.clone()).filter(|u| !u.is_empty()),
                    },
                ));
            }
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    rows.truncate(MAX_ITEM_ROWS);
    ReportResult {
        name: spec.name.clone(),
        render: spec.render,
        series: Vec::new(),
        table: Some(ReportTable {
            columns: ITEM_COLUMNS.iter().map(|c| c.to_string()).collect(),
            rows: rows.into_iter().map(|(_, _, r)| r).collect(),
        }),
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
    let rosters = roster_sets(&data.rosters);
    let points = match series.source {
        DataSource::WorkItems => reduce(&data.work_items, series, range, team, &rosters, now),
        DataSource::PullRequests => reduce(&data.pull_requests, series, range, team, &rosters, now),
        DataSource::Pipelines => reduce(&data.pipelines, series, range, team, &rosters, now),
        DataSource::PipelineRuns => reduce(&data.runs, series, range, team, &rosters, now),
    };
    ResultSeries {
        label,
        points,
        percent: matches!(series.metric, Metric::Ratio { .. }),
        // Per-person counts add up to a real total; other groupings can double-count
        // (a tagged row lands in several buckets) and rates/medians never sum.
        summable: matches!(series.metric, Metric::Count)
            && matches!(
                series.group_by,
                Some(GroupBy::Author) | Some(GroupBy::Reviewer)
            ),
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
    /// Lowercase identity tokens of the row's author (display name, sign-in, local
    /// part) - what team-roster matching keys on.
    fn author_tokens(&self) -> Vec<String> {
        Vec::new()
    }
    /// People who reviewed the row (voted on it) as `(display name, identity tokens)`,
    /// for [`GroupBy::Reviewer`].
    fn reviewers(&self) -> Vec<(String, Vec<String>)> {
        Vec::new()
    }
    /// Days from creation to close, for [`Metric::MedianDaysToClose`].
    fn days_to_close(&self) -> Option<f64> {
        None
    }
}

/// Time field meaning "ignore the report window" (e.g. PRs open right now).
const ANY_TIME: &str = "any";

/// Per-team rosters as sets of lowercase tokens (names, sign-ins, local parts), keyed by
/// lowercase team name. A team with an empty roster is left out = unrestricted.
type Rosters = std::collections::HashMap<String, std::collections::HashSet<String>>;

fn roster_sets(rosters: &std::collections::HashMap<String, Vec<String>>) -> Rosters {
    rosters
        .iter()
        .map(|(team, tokens)| {
            let set: std::collections::HashSet<String> = tokens
                .iter()
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect();
            (team.trim().to_lowercase(), set)
        })
        .filter(|(_, set)| !set.is_empty())
        .collect()
}

/// Whether a person on a row of `team` belongs to that team: the team has no roster
/// (unrestricted), or any of their tokens is on it.
fn is_member(rosters: &Rosters, team: &str, tokens: &[String]) -> bool {
    match rosters.get(&team.trim().to_lowercase()) {
        None => true,
        Some(set) => tokens.iter().any(|t| set.contains(t)),
    }
}

/// Whether the row's team has a roster restricting who may be listed.
fn is_restricted(rosters: &Rosters, team: &str) -> bool {
    rosters.contains_key(&team.trim().to_lowercase())
}

/// Build bots and service accounts show up as PR authors / reviewers but aren't
/// people, so per-person breakdowns leave them out.
fn is_service_identity(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("build service")
        || n.contains("project collection")
        || n.contains("[bot]")
        || n.ends_with("-bot")
        || n.ends_with(" bot")
        || n.starts_with("github-actions")
}

fn reduce<T: Queryable>(
    rows: &[T],
    series: &Series,
    range: &TimeRange,
    team: Option<&str>,
    rosters: &Rosters,
    now: DateTime<Utc>,
) -> Vec<Point> {
    let (from, to) = range_bounds(range, now);
    let time_field = series.time_field.as_deref();
    // Rows passing the team scope, time window, and the series' own filters.
    let kept: Vec<&T> = rows
        .iter()
        .filter(|r| team.is_none_or(|t| r.team() == t))
        .filter(|r| time_field == Some(ANY_TIME) || in_window(r.timestamp(time_field), from, to))
        .filter(|r| matches_all(*r, &series.filters))
        .collect();

    // Bucket -> the rows in it (a row with N tags lands in N tag buckets). The
    // index maps a bucket key to its slot so buckets keep first-seen order.
    let mut buckets: Vec<(String, Vec<&T>)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for &row in &kept {
        for key in bucket_keys(row, series.group_by, time_field, rosters) {
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
    rosters: &Rosters,
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
        Some(GroupBy::Title) => vec![row.field("title").unwrap_or_else(none_label)],
        Some(GroupBy::Author) => match row.field("author") {
            Some(a) if is_service_identity(&a) => Vec::new(),
            Some(a) if is_member(rosters, row.team(), &row.author_tokens()) => vec![a],
            Some(_) => Vec::new(),
            None if !is_restricted(rosters, row.team()) => vec![none_label()],
            None => Vec::new(),
        },
        Some(GroupBy::Reviewer) => {
            // Not the author reviewing their own PR (matched by identity, so a display
            // name that differs from the sign-in still counts as the same person).
            let author = row.author_tokens();
            row.reviewers()
                .into_iter()
                .filter(|(name, tokens)| {
                    !is_service_identity(name)
                        && !tokens.iter().any(|t| author.contains(t))
                        && is_member(rosters, row.team(), tokens)
                })
                .map(|(name, _)| name)
                .collect()
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
        Metric::MedianDaysToClose => {
            let mut days: Vec<f64> = rows.iter().filter_map(|r| r.days_to_close()).collect();
            if days.is_empty() {
                return 0.0;
            }
            days.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = days.len() / 2;
            if days.len() % 2 == 1 {
                days[mid]
            } else {
                (days[mid - 1] + days[mid]) / 2.0
            }
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
        // Title lists: alphabetical by label.
        Some(GroupBy::Title) => points.sort_by(|a, b| a.label.cmp(&b.label)),
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
            // The person who created the item - the same "author" a pull request has, so
            // per-person grouping and filters read the same across sources.
            "author" | "created_by" => self.created_by.clone(),
            "title" => Some(if self.title.is_empty() {
                format!("#{}", self.id)
            } else {
                self.title.clone()
            }),
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
    fn author_tokens(&self) -> Vec<String> {
        poseidon_core::identity_tokens(
            self.created_by.as_deref(),
            self.created_by_unique.as_deref(),
        )
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
    fn timestamp(&self, field: Option<&str>) -> Option<DateTime<Utc>> {
        match field {
            Some("closed") => self.closed_at,
            _ => self.created_at,
        }
    }
    fn team(&self) -> &str {
        &self.team
    }
    fn author_tokens(&self) -> Vec<String> {
        poseidon_core::identity_tokens(self.author.as_deref(), self.author_unique.as_deref())
    }
    fn reviewers(&self) -> Vec<(String, Vec<String>)> {
        self.reviewers
            .iter()
            .filter(|r| r.vote != 0)
            .map(|r| {
                (
                    r.name.clone(),
                    poseidon_core::identity_tokens(Some(&r.name), r.unique_name.as_deref()),
                )
            })
            .collect()
    }
    fn days_to_close(&self) -> Option<f64> {
        let (created, closed) = (self.created_at?, self.closed_at?);
        Some(((closed - created).num_seconds().max(0) as f64) / 86_400.0)
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
pub const BUILTIN_WORK_ITEMS_CREATED_7D_LIST: &str = "work-items-created-7d-list";
pub const BUILTIN_WORK_ITEMS_CLOSED_7D_LIST: &str = "work-items-closed-7d-list";
pub const BUILTIN_PR_MERGE_RATE: &str = "pr-merge-rate";
pub const BUILTIN_PR_CONTRIBUTORS: &str = "pr-contributors";
pub const BUILTIN_WORK_ITEMS_BY_CREATOR: &str = "work-items-created-by-person";
pub const BUILTIN_WORK_ITEMS_CREATED_BY_USER: &str = "work-items-created-by-user";

/// The built-in report templates shipped with POSEIDON. The Stat-render specs
/// among them back the home screen's velocity tiles.
pub fn builtins() -> Vec<ReportSpec> {
    vec![
        work_items_by_tag(),
        pipeline_success_rate(),
        work_items_created_7d(),
        work_items_closed_7d(),
        work_items_created_7d_list(),
        work_items_closed_7d_list(),
        pr_merge_rate(),
        pr_contributors(),
        work_items_created_by_person(),
        work_items_created_by_user(),
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

fn work_items_created_7d_list() -> ReportSpec {
    ReportSpec {
        name: BUILTIN_WORK_ITEMS_CREATED_7D_LIST.into(),
        description: Some("Work items created in the last 7 days — individual titles.".into()),
        builtin: true,
        team: None,
        time_range: TimeRange::LastDays { days: 7 },
        series: vec![Series {
            label: None,
            source: DataSource::WorkItems,
            metric: Metric::Count,
            group_by: Some(GroupBy::Title),
            filters: vec![],
            time_field: None,
        }],
        render: RenderKind::List,
    }
}

fn work_items_closed_7d_list() -> ReportSpec {
    ReportSpec {
        name: BUILTIN_WORK_ITEMS_CLOSED_7D_LIST.into(),
        description: Some("Work items closed in the last 7 days — individual titles.".into()),
        builtin: true,
        team: None,
        time_range: TimeRange::LastDays { days: 7 },
        series: vec![Series {
            label: None,
            source: DataSource::WorkItems,
            metric: Metric::Count,
            group_by: Some(GroupBy::Title),
            filters: vec![cond("state", Op::In, "Closed,Resolved,Done,Completed")],
            time_field: Some("closed".into()),
        }],
        render: RenderKind::List,
    }
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

/// Work items created per team member over the last 30 days, as a bar chart.
fn work_items_created_by_person() -> ReportSpec {
    ReportSpec {
        name: BUILTIN_WORK_ITEMS_BY_CREATOR.into(),
        description: Some("Work items created per team member (last 30 days).".into()),
        builtin: true,
        team: None,
        time_range: TimeRange::LastDays { days: 30 },
        series: vec![Series {
            label: Some("Work items created".into()),
            source: DataSource::WorkItems,
            metric: Metric::Count,
            group_by: Some(GroupBy::Author),
            filters: vec![],
            time_field: Some("created".into()),
        }],
        render: RenderKind::Bar,
    }
}

/// Every work item created in the window, as a table with who created each. The report
/// view narrows it to one person; edit the time range for "since <date>".
fn work_items_created_by_user() -> ReportSpec {
    ReportSpec {
        name: BUILTIN_WORK_ITEMS_CREATED_BY_USER.into(),
        description: Some(
            "Work items created by a team member: every item in the window, newest first. \
             Pick a person to narrow it down."
                .into(),
        ),
        builtin: true,
        team: None,
        time_range: TimeRange::LastDays { days: 30 },
        series: vec![Series {
            label: Some("Work items".into()),
            source: DataSource::WorkItems,
            metric: Metric::Count,
            group_by: None,
            filters: vec![],
            time_field: Some("created".into()),
        }],
        render: RenderKind::Items,
    }
}

/// Per-person pull-request activity over the last 30 days: one row per author,
/// one column per measure. Opened is windowed on creation; merged / abandoned /
/// merge rate / time-to-merge on the close date; "open now" ignores the window.
/// Reviews given is windowed on creation like Opened.
fn pr_contributors() -> ReportSpec {
    let by_author =
        |label: &str, metric: Metric, filters: Vec<Condition>, time_field: &str| Series {
            label: Some(label.into()),
            source: DataSource::PullRequests,
            metric,
            group_by: Some(GroupBy::Author),
            filters,
            time_field: Some(time_field.into()),
        };
    ReportSpec {
        name: BUILTIN_PR_CONTRIBUTORS.into(),
        description: Some(
            "Pull requests per person: opened, merged, abandoned, open now, merge rate, \
             median days to merge and reviews given."
                .into(),
        ),
        builtin: true,
        team: None,
        time_range: TimeRange::LastDays { days: 30 },
        series: vec![
            by_author("Opened", Metric::Count, vec![], "created"),
            by_author(
                "Merged",
                Metric::Count,
                vec![cond("status", Op::Eq, "completed")],
                "closed",
            ),
            by_author(
                "Abandoned",
                Metric::Count,
                vec![cond("status", Op::Eq, "abandoned")],
                "closed",
            ),
            by_author(
                "Open now",
                Metric::Count,
                vec![cond("status", Op::Eq, "active")],
                ANY_TIME,
            ),
            by_author(
                "Merge rate",
                Metric::Ratio {
                    numerator: vec![cond("status", Op::Eq, "completed")],
                    denominator: vec![cond("status", Op::In, "completed,abandoned")],
                },
                vec![],
                "closed",
            ),
            by_author(
                "Median days to merge",
                Metric::MedianDaysToClose,
                vec![cond("status", Op::Eq, "completed")],
                "closed",
            ),
            Series {
                label: Some("Reviews given".into()),
                source: DataSource::PullRequests,
                metric: Metric::Count,
                group_by: Some(GroupBy::Reviewer),
                filters: vec![],
                time_field: Some("created".into()),
            },
        ],
        render: RenderKind::Table,
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
            board_column: None,
            board_column_done: None,
            board_lane: None,
            backlog_rank: None,
            created_by: None,
            created_by_unique: None,
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

    /// A work item created `days_ago` days ago by `who` (display name, sign-in).
    fn created_by(id: i64, team: &str, who: Option<(&str, &str)>, days_ago: i64) -> WorkItem {
        let mut w = wi(id, team, &[], days_ago);
        w.created_by = who.map(|(n, _)| n.to_string());
        w.created_by_unique = who.map(|(_, u)| u.to_string());
        w.title = format!("item {id}");
        w
    }

    #[test]
    fn created_by_person_counts_each_team_member_and_skips_bots_and_strangers() {
        let ana = Some(("Ana", "ana@x.com"));
        let ben = Some(("Ben", "ben@x.com"));
        let data = Datasets {
            work_items: vec![
                created_by(1, "A", ana, 1),
                created_by(2, "A", ana, 2),
                created_by(3, "A", ben, 3),
                created_by(4, "A", Some(("Zed", "zed@x.com")), 3), // not on the roster
                created_by(5, "A", Some(("Project Collection Build Service", "svc")), 3),
                created_by(6, "A", ana, 40), // outside the 30-day window
            ],
            rosters: [(
                "A".to_string(),
                vec![
                    "ana@x.com".to_string(),
                    "ben".to_string(),
                    "ben@x.com".to_string(),
                ],
            )]
            .into(),
            ..Default::default()
        };
        let r = run(&work_items_created_by_person(), &data, now());
        assert_eq!(r.render, RenderKind::Bar);
        let s = &r.series[0];
        assert_eq!(value(s, "Ana"), Some(2.0));
        assert_eq!(value(s, "Ben"), Some(1.0));
        assert_eq!(s.points.len(), 2, "no Zed, no build service");
        assert!(s.summable);
    }

    #[test]
    fn created_by_user_lists_items_newest_first_with_links_and_creator() {
        let mut older = created_by(10, "A", Some(("Ana", "ana@x.com")), 5);
        older.url = "https://tracker/10".into();
        let newer = created_by(11, "A", Some(("Ben", "ben@x.com")), 1);
        let far = created_by(12, "A", Some(("Ana", "ana@x.com")), 90);
        let data = Datasets {
            work_items: vec![older, newer, far],
            ..Default::default()
        };
        let r = run(&work_items_created_by_user(), &data, now());
        assert!(r.series.is_empty());
        let t = r.table.expect("an items report fills the table");
        assert_eq!(t.columns.len(), 7);
        let ids: Vec<&str> = t.rows.iter().map(|r| r.cells[0].as_str()).collect();
        assert_eq!(
            ids,
            vec!["11", "10"],
            "newest first; the 90-day-old item is out"
        );
        assert_eq!(t.rows[1].cells[4], "Ana");
        assert_eq!(t.rows[1].url.as_deref(), Some("https://tracker/10"));
    }

    #[test]
    fn created_by_user_honours_a_creator_filter_and_an_explicit_date_range() {
        let ana = Some(("Ana", "ana@x.com"));
        let mut spec = work_items_created_by_user();
        spec.series[0].filters = vec![cond("author", Op::Eq, "Ana")];
        spec.time_range = TimeRange::Between {
            from: "2026-07-01".into(),
            to: "2026-07-31".into(),
        };
        let at = |id, day: u32, who| {
            let mut w = created_by(id, "A", who, 0);
            w.created_at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 7, day, 9, 0, 0).unwrap();
            w
        };
        let data = Datasets {
            work_items: vec![
                at(1, 3, ana),
                at(2, 20, ana),
                at(3, 20, Some(("Ben", "ben@x.com"))), // someone else
                {
                    let mut w = at(4, 1, ana);
                    w.created_at =
                        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 2, 9, 0, 0).unwrap();
                    w // outside the explicit range
                },
            ],
            ..Default::default()
        };
        let t = run(&spec, &data, now()).table.unwrap();
        let ids: Vec<&str> = t.rows.iter().map(|r| r.cells[0].as_str()).collect();
        assert_eq!(ids, vec!["2", "1"]);
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
            author_unique: None,
            created_at: Some(now() - chrono::Duration::days(created_days_ago)),
            source_branch: None,
            target_branch: None,
            closed_at: None,
            reviewer_count: 0,
            reviewers: vec![],
            url: "u".into(),
            flags: vec![],
            linked_work_items: vec![],
        }
    }

    /// A PR by `author`, opened `opened` days ago and (when `closed` is set) closed
    /// that many days ago, reviewed by `reviewers` (each voting approve).
    fn authored(
        id: i64,
        author: &str,
        status: PrStatus,
        opened: i64,
        closed: Option<i64>,
        reviewers: &[&str],
    ) -> PullRequest {
        let mut p = pr(id, "A", status, opened);
        p.author = Some(author.into());
        p.closed_at = closed.map(|d| now() - chrono::Duration::days(d));
        p.reviewers = reviewers
            .iter()
            .map(|n| poseidon_core::PrReviewer {
                name: (*n).into(),
                unique_name: None,
                vote: 10,
            })
            .collect();
        p
    }

    fn value(s: &poseidon_core::ResultSeries, label: &str) -> Option<f64> {
        s.points.iter().find(|p| p.label == label).map(|p| p.value)
    }

    #[test]
    fn pr_contributors_breaks_activity_down_per_person() {
        let data = Datasets {
            pull_requests: vec![
                // Ana: 2 merged (4d and 2d to merge), 1 abandoned, 1 still open.
                authored(1, "Ana", PrStatus::Completed, 10, Some(6), &["Ben"]),
                authored(2, "Ana", PrStatus::Completed, 5, Some(3), &["Ben", "Ana"]),
                authored(3, "Ana", PrStatus::Abandoned, 8, Some(7), &[]),
                authored(4, "Ana", PrStatus::Active, 2, None, &["Ben"]),
                // Ben: 1 merged. Opened 60d ago, merged 2d ago: counts as merged in
                // the window, but was NOT opened in it.
                authored(5, "Ben", PrStatus::Completed, 60, Some(2), &["Ana"]),
                // Outside the 30-day window entirely.
                authored(6, "Ben", PrStatus::Completed, 90, Some(80), &["Ana"]),
                // Build bots are not people.
                authored(
                    7,
                    "Project Collection Build Service",
                    PrStatus::Completed,
                    3,
                    Some(1),
                    &[],
                ),
            ],
            ..Default::default()
        };
        let r = run(&pr_contributors(), &data, now());
        let by = |label: &str| r.series.iter().find(|s| s.label == label).unwrap();

        let opened = by("Opened");
        assert_eq!(value(opened, "Ana"), Some(4.0));
        assert_eq!(
            value(opened, "Ben"),
            None,
            "Ben opened nothing in the window"
        );
        assert!(opened
            .points
            .iter()
            .all(|p| !p.label.contains("Build Service")));
        assert!(opened.summable);

        let merged = by("Merged");
        assert_eq!(value(merged, "Ana"), Some(2.0));
        assert_eq!(
            value(merged, "Ben"),
            Some(1.0),
            "windowed on the close date"
        );

        assert_eq!(value(by("Abandoned"), "Ana"), Some(1.0));
        assert_eq!(value(by("Open now"), "Ana"), Some(1.0));

        let rate = by("Merge rate");
        assert!(rate.percent && !rate.summable);
        assert!((value(rate, "Ana").unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(value(rate, "Ben"), Some(1.0));

        // Ana's merges took 4 and 2 days -> median 3. Ben's took 58.
        let median = by("Median days to merge");
        assert!((value(median, "Ana").unwrap() - 3.0).abs() < 0.01);
        assert!((value(median, "Ben").unwrap() - 58.0).abs() < 0.01);

        // Reviews are credited to the reviewer, never to the author for their own PR,
        // and only for PRs opened in the window.
        let reviews = by("Reviews given");
        assert_eq!(value(reviews, "Ben"), Some(3.0));
        assert_eq!(
            value(reviews, "Ana"),
            None,
            "self-review and out-of-window PRs excluded"
        );
    }

    #[test]
    fn members_limit_the_per_person_breakdown_but_not_other_reports() {
        let mut data = Datasets {
            pull_requests: vec![
                authored(1, "Ana", PrStatus::Completed, 5, Some(3), &["Ben", "Dee"]),
                authored(2, "Ben", PrStatus::Completed, 5, Some(3), &["Ana"]),
                authored(3, "Dee", PrStatus::Completed, 5, Some(3), &["Ana"]),
            ],
            ..Default::default()
        };
        // Team "A" (pr() puts every PR there) is restricted; case + padding don't matter.
        data.rosters
            .insert("a".into(), vec!["ana".into(), " BEN ".into()]);
        let r = run(&pr_contributors(), &data, now());
        for s in &r.series {
            let names: Vec<&str> = s.points.iter().map(|p| p.label.as_str()).collect();
            assert!(!names.contains(&"Dee"), "{}: non-member listed", s.label);
        }
        let opened = r.series.iter().find(|s| s.label == "Opened").unwrap();
        assert_eq!(opened.points.len(), 2);
        let reviews = r
            .series
            .iter()
            .find(|s| s.label == "Reviews given")
            .unwrap();
        assert_eq!(value(reviews, "Ana"), Some(2.0));
        // The team-wide merge rate is untouched by the member list.
        let rate = &run(&pr_merge_rate(), &data, now()).series[0];
        assert_eq!(rate.points[0].value, 1.0);
    }

    #[test]
    fn roster_matches_on_sign_in_when_display_names_differ() {
        // The roster knows them as "Jonathan Reyes"; PRs show "Jon Reyes" - the same
        // sign-in. A stranger sharing nobody's name or sign-in stays out.
        let mut jon = authored(1, "Jon Reyes", PrStatus::Completed, 5, Some(3), &[]);
        jon.author_unique = Some("Jonathan.Reyes@contoso.com".into());
        let mut by_jon = authored(2, "Ben", PrStatus::Completed, 5, Some(3), &[]);
        by_jon.reviewers = vec![poseidon_core::PrReviewer {
            name: "Jon Reyes".into(),
            unique_name: Some("jonathan.reyes@contoso.com".into()),
            vote: 10,
        }];
        let stranger = authored(3, "Dee", PrStatus::Completed, 5, Some(3), &[]);
        let data = Datasets {
            pull_requests: vec![jon, by_jon, stranger],
            // A roster entry expands to name + sign-in + local part.
            rosters: [(
                "A".to_string(),
                poseidon_core::TeamMember {
                    name: "Jonathan Reyes".into(),
                    unique_name: Some("jonathan.reyes@contoso.com".into()),
                }
                .tokens(),
            )]
            .into(),
            ..Default::default()
        };
        let r = run(&pr_contributors(), &data, now());
        let by = |l: &str| r.series.iter().find(|s| s.label == l).unwrap();
        assert_eq!(value(by("Opened"), "Jon Reyes"), Some(1.0));
        assert_eq!(by("Opened").points.len(), 1, "only the rostered person");
        assert_eq!(value(by("Reviews given"), "Jon Reyes"), Some(1.0));
    }

    #[test]
    fn a_roster_only_restricts_its_own_teams_prs() {
        // Team A has a roster (Ana only); team B has none. Dee's PR in B still shows.
        let mut dee = authored(2, "Dee", PrStatus::Completed, 5, Some(3), &[]);
        dee.team = "B".into();
        let data = Datasets {
            pull_requests: vec![
                authored(1, "Ana", PrStatus::Completed, 5, Some(3), &[]),
                authored(3, "Zed", PrStatus::Completed, 5, Some(3), &[]), // team A, not on it
                dee,
            ],
            rosters: [("a".to_string(), vec!["ana".to_string()])].into(),
            ..Default::default()
        };
        let r = run(&pr_contributors(), &data, now());
        let opened = r.series.iter().find(|s| s.label == "Opened").unwrap();
        let names: Vec<&str> = opened.points.iter().map(|p| p.label.as_str()).collect();
        assert!(names.contains(&"Ana") && names.contains(&"Dee"));
        assert!(!names.contains(&"Zed"));
    }

    #[test]
    fn open_now_ignores_the_report_window() {
        let data = Datasets {
            pull_requests: vec![authored(1, "Ana", PrStatus::Active, 120, None, &[])],
            ..Default::default()
        };
        let r = run(&pr_contributors(), &data, now());
        let open = r.series.iter().find(|s| s.label == "Open now").unwrap();
        assert_eq!(
            value(open, "Ana"),
            Some(1.0),
            "a 4-month-old open PR is still open"
        );
        let opened = r.series.iter().find(|s| s.label == "Opened").unwrap();
        assert!(
            opened.points.is_empty(),
            "but it wasn't opened in the window"
        );
    }

    #[test]
    fn reviewers_who_did_not_vote_are_not_counted() {
        let mut p = authored(1, "Ana", PrStatus::Active, 1, None, &[]);
        p.reviewers = vec![
            poseidon_core::PrReviewer {
                name: "Ben".into(),
                unique_name: None,
                vote: 0,
            },
            poseidon_core::PrReviewer {
                name: "Cy".into(),
                unique_name: None,
                vote: -5,
            },
        ];
        let data = Datasets {
            pull_requests: vec![p],
            ..Default::default()
        };
        let r = run(&pr_contributors(), &data, now());
        let reviews = r
            .series
            .iter()
            .find(|s| s.label == "Reviews given")
            .unwrap();
        assert_eq!(value(reviews, "Ben"), None);
        assert_eq!(
            value(reviews, "Cy"),
            Some(1.0),
            "waiting-for-author is still a review"
        );
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
