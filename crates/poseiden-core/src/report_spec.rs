//! Configurable report definitions - the saveable, runnable query model behind
//! both the Reports screen and the home velocity tiles. A [`ReportSpec`] names
//! one or more [`Series`] (each a datasource + metric + optional grouping +
//! filters) and a [`RenderKind`]; the `poseiden-reports` engine turns a spec +
//! loaded data into a [`ReportResult`]. Kept provider-agnostic and pure data:
//! no logic lives here, only the shapes both the engine and the transports share.

use serde::{Deserialize, Serialize};

/// Which stored entity a series queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataSource {
    WorkItems,
    PullRequests,
    Pipelines,
    PipelineRuns,
}

/// How to bucket a series' rows. `None` (absent) collapses to a single value -
/// the natural shape for a Stat tile. Time buckets drive line/bar-over-time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    Tag,
    State,
    Status,
    Team,
    WorkItemType,
    Day,
    Week,
}

/// What a series measures per bucket.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Metric {
    /// Number of matching rows in the bucket.
    #[default]
    Count,
    /// `|rows matching numerator| / |rows matching denominator|`, 0.0-1.0 (or
    /// `NaN`-safe 0 when the denominator is empty). Powers e.g. pipeline success
    /// rate: numerator = succeeded, denominator = succeeded + failed.
    Ratio {
        numerator: Vec<Condition>,
        denominator: Vec<Condition>,
    },
}

/// Comparison operators for a [`Condition`]. `In` matches any of a comma-
/// separated value list; `Contains` is a case-insensitive substring test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    Ne,
    In,
    Contains,
}

/// A single filter: `field <op> value`. `field` names a column on the series'
/// datasource (e.g. `state`, `status`, `is_draft`, `tag`); the engine maps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub field: String,
    pub op: Op,
    pub value: String,
}

/// One measured stream in a report. Multiple series overlay on the same chart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Series {
    /// Display label; defaults to the source name when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub source: DataSource,
    #[serde(default)]
    pub metric: Metric,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<GroupBy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Condition>,
    /// Which timestamp the report's time window + Day/Week bucketing apply to
    /// (`created` / `closed` / `changed` for work items, `finished` / `started`
    /// for runs). `None` uses the source's primary timestamp. This is what lets a
    /// flow report count items *closed* in a window rather than *created*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_field: Option<String>,
}

/// How the result should be drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderKind {
    Stat,
    Bar,
    Pie,
    Line,
    Table,
    Plaintext,
}

/// Time window a report covers, applied to each source's primary timestamp
/// (work items: created; PRs: created; runs: finished; pipelines: last run).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TimeRange {
    /// No time bound - every stored row.
    #[default]
    AllTime,
    /// Rows whose primary timestamp is within the last N days of `now`.
    LastDays { days: i64 },
    /// Inclusive ISO-8601 (`YYYY-MM-DD` or RFC3339) bounds.
    Between { from: String, to: String },
}

/// A complete, runnable report. `builtin` specs are code-defined templates and
/// are never overwritten - editing one is a "save as" into a new user report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub builtin: bool,
    /// Team scope; `None` = all teams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default)]
    pub time_range: TimeRange,
    pub series: Vec<Series>,
    pub render: RenderKind,
}

// ── Result shapes ──────────────────────────────────────────────────────

/// One (label, value) datum in a result series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub label: String,
    pub value: f64,
}

/// A computed series: its label plus the points (one for a Stat, many for a
/// grouped chart, ordered as the engine emits them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultSeries {
    pub label: String,
    pub points: Vec<Point>,
    /// True when the values are a 0.0-1.0 rate (a Ratio metric), so the UI can
    /// render them as percentages rather than raw counts.
    #[serde(default)]
    pub percent: bool,
}

/// The outcome of running a [`ReportSpec`]: echoes name + render so the UI can
/// draw it without re-fetching the spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportResult {
    pub name: String,
    pub render: RenderKind,
    pub series: Vec<ResultSeries>,
}
